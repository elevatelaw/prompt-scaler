//! Command-line entry points.

use clap::Args;
use futures::StreamExt as _;

use crate::{async_utils::BoxedStream, prelude::*};

pub mod chat;
pub mod ocr;
pub mod schema;

/// Common options for subcommands that process data streams.
#[derive(Debug, Clone, Args)]
pub struct StreamOpts {
    /// Limit processing to the first N records.
    #[clap(long)]
    take_first: Option<usize>,

    /// Max number of requests to process at a time.
    #[clap(short = 'j', long = "jobs", default_value = "8")]
    job_count: usize,

    /// What portion of inputs should we allow to fail? Specified as a
    /// number between 0.0 and 1.0.
    #[clap(long, default_value = "0.01")]
    allowed_failure_rate: f32,
}

impl StreamOpts {
    /// Apply any necessary stream opts to our input stream.
    pub fn apply_stream_input_opts<T>(
        &self,
        input: BoxedStream<Result<T>>,
    ) -> BoxedStream<Result<T>>
    where
        T: 'static,
    {
        if let Some(take_first) = self.take_first {
            input.take(take_first).boxed()
        } else {
            input
        }
    }
}
