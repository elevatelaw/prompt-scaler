//! Support utilities for [`keen_retry`]'s retry API.

use keen_retry::RetryResult;
use reqwest::StatusCode;

/// Macro which implements `?`-like behavior for [`RetryResult`].
macro_rules! try_retry_result {
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

/// On error, return a [`RetryResult::Transient`] value.
macro_rules! try_transient {
    ($result:expr) => {
        match $result {
            Ok(value) => value,
            Err(error) => {
                debug!("Potentially transient error: {:?}", error);
                return ::keen_retry::RetryResult::Transient {
                    input: (),
                    error: From::from(error),
                };
            }
        }
    };
}

/// On error, return a [`RetryResult::Fatal`] value.
macro_rules! try_fatal {
    ($result:expr) => {
        match $result {
            Ok(value) => value,
            Err(error) => {
                return ::keen_retry::RetryResult::Fatal {
                    input: (),
                    error: From::from(error),
                };
            }
        }
    };
}

/// On error, return either a [`RetryResult::Transient`] or [`RetryResult::Fatal`]
/// value, depending on the return value of [`IsKnownTransient::is_known_transient`].
macro_rules! try_potentially_transient {
    ($result:expr) => {
        match $result {
            Ok(value) => value,
            Err(error) if IsKnownTransient::is_known_transient(&error) => {
                debug!("Potentially transient error: {:?}", error);
                return ::keen_retry::RetryResult::Transient {
                    input: (),
                    error: From::from(error),
                };
            }
            Err(error) => {
                return ::keen_retry::RetryResult::Fatal {
                    input: (),
                    error: From::from(error),
                };
            }
        }
    };
}

// Here's a trick to export a macro within a crate as if it were a normal
// symbol.
pub(crate) use {try_fatal, try_potentially_transient, try_retry_result, try_transient};

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

impl IsKnownTransient for reqwest::Error {
    fn is_known_transient(&self) -> bool {
        if let Some(status) = self.status() {
            status.is_known_transient()
        } else {
            // Assume all other kinds of HTTP errors are transient. Unfortunately,
            // there are a lot of things that can go wrong, and `reqwest` doesn't
            // expose most of them in sufficient detail to be certain which are
            // transient.
            true
        }
    }
}

impl IsKnownTransient for StatusCode {
    fn is_known_transient(&self) -> bool {
        let transient_failures = [
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ];
        transient_failures.contains(self)
    }
}
