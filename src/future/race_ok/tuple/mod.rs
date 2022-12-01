use super::RaceOk;
use crate::utils;

use core::array;
use core::fmt;
use core::future::{Future, IntoFuture};
use core::mem::{self, MaybeUninit};
use core::pin::Pin;
use core::task::{Context, Poll};

use pin_project::pin_project;

mod error;
pub(crate) use error::AggregateError;

macro_rules! impl_race_ok_tuple {
    ($StructName:ident $($F:ident)+) => {
        /// Wait for the first successful future to complete.
        ///
        /// This `struct` is created by the [`race_ok`] method on the [`RaceOk`] trait. See
        /// its documentation for more.
        ///
        /// [`race_ok`]: crate::future::RaceOk::race_ok
        /// [`RaceOk`]: crate::future::RaceOk
        #[must_use = "futures do nothing unless you `.await` or poll them"]
        #[allow(non_snake_case)]
        #[pin_project]
        pub struct $StructName<T, ERR, $($F),*>
        where
            $( $F: Future<Output = Result<T, ERR>>, )*
            ERR: fmt::Debug,
        {
            completed: usize,
            done: bool,
            indexer: utils::Indexer,
            errors: [MaybeUninit<ERR>; {utils::tuple_len!($($F,)*)}],
            $(#[pin] $F: $F,)*
        }

        impl<T, ERR, $($F),*> fmt::Debug for $StructName<T, ERR, $($F),*>
        where
            $( $F: Future<Output = Result<T, ERR>> + fmt::Debug, )*
            T: fmt::Debug,
            ERR: fmt::Debug,
        {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple("Race")
                    $(.field(&self.$F))*
                    .finish()
            }
        }

        impl<T, ERR, $($F),*> RaceOk for ($($F,)*)
        where
            $( $F: IntoFuture<Output = Result<T, ERR>>, )*
            ERR: fmt::Debug,
        {
            type Output = T;
            type Error = AggregateError<ERR, {utils::tuple_len!($($F,)*)}>;
            type Future = $StructName<T, ERR, $($F::IntoFuture),*>;

            fn race_ok(self) -> Self::Future {
                let ($($F,)*): ($($F,)*) = self;
                $StructName {
                    completed: 0,
                    done: false,
                    indexer: utils::Indexer::new(utils::tuple_len!($($F,)*)),
                    errors: array::from_fn(|_| MaybeUninit::uninit()),
                    $($F: $F.into_future()),*
                }
            }
        }

        impl<T, ERR, $($F: Future),*> Future for $StructName<T, ERR, $($F),*>
        where
            $( $F: Future<Output = Result<T, ERR>>, )*
            ERR: fmt::Debug,
        {
            type Output = Result<T, AggregateError<ERR, {utils::tuple_len!($($F,)*)}>>;

            fn poll(
                self: Pin<&mut Self>, cx: &mut Context<'_>
            ) -> Poll<Self::Output> {
                const LEN: usize = utils::tuple_len!($($F,)*);

                let mut this = self.project();

                let can_poll = !*this.done;
                assert!(can_poll, "Futures must not be polled after completing");

                #[repr(usize)]
                enum Indexes {
                    $($F),*
                }

                for i in this.indexer.iter() {
                    utils::gen_conditions!(i, this, cx, poll, $((Indexes::$F as usize; $F, {
                        Poll::Ready(output) => match output {
                            Ok(output) => {
                                *this.done = true;
                                *this.completed += 1;
                                return Poll::Ready(Ok(output));
                            },
                            Err(err) => {
                                this.errors[i] = MaybeUninit::new(err);
                                *this.completed += 1;
                                continue;
                            },
                        },
                        _ => continue,
                    }))*);
                }

                let all_completed = *this.completed == LEN;
                if all_completed {
                    let mut errors = array::from_fn(|_| MaybeUninit::uninit());
                    mem::swap(&mut errors, this.errors);

                    let result = unsafe { utils::array_assume_init(errors) };

                    *this.done = true;
                    return Poll::Ready(Err(AggregateError::new(result)));
                }

                Poll::Pending
            }
        }
    };
}

impl_race_ok_tuple! { RaceOk1 A }
impl_race_ok_tuple! { RaceOk2 A B }
impl_race_ok_tuple! { RaceOk3 A B C }
impl_race_ok_tuple! { RaceOk4 A B C D }
impl_race_ok_tuple! { RaceOk5 A B C D E }
impl_race_ok_tuple! { RaceOk6 A B C D E F }
impl_race_ok_tuple! { RaceOk7 A B C D E F G }
impl_race_ok_tuple! { RaceOk8 A B C D E F G H }
impl_race_ok_tuple! { RaceOk9 A B C D E F G H I }
impl_race_ok_tuple! { RaceOk10 A B C D E F G H I J }
impl_race_ok_tuple! { RaceOk11 A B C D E F G H I J K }
impl_race_ok_tuple! { RaceOk12 A B C D E F G H I J K L }

#[cfg(test)]
mod test {
    use super::*;
    use core::future;
    use std::error::Error;

    type DynError = Box<dyn Error>;

    #[test]
    fn race_ok_1() {
        futures_lite::future::block_on(async {
            let a = async { Ok::<_, DynError>("world") };
            let res = (a,).race_ok().await;
            assert!(matches!(res, Ok("world")));
        });
    }

    #[test]
    fn race_ok_2() {
        futures_lite::future::block_on(async {
            let a = future::pending();
            let b = async { Ok::<_, DynError>("world") };
            let res = (a, b).race_ok().await;
            assert!(matches!(res, Ok("world")));
        });
    }

    #[test]
    fn race_ok_3() {
        futures_lite::future::block_on(async {
            let a = future::pending();
            let b = async { Ok::<_, DynError>("hello") };
            let c = async { Ok::<_, DynError>("world") };
            let result = (a, b, c).race_ok().await;
            assert!(matches!(result, Ok("hello") | Ok("world")));
        });
    }

    #[test]
    fn race_ok_err() {
        futures_lite::future::block_on(async {
            let a = async { Err::<(), _>("hello") };
            let b = async { Err::<(), _>("world") };
            let errors = (a, b).race_ok().await.unwrap_err();
            assert_eq!(errors[0], "hello");
            assert_eq!(errors[1], "world");
        });
    }
}
