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

use std::sync::Arc;

use futures::{
    FutureExt, SinkExt as _, StreamExt,
    channel::{mpsc, oneshot},
};
use tokio::task::JoinHandle;

use crate::{
    io::{BoxedFuture, BoxedStream},
    prelude::*,
};

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
    type Input: Send + 'static;
    type Output: Send + 'static;

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
    Input: Send + 'static,
    Output: Send + 'static,
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
    Input: Send + 'static,
    Output: Send + 'static,
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

    /// Handle to an async background worker that pulls results from the queue
    /// and passes them along.
    worker_handle: JoinHandle<()>,
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
    pub fn new(concurrency_limit: usize, work_fn: WorkFn<Input, Output>) -> Result<Self> {
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
        });
        Ok(Self {
            tx,
            worker_handle: worker,
        })
    }

    /// Get a handle for submitting items to the work queue.
    pub fn handle(&self) -> WorkQueueHandle<Input, Output> {
        WorkQueueHandle {
            tx: self.tx.clone(),
        }
    }

    /// Attempt to close the work queue. This will wait until all sender handles
    /// are eventually shut down. If any sender handles are "leaked", this may
    /// block forever.
    pub async fn close(self) -> Result<()> {
        let Self { tx, worker_handle } = self;
        drop(tx);
        worker_handle
            .await
            .context("failed to join worker thread")?;
        Ok(())
    }
}
