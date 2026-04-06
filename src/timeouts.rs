//! Reusable timeout primitives.

use std::{error, fmt, pin::Pin, time::Duration};

use futures::FutureExt as _;
use tokio::time;

use crate::{prelude::*, retry::IsKnownTransient};

/// A wrapper error that adds a Timeout variant to any error type.
#[derive(Debug)]
pub enum TimeoutError<E> {
    /// The original error from the wrapped operation.
    Native(E),
    /// The operation timed out.
    Timeout,
}

impl<E> IsKnownTransient for TimeoutError<E>
where
    E: IsKnownTransient,
{
    /// Is this a known transient error?
    fn is_known_transient(&self) -> bool {
        match self {
            TimeoutError::Native(err) => err.is_known_transient(),
            // Runaway LLM responses and some kinds of network timeouts can
            // be retried with hope of a better result.
            TimeoutError::Timeout => true,
        }
    }
}

impl<E> fmt::Display for TimeoutError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TimeoutError::Native(err) => write!(f, "{err}"),
            TimeoutError::Timeout => write!(f, "request timed out"),
        }
    }
}

impl<E> error::Error for TimeoutError<E>
where
    E: error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            TimeoutError::Native(err) => Some(err),
            TimeoutError::Timeout => None,
        }
    }
}

/// Extension methods for `Result<T, TimeoutError<E>>`.
pub trait TimeoutResultExt<T, E> {
    /// Collapse `TimeoutError<E>` back into `E`, using the closure
    /// to produce the error for the `Timeout` case.
    fn flatten_timeout_err(self, f: impl FnOnce() -> E) -> Result<T, E>;

    /// Convert a timeout into an `Ok` value; propagate native errors.
    fn recover_timeout(self, f: impl FnOnce() -> T) -> Result<T, E>;
}

impl<T, E> TimeoutResultExt<T, E> for Result<T, TimeoutError<E>> {
    fn flatten_timeout_err(self, f: impl FnOnce() -> E) -> Result<T, E> {
        match self {
            Ok(v) => Ok(v),
            Err(TimeoutError::Timeout) => Err(f()),
            Err(TimeoutError::Native(e)) => Err(e),
        }
    }

    fn recover_timeout(self, f: impl FnOnce() -> T) -> Result<T, E> {
        match self {
            Ok(v) => Ok(v),
            Err(TimeoutError::Timeout) => Ok(f()),
            Err(TimeoutError::Native(e)) => Err(e),
        }
    }
}

/// Extension trait that adds `.with_timeout(duration)` to any fallible future.
pub trait WithTimeout<'fut, T, E>: Future<Output = Result<T, E>> + Send + 'fut
where
    T: Send + 'static,
    E: Send + 'static,
{
    /// Wrap this future with an optional timeout.
    ///
    /// If `duration` is `Some`, the future will be wrapped with a timeout.
    /// If the future does not complete within the timeout, a
    /// [`TimeoutError::Timeout`] error is returned. If `duration` is `None`,
    /// the future runs without a timeout.
    ///
    /// The `Pin<Box<dyn Future<...>>>` return is needed because the two
    /// branches (with and without timeout) produce different future types, so
    /// we erase the type via boxing.
    fn with_timeout(
        self,
        duration: Option<Duration>,
    ) -> Pin<Box<dyn Future<Output = Result<T, TimeoutError<E>>> + Send + 'fut>>;
}

impl<'fut, T, E, F> WithTimeout<'fut, T, E> for F
where
    F: Future<Output = Result<T, E>> + Send + 'fut,
    T: Send + 'static,
    E: Send + 'static,
{
    fn with_timeout(
        self,
        duration: Option<Duration>,
    ) -> Pin<Box<dyn Future<Output = Result<T, TimeoutError<E>>> + Send + 'fut>> {
        let future = self.map(|r| r.map_err(TimeoutError::Native));
        match duration {
            Some(dur) => time::timeout(dur, future)
                .map(|result| match result {
                    Ok(inner) => inner,
                    Err(_elapsed) => Err(TimeoutError::Timeout),
                })
                .boxed(),
            None => future.boxed(),
        }
    }
}
