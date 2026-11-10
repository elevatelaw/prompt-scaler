//! Concurrent chat requests implemented as an async stream.

use std::sync::Arc;

use futures::FutureExt as _;
use keen_retry::ExponentialJitter;
use schemars::JsonSchema;

use super::work::{WorkInput, WorkOutput, WorkQueue, WorkResponse, WorkStatus};
use crate::{
    async_utils::{BoxedFuture, BoxedStream, JoinWorker, io::JsonObject},
    drivers::{
        DriverError, DriverErrorKind, DriverRetryResult, DriverType, TokenUsage,
        llm::{ChatCompletionResponse, LlmDriver, LlmOpts},
    },
    mem_limit::MemLimiter,
    prelude::*,
    prompts::{Rendered, chat::ChatPrompt},
    queues::processor_state::CommonProcessorState,
    retry::{
        retry_result_fatal, retry_result_ok, retry_with_backoff, try_fatal,
        try_potentially_transient, try_transient,
    },
    timeouts::{TimeoutError, WithTimeout},
};

/// Whether a chat queue retries a request whose generation was unusable: the
/// model produced output that doesn't parse or match the schema, or ran past
/// the timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadGenerationRetries {
    /// Retry with backoff. Right for models sampled with a temperature, where
    /// another attempt usually produces different output.
    Retry,
    /// Fail at once, so the caller can fall back to something else. Right for
    /// models decoded greedily, where a retry repeats the same output.
    FailFast,
}

/// Was this error caused by what the model generated, rather than by the API?
fn is_bad_generation(error: &TimeoutError<DriverError>) -> bool {
    match error {
        TimeoutError::Timeout => true,
        TimeoutError::Native(err) => matches!(err.kind, DriverErrorKind::InvalidOutput),
    }
}

/// A chat input record.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct ChatInput {
    /// Other fields. We keep these "flattened" in the record because they're
    /// under the control of the caller, and because our input format may be a
    /// CSV file, which is inherently "flat".
    #[serde(flatten)]
    pub template_bindings: JsonObject,
}

/// A chat output record.
#[derive(Clone, Debug, Default, JsonSchema, Serialize)]
pub struct ChatOutput {
    /// The response from the LLM. If this is present, the request succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
}

impl WorkResponse for ChatCompletionResponse {
    type Output = ChatOutput;

    fn to_output(self) -> Self::Output {
        ChatOutput {
            response: Some(self.response),
        }
    }

    fn token_usage(&self) -> Option<&TokenUsage> {
        self.token_usage.as_ref()
    }
}

/// Return value of [`process_chat_stream`].
pub struct ChatStreamInfo {
    pub stream: BoxedStream<BoxedFuture<Result<WorkOutput<ChatOutput>>>>,
    pub worker: JoinWorker,
}

/// Process a stream of input records, using `prompt` and `model` to generate
/// responses.
///
/// We take our arguments by value, not reference, because we'll need to hold
/// onto them while we process the stream.
#[instrument(level = "debug", skip(input, prompt))]
pub async fn process_chat_stream(
    concurrency_limit: usize,
    input: BoxedStream<Result<WorkInput<ChatInput>>>,
    prompt: ChatPrompt,
    model: String,
    llm_opts: LlmOpts,
    mem_limiter: MemLimiter,
) -> Result<ChatStreamInfo> {
    // Create our work queue.
    let (queue, worker) = create_chat_work_queue(
        concurrency_limit,
        prompt,
        model,
        llm_opts,
        mem_limiter,
        BadGenerationRetries::Retry,
    )
    .await?;
    let handle = queue.handle();
    Ok(ChatStreamInfo {
        stream: handle.process_stream(input).await,
        worker,
    })
}

/// Make a [`WorkQueue`] that handles chats.
pub async fn create_chat_work_queue(
    concurrency_limit: usize,
    prompt: ChatPrompt,
    model: String,
    llm_opts: LlmOpts,
    mem_limiter: MemLimiter,
    bad_generation_retries: BadGenerationRetries,
) -> Result<(WorkQueue<ChatInput, ChatOutput>, JoinWorker)> {
    // Create our LLM client.
    let driver = llm_opts.driver.create_driver().await?;

    // Look up model cost data.
    let cost = llm_opts
        .driver_opts
        .cost_info(DriverType::Llm(llm_opts.driver), &model);

    // Read our schema.
    //
    // TODO: Make sure `description` fields are present?
    let schema = prompt.response_schema.to_json_schema().await?;
    debug!(%schema, "Schema");
    let validator = jsonschema::validator_for(&schema)?;

    // Construct a rate limiter to control the rate of API requests.
    let rate_limiter = llm_opts.driver_opts.rate_limiter();

    // Build our shared state.
    let state = Arc::new(ChatProcessorState {
        common: CommonProcessorState {
            rate_limiter,
            mem_limiter,
            model_cost: cost,
        },
        driver,
        model,
        prompt,
        schema,
        validator,
        llm_opts,
        bad_generation_retries,
    });

    // Define worker function.
    let work_fn = move |input| {
        let state = state.clone();
        run_chat(state, input).boxed()
    };

    // Create our work queue.
    WorkQueue::new(concurrency_limit, Arc::new(work_fn))
}

/// Shared processor state.
#[derive(Debug)]
struct ChatProcessorState {
    /// State fields shared with other queue processor types.
    common: CommonProcessorState,

    /// Our LLM client.
    driver: Box<dyn LlmDriver>,

    /// The model to use.
    model: String,

    /// The prompt to use.
    prompt: ChatPrompt,

    /// Our JSON Schema.
    schema: Value,

    /// Our JSON Schema validator.
    validator: jsonschema::Validator,

    /// The LLM options to use.
    llm_opts: LlmOpts,

    /// Whether to retry unusable generations.
    bad_generation_retries: BadGenerationRetries,
}

/// Process a single JSON Object.
#[instrument(level = "debug", skip_all, fields(id = %input_record.id))]
async fn run_chat(
    state: Arc<ChatProcessorState>,
    input_record: WorkInput<ChatInput>,
) -> Result<WorkOutput<ChatOutput>> {
    let id = input_record.id.clone();
    let skip_processing = input_record.skip_processing.unwrap_or(false);
    let passthrough_data = input_record.passthrough_data;

    // Early return if skip_processing is true (before prompt rendering)
    if skip_processing {
        return Ok(WorkOutput {
            id,
            status: WorkStatus::Skipped,
            errors: vec![],
            estimated_cost: None,
            token_usage: None,
            passthrough_data,
            data: ChatOutput { response: None },
        });
    }

    // Render our prompt.
    trace!(
        template_bindings = ?input_record.data.template_bindings,
        "Template bindings"
    );
    let prompt = state.prompt.render(
        &input_record.data.template_bindings,
        state.llm_opts.driver_opts.canonical_base_dir(),
    )?;

    // Release the input data, because it adds up, especially for images.
    drop(input_record.data);

    // If we have a transient failure, back off exponentially.
    let jitter = ExponentialJitter::FromBackoffRange {
        backoff_range_millis: 1..=30_000,
        re_attempts: 5,
        jitter_ratio: 0.2,
    };

    // Do our real work, retrying as specified.
    let result = retry_with_backoff(jitter, || {
        let prompt = prompt.clone();
        run_chat_inner(state.clone(), prompt)
    })
    .await;

    Ok(WorkOutput::<ChatOutput>::from_resolved_result(
        id,
        state.common.model_cost.as_ref(),
        result,
        passthrough_data,
    ))
}

/// Process the data portion of a record.
#[instrument(level = "debug", skip_all)]
async fn run_chat_inner(
    state: Arc<ChatProcessorState>,
    prompt: ChatPrompt<Rendered>,
) -> DriverRetryResult<ChatCompletionResponse> {
    // If we have a rate limiter, acquire a permit for one request.
    state.common.wait_on_rate_limiter().await;

    // Call LLM.
    let result = state
        .driver
        .chat_completion(
            &state.model,
            &prompt,
            state.schema.clone(),
            &state.llm_opts,
            &state.common.mem_limiter,
        )
        .with_timeout(state.llm_opts.llm_timeout_duration())
        .await;
    let result = match result {
        Err(err)
            if state.bad_generation_retries == BadGenerationRetries::FailFast
                && is_bad_generation(&err) =>
        {
            return retry_result_fatal(err.into());
        }
        result => result,
    };
    let completion_response = try_potentially_transient!(result);

    // Validate the result using JSON Schema. Schema validation failure is
    // treated as a transient retry failure, because it may be caused by a dodgy
    // implementation of `response_format` by a specific LLM endpoint, unless
    // the caller asked us to fail fast.
    let validation = state
        .validator
        .validate(&completion_response.response)
        .map_err(|err| err.to_owned())
        .with_context(|| format!("Failed to validate {}:", completion_response.response));
    match state.bad_generation_retries {
        BadGenerationRetries::Retry => try_transient!(validation),
        BadGenerationRetries::FailFast => try_fatal!(validation),
    }

    retry_result_ok(completion_response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_generations_are_timeouts_and_invalid_output() {
        assert!(is_bad_generation(&TimeoutError::Timeout));
        assert!(is_bad_generation(&TimeoutError::Native(
            DriverError::invalid_output_transient(anyhow!("EOF while parsing"))
        )));
        assert!(!is_bad_generation(&TimeoutError::Native(
            DriverError::invalid_input(anyhow!("bad prompt"))
        )));
    }
}
