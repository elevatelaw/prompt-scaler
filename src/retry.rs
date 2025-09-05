//! Support utilities for [`keen_retry`]'s retry API.

use std::{
    pin::{Pin},
    sync::{Arc, Mutex},
};

use keen_retry::{ExponentialJitter, ResolvedResult, RetryResult};
use reqwest::StatusCode;
use tokio::sync::Mutex as AsyncMutex;

use crate::prelude::*;

pub const DEFAULT_JITTER: ExponentialJitter<anyhow::Error> = ExponentialJitter::FromBackoffRange {
    backoff_range_millis: 1..=30_000,
    re_attempts: 5,
    jitter_ratio: 0.2,
};

/// Retry with exponential backoff and jitter.
///
/// We wrap `keen_retry`'s API in something a bit less general-purpose, but
/// hopefully easier to use.
#[instrument(level = "debug", skip_all)]
pub async fn retry_with_backoff<'fut, Output, Func, Fut>(
    jitter: ExponentialJitter<anyhow::Error>,
    func: Func,
) -> ResolvedResult<(), (), Output, anyhow::Error>
where
    Func: (FnMut() -> Fut)
        + Send
        + Sync,
    Fut: Future<Output = RetryResult<(), (), Output, anyhow::Error>> + Send + 'fut,
{
    // Do our real work, retrying as specified.
    let attempt_number = Arc::new(Mutex::new(0));
    let shared_func = Arc::pin(AsyncMutex::new(func));
    retry_helper(attempt_number.clone(), shared_func.clone())
        .await
        .retry_with_async(move |_| { 
            retry_helper(attempt_number.clone(), shared_func.clone())
        })
        .with_exponential_jitter(|| jitter)
        .await
        .inspect_fatal(|_, fatal_error| {
            error!(
                "FAILED with error {fatal_error:?}"
            )
        })
        .inspect_recovered(|_, _, retry_errors_list| {
            warn!(
                "suceeded after retrying {} times (failed attempts: [{}])",
                retry_errors_list.len(),
                keen_retry::loggable_retry_errors(retry_errors_list)
            )
        })
        .inspect_given_up(|_, retry_errors_list, fatal_error| {
            error!(
                "FAILED after exhausting all {} retrying attempts with error {fatal_error:?}. Previous transient failures: [{}]",
                retry_errors_list.len(),
                keen_retry::loggable_retry_errors(retry_errors_list)
            )
        })
}

/// Helper function to update the attempt number and call a user function.
#[instrument(level = "debug", skip_all, fields(attempt_number = %*attempt_number.lock().expect("lock poisoned")))]
async fn retry_helper<'fut, Output, Func, Fut>(
    attempt_number: Arc<Mutex<usize>>,
    func: Pin<Arc<AsyncMutex<Func>>>,
) -> RetryResult<(), (), Output, anyhow::Error>
where
    Func: (FnMut() -> Fut)
        + Send
        + Sync,
    Fut: Future<Output = RetryResult<(), (), Output, anyhow::Error>> + Send + 'fut,
{
    // Increment our attempt number.
    {
        let mut attempt_number = attempt_number.lock().expect("lock poisoned");
        *attempt_number += 1;
    };

    // Call the user function.
    let mut func = func.lock().await;
    func().await
}

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
            Err(error) if crate::retry::IsKnownTransient::is_known_transient(&error) => {
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

/// Build an [`RetryResult::Transient`] value.
pub(crate) fn retry_result_transient<T, E>(error: E) -> RetryResult<(), (), T, E> {
    RetryResult::Transient { input: (), error }
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
