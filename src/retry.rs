//! Support utilities for [`keen_retry`]'s retry API.

use core::fmt;

use async_openai::error::OpenAIError;
use keen_retry::RetryResult;
use reqwest::StatusCode;

use crate::prelude::*;

/// Macro which implements `?`-like behavior for [`RetryResult`].
macro_rules! try_with_retry_result {
    ($result:expr) => {
        match $result {
            ::keen_retry::RetryResult::Ok { output, .. } => output,
            ::keen_retry::RetryResult::Transient { input, error } => {
                return ::keen_retry::RetryResult::Transient {
                    input,
                    error: From::from(error),
                };
            }
            ::keen_retry::RetryResult::Fatal { input, error } => {
                return ::keen_retry::RetryResult::Fatal {
                    input,
                    error: From::from(error),
                };
            }
        }
    };
}

// Here's a trick to export a macro within a crate as if it were a normal
// symbol.
pub(crate) use try_with_retry_result;

/// Build an [`RetryResult::Ok`] value.
pub(crate) fn retry_result_ok<T, E>(output: T) -> RetryResult<(), (), T, E> {
    RetryResult::Ok {
        reported_input: (),
        output,
    }
}

/// Build an [`RetryResult::Fatal`] value.
pub(crate) fn retry_result_fatal<T, E>(error: E) -> RetryResult<(), (), T, E> {
    RetryResult::Fatal { input: (), error }
}

/// Convert a [`Result`] into a [`RetryResult`].
pub(crate) trait IntoRetryResult<T, E> {
    /// Convert a [`Result`] into a [`RetryResult::Transient`].
    fn into_transient(self) -> RetryResult<(), (), T, E>;

    /// Convert a [`Result`] into a [`RetryResult::Fatal`].
    fn into_fatal(self) -> RetryResult<(), (), T, E>;

    /// Convert a [`Result`] into an appropriate [`RetryResult`],
    /// depending on the return value of `is_transient`.
    fn into_retry_result<F>(self, is_transient: F) -> RetryResult<(), (), T, E>
    where
        F: FnOnce(&E) -> bool;
}

impl<T, E> IntoRetryResult<T, E> for Result<T, E>
where
    E: fmt::Debug,
{
    fn into_transient(self) -> RetryResult<(), (), T, E> {
        match self {
            Ok(value) => RetryResult::Ok {
                reported_input: (),
                output: value,
            },
            Err(error) => {
                debug!("Potentially transient error: {:?}", error);
                RetryResult::Transient { input: (), error }
            }
        }
    }

    fn into_fatal(self) -> RetryResult<(), (), T, E> {
        match self {
            Ok(value) => RetryResult::Ok {
                reported_input: (),
                output: value,
            },
            Err(error) => RetryResult::Fatal { input: (), error },
        }
    }

    fn into_retry_result<F>(self, is_transient: F) -> RetryResult<(), (), T, E>
    where
        F: FnOnce(&E) -> bool,
    {
        match self {
            Ok(value) => RetryResult::Ok {
                reported_input: (),
                output: value,
            },
            Err(error) if is_transient(&error) => {
                debug!("Potentially transient error: {:?}", error);
                RetryResult::Transient { input: (), error }
            }
            Err(error) => RetryResult::Fatal { input: (), error },
        }
    }
}

/// Is this error a known transient error?
///
/// By default, we assume errors are not transient, until they're been observed
/// in the wild, investigated and determined to be transient. The prevents us
/// from doing large numbers of retries with exponential backoff on errors that
/// will never resolve.
pub trait IsKnownTransient {
    /// Is this error likely to be transient?
    fn is_known_transient(&self) -> bool;
}

impl IsKnownTransient for OpenAIError {
    fn is_known_transient(&self) -> bool {
        match self {
            OpenAIError::Reqwest(error) => error.is_known_transient(),
            _ => false,
        }
    }
}

impl IsKnownTransient for reqwest::Error {
    fn is_known_transient(&self) -> bool {
        if let Some(status) = self.status() {
            let transient_failures = [
                StatusCode::TOO_MANY_REQUESTS,
                StatusCode::BAD_GATEWAY,
                StatusCode::SERVICE_UNAVAILABLE,
                StatusCode::GATEWAY_TIMEOUT,
            ];
            transient_failures.contains(&status)
        } else {
            // Assume all other kinds of HTTP errors are transient. Unfortunately,
            // there are a lot of things that can go wrong, and `reqwest` doesn't
            // expose most of them in sufficient detail to be certain which are
            // transient.
            true
        }
    }
}
