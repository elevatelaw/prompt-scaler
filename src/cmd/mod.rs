//! Command-line entry points.

use clap::Args;
use futures::StreamExt as _;

use crate::{
    async_utils::{BoxedFuture, BoxedStream},
    prelude::*,
};

pub mod chat;
pub mod ocr;
pub mod schema;

/// Common options for subcommands that process data streams.
#[derive(Debug, Clone, Args)]
pub struct StreamOpts {
    /// Max number of requests to process at a time.
    #[clap(short = 'j', long = "jobs", default_value = "8")]
    pub job_count: usize,

    /// Limit processing to the first N records.
    #[clap(long, alias = "take-first")]
    pub limit: Option<usize>,

    /// Offset the start of processing by N records.
    #[clap(long, default_value = "0")]
    pub offset: usize,

    /// Allow reordering of the output stream. May help increase thoughput if
    /// some requests take much longer than others.
    #[clap(long)]
    pub allow_reordering: bool,

    /// What portion of inputs should we allow to fail? Specified as a
    /// number between 0.0 and 1.0.
    #[clap(long, default_value = "0.01")]
    pub allowed_failure_rate: f32,
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
        let input = input.skip(self.offset);
        if let Some(limit) = self.limit {
            input.take(limit).boxed()
        } else {
            input.boxed()
        }
    }

    /// Apply our buffering options to a stream of futures.
    pub fn apply_stream_buffering_opts<T>(
        &self,
        input: BoxedStream<BoxedFuture<Result<T>>>,
    ) -> BoxedStream<Result<T>>
    where
        T: 'static + Send,
    {
        if self.allow_reordering {
            input.buffer_unordered(self.job_count).boxed()
        } else {
            input.buffered(self.job_count).boxed()
        }
    }
}
