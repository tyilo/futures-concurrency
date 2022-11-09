use super::Merge as MergeTrait;
use crate::stream::IntoStream;
use crate::utils::{self, Fuse, RandomGenerator};

use core::fmt;
use futures_core::Stream;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

/// A stream that merges multiple streams into a single stream.
///
/// This `struct` is created by the [`merge`] method on the [`Merge`] trait. See its
/// documentation for more.
///
/// [`merge`]: trait.Merge.html#method.merge
/// [`Merge`]: trait.Merge.html
#[pin_project::pin_project]
pub struct Merge<S>
where
    S: Stream,
{
    #[pin]
    streams: Vec<Fuse<S>>,
    rng: RandomGenerator,
    readiness: Arc<Mutex<Readiness>>,
    complete: usize,
}

struct Readiness {
    count: usize,
    ready: Vec<bool>, // TODO: Use a bitvector
}

impl Readiness {
    /// Returns the old ready state for this id
    fn set_ready(&mut self, id: usize) -> bool {
        if !self.ready[id] {
            self.count += 1;
            self.ready[id] = true;

            false
        } else {
            true
        }
    }

    /// Returns whether the task id was previously ready
    fn clear_ready(&mut self, id: usize) -> bool {
        if self.ready[id] {
            self.count -= 1;
            self.ready[id] = false;

            true
        } else {
            false
        }
    }

    fn any_ready(&self) -> bool {
        self.count > 0
    }
}

struct StreamWaker {
    id: usize,
    readiness: Arc<Mutex<Readiness>>,
    parent: Waker,
}

impl Wake for StreamWaker {
    fn wake(self: std::sync::Arc<Self>) {
        if !self.readiness.lock().unwrap().set_ready(self.id) {
            self.parent.wake_by_ref()
        }
    }
}

impl<S> Merge<S>
where
    S: Stream,
{
    pub(crate) fn new(streams: Vec<S>) -> Self {
        let streams: Vec<_> = streams.into_iter().map(Fuse::new).collect();
        let count = streams.len();
        Self {
            streams,
            rng: RandomGenerator::new(),
            readiness: Arc::new(Mutex::new(Readiness {
                count,
                ready: vec![true; count],
            })),
            complete: 0,
        }
    }
}

impl<S> fmt::Debug for Merge<S>
where
    S: Stream + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.streams.iter()).finish()
    }
}

// Plan:
//
// 1. Add per-future wakers
// 2. Add a ready bitvec
// 3. In poll_next, only poll the futures whose ready bit is set (remember to clear it)
// 4. ???
// 5. Profit

impl<S> Stream for Merge<S>
where
    S: Stream,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        // Iterate over our streams one-by-one. If a stream yields a value,
        // we exit early. By default we'll return `Poll::Ready(None)`, but
        // this changes if we encounter a `Poll::Pending`.
        let mut index = this.rng.random(this.streams.len() as u32) as usize;

        let mut readiness = this.readiness.lock().unwrap();
        loop {
            if !readiness.any_ready() {
                // Nothing is ready yet
                return Poll::Pending;
            }

            index = (index + 1).wrapping_rem(this.streams.len());

            if !readiness.clear_ready(index) {
                continue;
            }

            // unlock readiness so we don't deadlock when polling
            drop(readiness);

            let stream = utils::get_pin_mut_from_vec(this.streams.as_mut(), index).unwrap();
            let waker = Arc::new(StreamWaker {
                id: index,
                readiness: this.readiness.clone(),
                parent: cx.waker().clone(),
            })
            .into();
            let mut cx = Context::from_waker(&waker);

            match stream.poll_next(&mut cx) {
                Poll::Ready(Some(item)) => {
                    // Mark ourselves as ready again because we need to poll for the next item.
                    this.readiness.lock().unwrap().set_ready(index);
                    return Poll::Ready(Some(item));
                }
                Poll::Ready(None) => {
                    *this.complete += 1;
                    if *this.complete == this.streams.len() {
                        return Poll::Ready(None);
                    }
                }
                Poll::Pending => (),
            }

            // Lock readiness so we can use it again
            readiness = this.readiness.lock().unwrap();
        }
    }
}

impl<S> MergeTrait for Vec<S>
where
    S: IntoStream,
{
    type Item = <Merge<S::IntoStream> as Stream>::Item;
    type Stream = Merge<S::IntoStream>;

    fn merge(self) -> Self::Stream {
        Merge::new(self.into_iter().map(|i| i.into_stream()).collect())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use super::*;
    use futures::channel::mpsc;
    use futures::executor::LocalPool;
    use futures::task::LocalSpawnExt;
    use futures::SinkExt;
    use futures_lite::future::block_on;
    use futures_lite::prelude::*;
    use futures_lite::stream;

    #[test]
    fn merge_vec_4() {
        block_on(async {
            let a = stream::once(1);
            let b = stream::once(2);
            let c = stream::once(3);
            let d = stream::once(4);
            let mut s = vec![a, b, c, d].merge();

            let mut counter = 0;
            while let Some(n) = s.next().await {
                counter += n;
            }
            assert_eq!(counter, 10);
        })
    }

    #[test]
    fn merge_vec_2x2() {
        block_on(async {
            let a = stream::repeat(1).take(2);
            let b = stream::repeat(2).take(2);
            let mut s = vec![a, b].merge();

            let mut counter = 0;
            while let Some(n) = s.next().await {
                counter += n;
            }
            assert_eq!(counter, 6);
        })
    }

    /// This test case uses channels so we'll have streams that return Pending from time to time.
    ///
    /// The purpose of this test is to make sure we have the waking logic working.
    #[test]
    fn merge_channels() {
        use crate::future::join::Join;

        struct LocalChannel<T> {
            queue: VecDeque<T>,
            waker: Option<Waker>,
            closed: bool,
        }

        struct LocalReceiver<T> {
            channel: Rc<RefCell<LocalChannel<T>>>,
        }

        impl<T> Stream for LocalReceiver<T> {
            type Item = T;

            fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                let mut channel = self.channel.borrow_mut();

                match channel.queue.pop_front() {
                    Some(item) => Poll::Ready(Some(item)),
                    None => {
                        if channel.closed {
                            Poll::Ready(None)
                        } else {
                            channel.waker = Some(cx.waker().clone());
                            Poll::Pending
                        }
                    }
                }
            }
        }

        struct LocalSender<T> {
            channel: Rc<RefCell<LocalChannel<T>>>,
        }

        impl<T> LocalSender<T> {
            fn send(&self, item: T) {
                let mut channel = self.channel.borrow_mut();

                channel.queue.push_back(item);

                let _ = channel.waker.take().map(Waker::wake);
            }
        }

        impl<T> Drop for LocalSender<T> {
            fn drop(&mut self) {
                let mut channel = self.channel.borrow_mut();
                channel.closed = true;
                let _ = channel.waker.take().map(Waker::wake);
            }
        }

        fn local_channel<T>() -> (LocalSender<T>, LocalReceiver<T>) {
            let channel = Rc::new(RefCell::new(LocalChannel {
                queue: VecDeque::new(),
                waker: None,
                closed: false,
            }));

            (
                LocalSender {
                    channel: channel.clone(),
                },
                LocalReceiver { channel },
            )
        }

        let mut pool = LocalPool::new();

        let done = Rc::new(RefCell::new(false));
        let done2 = done.clone();

        pool.spawner()
            .spawn_local(async move {
                let (send1, receive1) = local_channel();
                let (send2, receive2) = local_channel();
                let (send3, receive3) = local_channel();

                let (count, ()) = (
                    async {
                        vec![receive1, receive2, receive3]
                            .merge()
                            .fold(0, |a, b| a + b)
                            .await
                    },
                    async {
                        for i in 1..=4 {
                            send1.send(i);
                            send2.send(i);
                            send3.send(i);
                        }
                        drop(send1);
                        drop(send2);
                        drop(send3);
                    },
                )
                    .join()
                    .await;

                assert_eq!(count, 30);

                *done2.borrow_mut() = true;
            })
            .unwrap();

        while !*done.borrow() {
            pool.run_until_stalled()
        }
    }
}
