//! Asynchronous utilities for use with Tokio.
//!
//! Some of this stuff is frankly Rust magic, but it enables everything else we
//! do. We pay the complexity tax here to establish the async queue-based
//! architecture of everything else we do.
//!
//! Based on previous Rust experience, you should be able to leave this code
//! unchanged for years.

use std::pin::Pin;

use futures::Stream;
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

/// Report any command failures.
pub fn check_for_command_failure(
    command_name: &str,
    status: std::process::ExitStatus,
) -> Result<()> {
    if status.success() {
        Ok(())
    } else if let Some(exit_code) = status.code() {
        Err(anyhow::anyhow!(
            "{} failed with exit code {}",
            command_name,
            exit_code
        ))
    } else {
        // Not all platforms have exit codes.
        Err(anyhow::anyhow!("{} failed", command_name))
    }
}
