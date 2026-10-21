//! Support for decision models: a work queue which answers questions for each
//! input record.

use std::{collections::BTreeMap, sync::Arc};

use futures::FutureExt as _;
use schemars::JsonSchema;

use super::{
    processor_state::CommonProcessorState,
    work::{WorkInput, WorkOutput, WorkQueue, WorkResponse, WorkStatus},
};
use crate::{
    async_utils::{BoxedFuture, BoxedStream, JoinWorker, io::JsonObject},
    drivers::{
        DriverRetryResult, DriverType, TokenUsage,
        decide::{
            Answer, DecideDriver, DecideDriverOpts, DecideRequest, DecideResponse,
            verify_answers,
        },
    },
    mem_limit::MemLimiter,
    prelude::*,
    prompts::decide::DecidePrompt,
    retry::{
        DEFAULT_JITTER, retry_result_ok, retry_with_backoff, try_fatal,
        try_potentially_transient,
    },
    timeouts::WithTimeout,
};

/// A decision input record.
///
/// Our input format and the prompt together provide `state`, which is sent to
/// the model as-is. Exactly one of these must provide `state`:
///
/// - The input record's [`DecideInput::state`] field, which is used verbatim.
/// - [`DecidePrompt::state`], which is rendered as a template using the record's
///   other fields as bindings.
///
/// If both or neither provide `state`, the whole run stops with an error.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct DecideInput {
    /// A `state` field containing input to pass directly to the model. If
    /// present, the prompt must not contain a [`DecidePrompt::state`] field.
    ///
    /// Custom fields may still be present alongside this: they are used to
    /// render the prompt's `images`, but they are not sent to the model.
    #[serde(default)]
    pub state: Option<Value>,

    /// Other fields. We keep these "flattened" in the record because they're
    /// under the control of the caller, and because our input format may be a
    /// CSV file, which is inherently "flat".
    ///
    /// These are used as template bindings, either for the prompt's `state` or
    /// for its `images`.
    ///
    /// TODO: If neither `state`, `images` nor any other subsitutable field is
    /// present in the prompt, we might want to error or warn.
    #[serde(flatten)]
    pub template_bindings: JsonObject,
}

/// A decision output record.
#[derive(Clone, Debug, Default, JsonSchema, Serialize)]
pub struct DecideOutput {
    /// Answers to questions posed in the prompt, keyed by question name. Will
    /// not be present if the request returned an error.
    ///
    /// Answers are in a "/v1/systemone" shaped format, but the precise meaning
    /// of numeric fields like "confidence" may be dependendent on the model
    /// we're talking to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answers: Option<BTreeMap<String, Answer>>,
}

impl WorkResponse for DecideResponse {
    type Output = DecideOutput;

    fn to_output(self) -> Self::Output {
        DecideOutput {
            answers: Some(self.answers),
        }
    }

    fn token_usage(&self) -> Option<&TokenUsage> {
        self.token_usage.as_ref()
    }
}

/// Return value of [`process_decide_stream`].
pub struct DecideStreamInfo {
    /// Our stream of futures, each of which returns a single decision.
    pub stream: BoxedStream<BoxedFuture<Result<WorkOutput<DecideOutput>>>>,

    /// The worker thread which drives the queue.
    pub worker: JoinWorker,
}

/// Process a stream of input records, using `prompt` and `model` to answer the
/// prompt's questions for each record.
///
/// We take our arguments by value, not reference, because we'll need to hold
/// onto them while we process the stream.
#[instrument(level = "debug", skip(input, prompt))]
pub async fn process_decide_stream(
    concurrency_limit: usize,
    input: BoxedStream<Result<WorkInput<DecideInput>>>,
    prompt: DecidePrompt,
    model: String,
    decide_opts: DecideDriverOpts,
    mem_limiter: MemLimiter,
) -> Result<DecideStreamInfo> {
    // Create our work queue.
    let (queue, worker) = create_decide_work_queue(
        concurrency_limit,
        prompt,
        model,
        decide_opts,
        mem_limiter,
    )
    .await?;
    let handle = queue.handle();
    Ok(DecideStreamInfo {
        stream: handle.process_stream(input).await,
        worker,
    })
}

/// Make a [`WorkQueue`] that handles decisions.
pub async fn create_decide_work_queue(
    concurrency_limit: usize,
    prompt: DecidePrompt,
    model: String,
    decide_opts: DecideDriverOpts,
    mem_limiter: MemLimiter,
) -> Result<(WorkQueue<DecideInput, DecideOutput>, JoinWorker)> {
    // Create our driver.
    let driver = decide_opts.driver.create_driver()?;

    // Look up model cost data.
    let cost = decide_opts
        .driver_opts
        .cost_info(DriverType::Decide(decide_opts.driver), &model);

    // Construct a rate limiter to control the rate of API requests.
    let rate_limiter = decide_opts.driver_opts.rate_limiter();

    // Build our shared state.
    let state = Arc::new(DecideProcessorState {
        common: CommonProcessorState {
            rate_limiter,
            mem_limiter,
            model_cost: cost,
        },
        driver,
        model,
        prompt,
        decide_opts,
    });

    // Define worker function.
    let work_fn = move |input| {
        let state = state.clone();
        run_decide(state, input).boxed()
    };

    // Create our work queue.
    WorkQueue::new(concurrency_limit, Arc::new(work_fn))
}

/// Shared processor state.
#[derive(Debug)]
struct DecideProcessorState {
    /// State fields shared with other queue processor types.
    common: CommonProcessorState,

    /// Our decision driver.
    driver: Box<dyn DecideDriver>,

    /// The model to ask. This rides inside each request, so a server which
    /// routes or loads models on demand can use it.
    model: String,

    /// The prompt to use.
    prompt: DecidePrompt,

    /// The decide driver options to use.
    decide_opts: DecideDriverOpts,
}

/// Process a single input record.
///
/// Failures while preparing a record — assembling `state`, or rendering the
/// prompt — stop the whole run rather than becoming a `failed` record. That's
/// deliberate: those are setup mistakes, and since every row of a given input
/// file has the same shape, reporting them per-record might fail 600,000
/// identically-shaped rows one JSONL record at a time instead of stopping once.
/// (This is also how `chat` behaves.) Failures from the driver, by contrast,
/// become `failed` records, following the retry policy below.
#[instrument(level = "debug", skip_all, fields(id = %input_record.id))]
async fn run_decide(
    state: Arc<DecideProcessorState>,
    input_record: WorkInput<DecideInput>,
) -> Result<WorkOutput<DecideOutput>> {
    let id = input_record.id.clone();
    let skip_processing = input_record.skip_processing.unwrap_or(false);
    let passthrough_data = input_record.passthrough_data;

    // Early return if skip_processing is true (before prompt rendering).
    if skip_processing {
        return Ok(WorkOutput {
            id,
            status: WorkStatus::Skipped,
            errors: vec![],
            estimated_cost: None,
            token_usage: None,
            passthrough_data,
            data: DecideOutput::default(),
        });
    }

    // Render our prompt templates.
    trace!(
        template_bindings = ?input_record.data.template_bindings,
        "Template bindings"
    );
    let prompt = state.prompt.render(
        &input_record.data.template_bindings,
        state.decide_opts.driver_opts.canonical_base_dir(),
    )?;

    // Assemble our request from the rendered prompt and the input record.
    let request = DecideRequest {
        model: state.model.clone(),
        state: assemble_state(prompt.state, &input_record.data)?,
        // Treat an empty list as "no images", so that text-only requests are
        // byte-identical to requests from a prompt with no `images` field.
        images: prompt.images.filter(|images| !images.is_empty()),
        questions: prompt.questions,
    };

    // Release the input data, because it adds up, especially for images.
    drop(input_record.data);

    // Do our real work, retrying as specified.
    let result = retry_with_backoff(DEFAULT_JITTER, || {
        let request = request.clone();
        run_decide_inner(state.clone(), request)
    })
    .await;

    Ok(WorkOutput::<DecideOutput>::from_resolved_result(
        id,
        state.common.model_cost.as_ref(),
        result,
        passthrough_data,
    ))
}

/// Choose the `state` to send for a record.
///
/// Exactly one of `prompt_state` (already rendered) and
/// [`DecideInput::state`] must be present. See [`DecideInput`].
fn assemble_state(prompt_state: Option<Value>, input: &DecideInput) -> Result<Value> {
    match (prompt_state, &input.state) {
        (Some(state), None) => Ok(state),
        (None, Some(state)) => Ok(state.clone()),
        (Some(_), Some(_)) => Err(anyhow!(
            "Both the prompt and this record provide `state`, but only one may: \
             either remove `state` from the prompt, or remove it from the record"
        )),
        (None, None) => Err(anyhow!(
            "Neither the prompt nor this record provides `state`: add a `state` \
             field to the prompt, or to the record"
        )),
    }
}

/// Process a single request, retrying transient failures.
#[instrument(level = "debug", skip_all)]
async fn run_decide_inner(
    state: Arc<DecideProcessorState>,
    request: DecideRequest,
) -> DriverRetryResult<DecideResponse> {
    // If we have a rate limiter, acquire a permit for one request.
    state.common.wait_on_rate_limiter().await;

    // Ask our questions.
    let response = try_potentially_transient!(
        state
            .driver
            .decide(&request)
            .with_timeout(state.decide_opts.decide_timeout_duration())
            .await
    );

    // Check that we got answers to the questions we asked. Unlike chat's JSON
    // Schema validation, this failure is fatal rather than transient: decision
    // models answer from next-token probabilities rather than generating text,
    // so a mismatch means the endpoint is broken and retrying would send the
    // same request to the same broken server.
    try_fatal!(verify_answers(&request.questions, &response.answers));

    retry_result_ok(response)
}
