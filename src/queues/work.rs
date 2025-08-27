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
use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use crate::{
    async_utils::{
        BoxedFuture, BoxedStream, JoinWorker,
        io::{read_jsonl_or_csv, write_output},
    },
    cmd::StreamOpts,
    drivers::TokenUsage,
    prelude::*,
    ui::Ui,
};

/// Input record for a [`WorkItemProcessor`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkInput<T>
where
    T: 'static,
{
    /// The unique ID of the work item.
    pub id: Value,

    /// The input data for the work item.
    #[serde(flatten)]
    pub data: T,
}

impl<T> WorkInput<T>
where
    T: DeserializeOwned + Send + 'static,
{
    /// Convert from a JSON value to the input type.
    pub fn from_json(value: Value) -> Result<Self> {
        serde_json::from_value::<Self>(value).context("failed to deserialize input")
    }

    /// Read a stream from a [`Path`] or from standard input.
    pub async fn read_stream(
        ui: Ui,
        path: Option<&Path>,
    ) -> Result<BoxedStream<Result<Self>>> {
        Ok(read_jsonl_or_csv(ui, path)
            .await?
            .map(|value| Self::from_json(value?))
            .boxed())
    }
}

/// Output status of a work item.
#[derive(Clone, Copy, Debug, JsonSchema, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkStatus {
    // The work item was successful.
    Ok,

    // Partial data.
    Incomplete,

    // The work item failed.
    Failed,
}

/// Output record from a [`WorkItemProcessor`].
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct WorkOutput<T>
where
    T: 'static,
{
    /// The unique ID of the work item.
    pub id: Value,

    /// What is the status of this work item?
    pub status: WorkStatus,

    /// How much money do we think we spent?
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost: Option<f64>,

    /// How many tokens did we use?
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,

    /// Any errors that occurred during processing.
    pub errors: Vec<String>,

    /// The output data for the work item.
    #[serde(flatten)]
    pub data: T,
}

/// Trait implemented by output records from a [`WorkItemProcessor`].
impl<T> WorkOutput<T>
where
    T: Clone + Serialize + Send + 'static,
{
    /// Create a new failed output record.
    pub fn new_failed(id: Value, errors: Vec<String>, data: T) -> Self {
        Self {
            id,
            status: WorkStatus::Failed,
            estimated_cost: None,
            token_usage: None,
            errors,
            data,
        }
    }

    /// Convert from the output type to a JSON value.
    pub fn to_json(&self) -> Result<Value> {
        serde_json::to_value::<Self>((*self).to_owned())
            .context("failed to serialize output")
    }

    /// Write a stream of outputs to a [`Path`] or to standard output.
    pub async fn write_stream(
        ui: &Ui,
        path: Option<&Path>,
        stream: BoxedStream<Result<Self>>,
        stream_opts: &StreamOpts,
    ) -> Result<()> {
        let (stream, counters) = WorkOutputCounters::wrap_stream(stream);
        let output = stream
            .map(|value| {
                let value = value?;
                value.to_json()
            })
            .boxed();
        write_output(path, output).await?;
        counters.finish(ui, stream_opts)
    }
}

/// Counters associated with a work item.
#[derive(Clone, Debug, Default)]
pub struct WorkOutputCounters {
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

impl WorkOutputCounters {
    /// Wrap a stream with counters.
    pub fn wrap_stream<T>(
        stream: BoxedStream<Result<WorkOutput<T>>>,
    ) -> (
        BoxedStream<Result<WorkOutput<T>>>,
        Arc<Mutex<WorkOutputCounters>>,
    ) {
        let counters = Arc::new(Mutex::new(Self::default()));
        let counters_clone = counters.clone();
        let stream = stream
            .map(move |value| {
                let value = value?;
                counters_clone.update(&value);
                Ok(value)
            })
            .boxed();
        (stream, counters)
    }
}

/// We actually want to put methods in `Mutex<WorkOutputCounters>`, because
/// that's the type we actually work with. To do that, we need to define an
/// extension trait with the methods we want.
pub trait WorkItemCounterExt {
    /// Update counters for a work item.
    fn update<T>(&self, item: &WorkOutput<T>);

    /// Display counter values to the user.
    fn finish(self: Arc<Self>, ui: &Ui, stream_opts: &StreamOpts) -> Result<()>;
}

impl WorkItemCounterExt for Mutex<WorkOutputCounters> {
    fn update<T>(&self, item: &WorkOutput<T>) {
        // Hold a sync lock, but just for an instant to update counters.
        let mut counters = self.lock().expect("lock poisoned");
        counters.total_record_count += 1;
        if item.status != WorkStatus::Ok {
            counters.failure_count += 1;
        } else if !item.errors.is_empty() {
            counters.non_fatal_error_count += item.errors.len();
        }
        if let Some(cost) = item.estimated_cost {
            counters.cost_estimate += cost;
        }
        if let Some(token_usage) = &item.token_usage {
            counters.token_usage += token_usage.clone();
        }
    }

    fn finish(self: Arc<Self>, ui: &Ui, stream_opts: &StreamOpts) -> Result<()> {
        let counters = self.lock().expect("lock poisoned").to_owned();
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
        if failure_rate > stream_opts.allowed_failure_rate {
            Err(anyhow::anyhow!(
                "{}/{} ({:.2}%) of outputs were failures, but only {:.2}% were allowed",
                counters.failure_count,
                counters.total_record_count,
                failure_rate * 100.0,
                stream_opts.allowed_failure_rate * 100.0
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
pub struct WorkItem<InputData, OutputData>
where
    InputData: 'static,
    OutputData: 'static,
{
    /// The input to the work item.
    pub input: WorkInput<InputData>,

    /// The one-shot channel on which to return the result.
    pub tx: oneshot::Sender<Result<WorkOutput<OutputData>>>,
}

/// API shared by workers.
///
/// This is fairly bare bones; you'll probably want to use [`WorkQueue`] and
/// [`WorkQueueHandle`] in normal usage.
pub trait WorkItemProcessor {
    type InputData: 'static;
    type OutputData: 'static;

    /// Process a work item. The result will be sent to `item.tx`.
    ///
    /// This should normally only block if our processing capacity has been
    /// maxed out.
    async fn submit_work_item(
        &self,
        item: WorkItem<Self::InputData, Self::OutputData>,
    ) -> Result<()>;

    /// Process an input and return a channel that will receive the output.
    ///
    /// This should normally only block if our processing capacity has been
    /// maxed out.
    async fn submit_input(
        &self,
        input: WorkInput<Self::InputData>,
    ) -> Result<oneshot::Receiver<Result<WorkOutput<Self::OutputData>>>> {
        let (tx, rx) = oneshot::channel();
        let item = WorkItem { input, tx };
        self.submit_work_item(item).await?;
        Ok(rx)
    }

    /// Process an input and wait for the output.
    async fn process_blocking(
        &self,
        input: WorkInput<Self::InputData>,
    ) -> Result<WorkOutput<Self::OutputData>> {
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
type WorkFn<InputData, OutputData> = Arc<
    dyn Fn(WorkInput<InputData>) -> BoxedFuture<Result<WorkOutput<OutputData>>>
        + Send
        + Sync
        + 'static,
>;

/// A handle to a [`WorkQueue`].
///
/// This is basically just a wrapper around a [`mpsc::Sender`] that implements
/// [`WorkItemProcessor`]. It can be cloned cheaply and passed around.
pub struct WorkQueueHandle<InputData, OutputData>
where
    InputData: 'static,
    OutputData: 'static,
{
    /// Our sender.
    tx: mpsc::Sender<WorkItem<InputData, OutputData>>,
}

impl<InputData, OutputData> WorkQueueHandle<InputData, OutputData>
where
    InputData: Send + 'static,
    OutputData: Send + 'static,
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
        input: BoxedStream<Result<WorkInput<InputData>>>,
    ) -> BoxedStream<BoxedFuture<Result<WorkOutput<OutputData>>>> {
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
impl<InputData, OutputData> Clone for WorkQueueHandle<InputData, OutputData> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl<InputData, OutputData> WorkItemProcessor for WorkQueueHandle<InputData, OutputData>
where
    InputData: 'static,
    OutputData: 'static,
{
    type InputData = InputData;
    type OutputData = OutputData;

    async fn submit_work_item(
        &self,
        item: WorkItem<Self::InputData, Self::OutputData>,
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
pub struct WorkQueue<InputData, OutputData>
where
    InputData: 'static,
    OutputData: 'static,
{
    /// Queue for submitting work items.
    tx: mpsc::Sender<WorkItem<InputData, OutputData>>,
}

impl<InputData, OutputData> WorkQueue<InputData, OutputData>
where
    InputData: Send + 'static,
    OutputData: Send + 'static,
{
    /// Create a new work queue with the given concurrency limit.
    ///
    /// Note that up to `concurrency_limit` work may be waiting at any one time, and another
    /// `concurrency_limit` work items may be in progress. This means that the total number
    /// of work items in the system at any time may be up to `2 * concurrency_limit`.
    pub fn new(
        concurrency_limit: usize,
        work_fn: WorkFn<InputData, OutputData>,
    ) -> Result<(Self, JoinWorker)> {
        let (tx, rx) = mpsc::channel(concurrency_limit);
        let worker = tokio::spawn(async move {
            rx.for_each_concurrent(
                concurrency_limit,
                |item: WorkItem<InputData, OutputData>| async {
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
    pub fn handle(&self) -> WorkQueueHandle<InputData, OutputData> {
        WorkQueueHandle {
            tx: self.tx.clone(),
        }
    }
}
