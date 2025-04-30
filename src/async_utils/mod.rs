//! Asynchronous utilities for use with Tokio.
//!
//! Some of this stuff is frankly Rust magic, but it enables everything else we
//! do. We pay the complexity tax here to establish the async queue-based
//! architecture of everything else we do.
//!
//! Based on previous Rust experience, you should be able to leave this code
//! unchanged for years.

use std::{pin::Pin, sync::LazyLock};

use anyhow::anyhow;
use futures::Stream;
use regex::Regex;
use tokio::task::JoinHandle;

use crate::prelude::*;

pub mod blocking_iter_streams;
pub mod io;
pub mod size_hint;

/// A type alias for a boxed future. This is used to make it easier to work with
/// with complex futures.
pub type BoxedFuture<Output> = Pin<Box<dyn Future<Output = Output> + Send>>;

/// A type alias for a boxed stream. This is used to make it easier to work
/// streams that return complex types.
pub type BoxedStream<Item> = Pin<Box<dyn Stream<Item = Item> + Send>>;

/// A handle for one or more background workers. This can be awaited
/// to wait for all workers to complete normally.
pub struct JoinWorker {
    /// The task handle.
    future: BoxedFuture<Result<()>>,
}

impl JoinWorker {
    /// Create a new worker handle from a [`JoinHandle`].
    pub fn from_handle(handle: JoinHandle<Result<()>>) -> Self {
        Self {
            future: Box::pin(async move { handle.await.context("could not join task")? }),
        }
    }

    /// Create a new worker that returns immediately.
    ///
    /// This is useful if there's no actual worker to be joined, but
    /// an interface expects you to return one.
    pub fn noop() -> Self {
        Self {
            future: Box::pin(async { Ok(()) }),
        }
    }

    /// Wait for the worker to complete.
    pub async fn join(self) -> Result<()> {
        self.future.await
    }
}

/// A default error regex for checking command output.
pub static DEFAULT_ERROR_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)error").expect("failed to compile regex"));

/// Report any command failures, and include any error output.
///
/// The output of standard error and standard output will be logged at
/// appropriate levels. And standard error may be optionally checked against a
/// regex to determine if the command failed.
pub fn check_for_command_failure(
    command_name: &str,
    output: &std::process::Output,
    error_regex: Option<&Regex>,
) -> Result<()> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    debug!(
        command_name = command_name,
        output = %stdout,
        "Standard output from command"
    );
    error!(
        command_name = command_name,
        output = %stderr,
        "Standard error from command",
    );

    if output.status.success() {
        if let Some(regex) = error_regex {
            if regex.is_match(&stderr) {
                return Err(anyhow!(
                    "{} printed error output:\n{}",
                    command_name,
                    stderr,
                ));
            }
        }
        Ok(())
    } else if let Some(exit_code) = output.status.code() {
        Err(anyhow!(
            "{} failed with exit code {} and error output:\n{}",
            command_name,
            exit_code,
            stderr,
        ))
    } else {
        Err(anyhow!(
            "{} failed with error output:\n{}",
            command_name,
            stderr,
        ))
    }
}
