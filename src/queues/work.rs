//! Async item processing with backpressure.
//!
//! The key concepts here are borrowed from ["Queues Don't Fix
//! Overload"](https://ferd.ca/queues-don-t-fix-overload.html). In order to
//! prevent overflow, we limit the number of work items that may be "in flight"
//! at any one time, and once that limit is reached, trying to submit more items
//! for processing will block until one of the in-flight items is completed.
//!
//! Note that this is a strictly "in process" queue for meant for
//! closely-related subtasks of a larger task. It does not attempt to handle
//! priorities or starvation, so it is not appropriate for servers handling
//! requests from multiple unrelated clients.
//!
//! Normally, you will want to use [`WorkQueue`] and [`WorkQueueHandle`], which
//! provide a simple interface for submitting work items and waiting for them to
//! finish.
//!
//! The lower-level interface here is [`WorkItem`] and [`WorkItemProcessor`],
//! which are much more agnostic about what's going on. You won't normally need
//! to work with these directly.

use std::sync::{Arc, Mutex};

use futures::{
    FutureExt, SinkExt as _, StreamExt,
    channel::{mpsc, oneshot},
};
use serde::de::DeserializeOwned;

use crate::{
    async_utils::{
        BoxedFuture, BoxedStream, JoinWorker,
        io::{read_jsonl_or_csv, write_output},
    },
    prelude::*,
    ui::Ui,
};

use super::chat::TokenUsage;

/// Trait implemented by input records to a [`WorkItemProcessor`].
pub trait WorkInput: DeserializeOwned + Send + 'static {
    /// Convert from a JSON value to the input type.
    fn from_json(value: Value) -> Result<Self> {
        serde_json::from_value::<Self>(value).context("failed to deserialize input")
    }

    /// Read a stream from a [`Path`] or from standard input.
    async fn read_stream(
        ui: Ui,
        path: Option<&Path>,
    ) -> Result<BoxedStream<Result<Self>>> {
        Ok(read_jsonl_or_csv(ui, path)
            .await?
            .map(|value| Self::from_json(value?))
            .boxed())
    }
}

/// Counters associated with a work item.
#[derive(Clone, Debug, Default)]
pub struct WorkItemCounters {
    /// How many records did we process?
    pub total_record_count: usize,

    /// How many records did we fail to process?
    pub failure_count: usize,

    /// How many non-fatal errors did we encounter?
    pub non_fatal_error_count: usize,

    /// How much money do we think we spent?
    pub cost_estimate: f64,

    /// How many tokens did we use?
    pub token_usage: TokenUsage,
}

/// Trait implemented by output records from a [`WorkItemProcessor`].
pub trait WorkOutput: Sized + Serialize + Send + 'static {
    /// Should we count this output as a failure?
    fn is_failure(&self) -> bool;

    /// How much money do we think we spent?
    fn cost_estimate(&self) -> Option<f64>;

    /// How many tokens did we use?
    fn token_usage(&self) -> Option<&TokenUsage>;

    /// Get any errors that occurred during processing.
    fn errors(&self) -> &[String];

    /// Convert from the output type to a JSON value.
    fn into_json(self) -> Result<Value> {
        serde_json::to_value::<Self>(self).context("failed to serialize output")
    }

    /// Write a stream of outputs to a [`Path`] or to standard output.
    async fn write_stream(
        ui: &Ui,
        path: Option<&Path>,
        stream: BoxedStream<Result<Self>>,
        allowed_failure_rate: f32,
    ) -> Result<()> {
        let counters = Arc::new(Mutex::new(WorkItemCounters::default()));

        let counters_clone = counters.clone();
        let output = stream
            .map(move |value| {
                let value = value?;

                // Hold a sync lock, but just for an instant to update counters.
                let mut counters = counters_clone.lock().expect("lock poisoned");
                counters.total_record_count += 1;
                if value.is_failure() {
                    counters.failure_count += 1;
                } else if !value.errors().is_empty() {
                    counters.non_fatal_error_count += value.errors().len();
                }
                if let Some(cost) = value.cost_estimate() {
                    counters.cost_estimate += cost;
                }
                if let Some(token_usage) = value.token_usage() {
                    counters.token_usage += token_usage.clone();
                }
                drop(counters);

                // Convert the value to JSON.
                value.into_json()
            })
            .boxed();
        write_output(path, output).await?;

        let counters = (*counters.lock().expect("lock poisoned")).to_owned();
        if !counters.token_usage.is_zero() {
            ui.display_message(
                "📈",
                &format!(
                    "{} input tokens and {} output tokens used",
                    counters.token_usage.prompt_tokens,
                    counters.token_usage.completion_tokens,
                ),
            );
        }
        if counters.cost_estimate > 0.0 {
            ui.display_message(
                "💸",
                &format!("Estimated cost: US${:.8}", counters.cost_estimate),
            );
        }
        let failure_rate =
            counters.failure_count as f32 / counters.total_record_count as f32;
        if failure_rate > allowed_failure_rate {
            Err(anyhow::anyhow!(
                "{}/{} ({:.2}%) of outputs were failures, but only {:.2}% were allowed",
                counters.failure_count,
                counters.total_record_count,
                failure_rate * 100.0,
                allowed_failure_rate * 100.0
            ))
        } else {
            if counters.non_fatal_error_count > 0 {
                ui.display_message(
                    "⚠️",
                    &format!(
                        "{} non-fatal errors encountered",
                        counters.non_fatal_error_count
                    ),
                );
            }
            if counters.failure_count > 0 {
                ui.display_message(
                    "❌",
                    &format!("{} records could not be processed", counters.failure_count),
                );
            }
            Ok(())
        }
    }
}

/// Work items are processed by [`WorkItemProcessor`]s. They contain an input,
/// and a one-shot channel on which to return the result.
#[derive(Debug)]
pub struct WorkItem<Input, Output> {
    /// The input to the work item.
    pub input: Input,

    /// The one-shot channel on which to return the result.
    pub tx: oneshot::Sender<Result<Output>>,
}

/// API shared by workers.
///
/// This is fairly bare bones; you'll probably want to use [`WorkQueue`] and
/// [`WorkQueueHandle`] in normal usage.
pub trait WorkItemProcessor {
    type Input: WorkInput;
    type Output: WorkOutput;

    /// Process a work item. The result will be sent to `item.tx`.
    ///
    /// This should normally only block if our processing capacity has been
    /// maxed out.
    async fn submit_work_item(
        &self,
        item: WorkItem<Self::Input, Self::Output>,
    ) -> Result<()>;

    /// Process an input and return a channel that will receive the output.
    ///
    /// This should normally only block if our processing capacity has been
    /// maxed out.
    async fn submit_input(
        &self,
        input: Self::Input,
    ) -> Result<oneshot::Receiver<Result<Self::Output>>> {
        let (tx, rx) = oneshot::channel();
        let item = WorkItem { input, tx };
        self.submit_work_item(item).await?;
        Ok(rx)
    }

    /// Process an input and wait for the output.
    async fn process_blocking(&self, input: Self::Input) -> Result<Self::Output> {
        let rx = self.submit_input(input).await?;
        let result = rx.await.context("failed to receive work item result");
        match result {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(err)) => Err(err),
            Err(err) => Err(err),
        }
    }
}

/// An async work function.
type WorkFn<Input, Output> =
    Arc<dyn Fn(Input) -> BoxedFuture<Result<Output>> + Send + Sync + 'static>;

/// A handle to a [`WorkQueue`].
///
/// This is basically just a wrapper around a [`mpsc::Sender`] that implements
/// [`WorkItemProcessor`]. It can be cloned cheaply and passed around.
pub struct WorkQueueHandle<Input, Output> {
    /// Our sender.
    tx: mpsc::Sender<WorkItem<Input, Output>>,
}

impl<Input, Output> WorkQueueHandle<Input, Output>
where
    Input: WorkInput,
    Output: WorkOutput,
{
    /// Process a stream of inputs, returning a stream of futures that will
    /// yield outputs. Typically used with [`futures::StreamExt::buffered`] or
    /// [`futures::StreamExt::buffer_unordered`] to resolve the futures,
    /// yielding a stream of outputs.
    ///
    /// You can use pretty much whatever concurrency you find appropriate when
    /// calling `buffered` or `buffer_unordered`, but the underlying concurrency
    /// limit on the [`WorkQueue`] will still be enforced normally.
    pub async fn process_stream(
        &self,
        input: BoxedStream<Result<Input>>,
    ) -> BoxedStream<BoxedFuture<Result<Output>>> {
        let handle = self.clone();
        input
            .map(move |input| {
                let handle = handle.clone();
                async move {
                    let input = input?;
                    handle.process_blocking(input).await
                }
                .boxed()
            })
            .boxed()
    }
}

// Override `Clone` so that `Input` and `Output` are not required to be `Clone`.
impl<Input, Output> Clone for WorkQueueHandle<Input, Output> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl<Input, Output> WorkItemProcessor for WorkQueueHandle<Input, Output>
where
    Input: WorkInput,
    Output: WorkOutput,
{
    type Input = Input;
    type Output = Output;

    async fn submit_work_item(
        &self,
        item: WorkItem<Self::Input, Self::Output>,
    ) -> Result<()> {
        // We need a mutable copy of `tx` to send the item, so we clone it here.
        let mut tx = self.tx.clone();
        tx.send(item).await.context("failed to send work item")?;
        Ok(())
    }
}

/// A [`WorkItemProcessor`] that maintains a queue of work items and processes them in parallel.
///
/// We maintain backpressure by limiting the number of work items queued, and the number currently
/// being processed.
pub struct WorkQueue<Input, Output> {
    /// Queue for submitting work items.
    tx: mpsc::Sender<WorkItem<Input, Output>>,
}

impl<Input, Output> WorkQueue<Input, Output>
where
    Input: Send + 'static,
    Output: Send + 'static,
{
    /// Create a new work queue with the given concurrency limit.
    ///
    /// Note that up to `concurrency_limit` work may be waiting at any one time, and another
    /// `concurrency_limit` work items may be in progress. This means that the total number
    /// of work items in the system at any time may be up to `2 * concurrency_limit`.
    pub fn new(
        concurrency_limit: usize,
        work_fn: WorkFn<Input, Output>,
    ) -> Result<(Self, JoinWorker)> {
        let (tx, rx) = mpsc::channel(concurrency_limit);
        let worker = tokio::spawn(async move {
            rx.for_each_concurrent(
                concurrency_limit,
                |item: WorkItem<Input, Output>| async {
                    let result = work_fn(item.input).await;
                    if let Err(_sent_value) = item.tx.send(result) {
                        debug!(
                            "failed to send work item result because receiver was dropped"
                        );
                    }
                },
            )
            .await;
            Ok(())
        });
        Ok((Self { tx }, JoinWorker::from_handle(worker)))
    }

    /// Get a handle for submitting items to the work queue.
    pub fn handle(&self) -> WorkQueueHandle<Input, Output> {
        WorkQueueHandle {
            tx: self.tx.clone(),
        }
    }
}
