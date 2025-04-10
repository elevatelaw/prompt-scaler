//! Asynchronous utilities for use with Tokio.
//!
//! Some of this stuff is frankly Rust magic, but it enables everything else we
//! do. We pay the complexity tax here to establish the async queue-based
//! architecture of everything else we do.
//!
//! Based on previous Rust experience, you should be able to leave this code
//! unchanged for years.

use crate::prelude::*;

pub mod blocking_iter_streams;
pub mod io;

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
