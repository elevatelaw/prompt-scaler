//! Support for `size_hint` for streams.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;

/// Decrement a size hint.
///
/// This saturates the lower bound to 0, so it's safe to call this even if the
/// size hint is already 0.
pub fn decrement_size_hint(size_hint: (usize, Option<usize>)) -> (usize, Option<usize>) {
    let (lower, upper) = size_hint;
    let lower = lower.saturating_sub(1);
    let upper = upper.map(|x| x.saturating_sub(1));
    (lower, upper)
}

/// A [`Stream`] with an external size hint, which will be updated
/// as items are consumed.
pub struct SizeHintStream<S> {
    /// The stream to wrap.
    stream: S,

    /// The size hint.
    size_hint: (usize, Option<usize>),
}

impl<S> SizeHintStream<S> {
    /// Create a new [`SizeHintStream`] from a stream and a size hint.
    pub fn new(stream: S, size_hint: (usize, Option<usize>)) -> Self {
        Self { stream, size_hint }
    }
}

impl<S> Stream for SizeHintStream<S>
where
    S: Stream + Send + Unpin + 'static,
    S::Item: Send + Unpin + 'static,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let stream = Pin::new(&mut this.stream);
        match stream.poll_next(cx) {
            Poll::Ready(Some(value)) => {
                this.size_hint = decrement_size_hint(this.size_hint);
                Poll::Ready(Some(value))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.size_hint
    }
}

/// Extension method adding a `with_size_hint` method to [`Stream`].
pub trait WithSizeHintExt: Stream {
    /// Wrap the stream in a [`SizeHintStream`] with the given size hint.
    fn with_size_hint(self, size_hint: (usize, Option<usize>)) -> SizeHintStream<Self>
    where
        Self: Sized,
    {
        SizeHintStream::new(self, size_hint)
    }
}

impl<S> WithSizeHintExt for S where S: Stream {}
