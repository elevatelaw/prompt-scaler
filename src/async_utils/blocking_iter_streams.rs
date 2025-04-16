//! Wrapper which converts CPU-intensive iterators to async streams.
//!
//! This is Tokio magic that makes other code much simpler. You can understand
//! this program without knowing how this works.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::{FutureExt as _, Stream};

use super::{BoxedFuture, size_hint::decrement_size_hint};
use crate::prelude::*;

/// A [`BlockingIterStream`] can be in one of two states:
///
/// 1. Waiting on a future.
/// 2. Holding the iterator.
/// 3. Done iterating.
enum BlockingIterStreamState<I, T>
where
    I: Iterator<Item = Result<T>> + Send + Unpin + 'static,
    T: Send + 'static,
{
    /// We have an iterator which we can ask for the next value.
    Iter(I),

    /// We are waiting on a future to complete.
    Waiting(BoxedFuture<(Option<Result<T>>, I)>),
}

/// A [`Stream`] wrapping a blocking iterator.
pub struct BlockingIterStream<I, T>
where
    I: Iterator<Item = Result<T>> + Send + Unpin + 'static,
    T: Send + 'static,
{
    size_hint: (usize, Option<usize>),
    state: Option<BlockingIterStreamState<I, T>>,
}

impl<I, T> BlockingIterStream<I, T>
where
    I: Iterator<Item = Result<T>> + Send + Unpin + 'static,
    T: Send + 'static,
{
    /// Create a new [`BlockingIterStream`] from an iterator.
    pub fn new(iter: I) -> Self {
        Self {
            size_hint: iter.size_hint(),
            state: Some(BlockingIterStreamState::Iter(iter)),
        }
    }
}

impl<I, T> Stream for BlockingIterStream<I, T>
where
    I: Iterator<Item = Result<T>> + Send + Unpin + 'static,
    T: Send + 'static,
{
    type Item = I::Item;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        // Extract our state. We _must_ put this back before we return.
        let mut this = self.as_mut();
        let state = this
            .state
            .take()
            .expect("should always have state on entry to BlockingIterStream::poll_next");

        // Either create a new future to wait on, or use the existing one.
        let mut future = match state {
            BlockingIterStreamState::Iter(mut iter) => {
                // Update our size hint before we temporarily give our iterator
                // away, subtracting 1 because we'll be calling `iter.next()`.
                this.size_hint = decrement_size_hint(iter.size_hint());

                // Run `iter.next()` on a background worker thread, to avoid
                // blocking the executor. This takes ownership of `iter`, but
                // we'll need to give it back later. Async Rust is fun!
                spawn_blocking_propagating_panics(move || {
                    let next = iter.next();
                    (next, iter)
                })
                .boxed()
            }
            BlockingIterStreamState::Waiting(future) => future,
        };

        // Poll our future, and replace our state.
        match Pin::new(&mut future).poll(cx) {
            Poll::Ready((next, iter)) => {
                this.state = Some(BlockingIterStreamState::Iter(iter));
                Poll::Ready(next)
            }
            Poll::Pending => {
                this.state = Some(BlockingIterStreamState::Waiting(future));
                Poll::Pending
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self.state {
            // Use a current size_hint if we have one.
            Some(BlockingIterStreamState::Iter(ref iter)) => iter.size_hint(),
            _ => self.size_hint,
        }
    }
}

/// Wrapper around [`tokio::task::spawn_blocking`] that propagates panics from
/// the background task.
pub async fn spawn_blocking_propagating_panics<F, T>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        // Propagate any panics from the blocking task.
        .unwrap()
}
