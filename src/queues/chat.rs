//! Concurrent chat requests implemented as an async stream.

use std::{iter, sync::Arc};

use futures::FutureExt as _;
use keen_retry::{ExponentialJitter, ResolvedResult};
use leaky_bucket::RateLimiter;
use schemars::JsonSchema;
use serde_json::Map;

use super::work::{WorkInput, WorkOutput, WorkQueue, WorkStatus};
use crate::{
    async_utils::{BoxedFuture, BoxedStream, JoinWorker, io::JsonObject},
    drivers::{ChatCompletionResponse, Driver, LlmOpts, LlmRetryResult, TokenUsage},
    litellm::{LiteLlmModel, litellm_model_info},
    prelude::*,
    prompt::{ChatPrompt, Rendered},
    retry::{retry_result_ok, retry_with_backoff, try_retry_result, try_transient},
};

/// An input record.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct ChatInput {
    /// Skip LLM processing and return status: "skipped"
    #[serde(default)]
    pub skip_processing: Option<bool>,

    /// Arbitrary data to pass through to output
    #[serde(default)]
    pub passthrough_data: Option<JsonObject>,

    /// Other fields. We keep these "flattened" in the record because they're
    /// under the control of the caller, and because our input format may be a
    /// CSV file, which is inherently "flat".
    #[serde(flatten)]
    pub template_bindings: JsonObject,
}

/// An output record.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct ChatOutput {
    /// The response from the LLM. If this is present, the request succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,

    /// Passthrough data from input
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passthrough_data: Option<JsonObject>,
}

impl ChatOutput {
    /// Create an empty chat output record for use when an error occurs.
    pub fn empty_for_error(passthrough_data: Option<JsonObject>) -> Self {
        Self {
            response: None,
            passthrough_data,
        }
    }
}

impl WorkOutput<ChatOutput> {
    /// Create a new output record from a [`ResolvedResult`].
    fn from_resolved_result(
        id: Value,
        model: Option<&LiteLlmModel>,
        result: ResolvedResult<(), (), ChatCompletionResponse, anyhow::Error>,
        passthrough_data: Option<JsonObject>,
    ) -> Self {
        let estimate_cost =
            |usage: Option<&TokenUsage>| usage.and_then(|u| u.estimate_cost(model));
        let full_err = |err: anyhow::Error| format!("{err:?}");
        match result {
            ResolvedResult::Ok {
                output:
                    ChatCompletionResponse {
                        response,
                        token_usage,
                    },
                ..
            } => WorkOutput {
                id,
                status: WorkStatus::Ok,
                errors: vec![],
                estimated_cost: estimate_cost(token_usage.as_ref()),
                token_usage,
                data: ChatOutput {
                    response: Some(response),
                    passthrough_data,
                },
            },
            ResolvedResult::Fatal { error, .. } => WorkOutput::new_failed(
                id,
                vec![full_err(error)],
                ChatOutput::empty_for_error(passthrough_data),
            ),
            ResolvedResult::Recovered {
                output:
                    ChatCompletionResponse {
                        response,
                        token_usage,
                    },
                retry_errors,
                ..
            } => WorkOutput {
                id,
                status: WorkStatus::Ok,
                errors: retry_errors.into_iter().map(full_err).collect(),
                estimated_cost: estimate_cost(token_usage.as_ref()),
                token_usage,
                data: ChatOutput {
                    response: Some(response),
                    passthrough_data,
                },
            },
            ResolvedResult::GivenUp {
                retry_errors,
                fatal_error,
                ..
            }
            | ResolvedResult::Unrecoverable {
                retry_errors,
                fatal_error,
                ..
            } => WorkOutput::new_failed(
                id,
                retry_errors
                    .into_iter()
                    .map(full_err)
                    .chain(iter::once(full_err(fatal_error)))
                    .collect(),
                ChatOutput::empty_for_error(passthrough_data),
            ),
        }
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
) -> Result<ChatStreamInfo> {
    // Create our work queue.
    let (queue, worker) =
        create_chat_work_queue(concurrency_limit, prompt, model, llm_opts).await?;
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
) -> Result<(WorkQueue<ChatInput, ChatOutput>, JoinWorker)> {
    // Create our OpenAI client.
    let driver = llm_opts.driver.create_driver().await?;

    // See if we can get LiteLLM info for this model.
    let model_info = litellm_model_info(&model).await;
    if let Some(model_info) = model_info {
        debug!(model_info = %model_info.to_string(), "Model info");
    } else {
        debug!(model = %model, "Model info not available");
    }

    // Read our schema.
    //
    // TODO: Make sure `description` fields are present?
    let schema = prompt.response_schema.to_json_schema().await?;
    debug!(%schema, "Schema");
    let validator = jsonschema::validator_for(&schema)?;

    // Construct a rate limiter to control the rate of API requests.
    let rate_limiter = llm_opts.rate_limit.as_ref().map(|rl| rl.to_rate_limiter());

    // Build our shared state.
    let state = Arc::new(ProcessorState {
        driver,
        rate_limiter,
        model,
        prompt,
        schema,
        validator,
        llm_opts,
        model_info,
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
struct ProcessorState {
    /// Our LLM client.
    driver: Box<dyn Driver>,

    /// A rate limiter to control API request rate.
    rate_limiter: Option<RateLimiter>,

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

    /// Model information, if available.
    model_info: Option<&'static LiteLlmModel>,
}

/// Process a single JSON Object.
#[instrument(level = "debug", skip_all, fields(id = %input_record.id))]
async fn run_chat(
    state: Arc<ProcessorState>,
    mut input_record: WorkInput<ChatInput>,
) -> Result<WorkOutput<ChatOutput>> {
    let id = input_record.id.clone();
    let skip_processing = input_record.data.skip_processing.unwrap_or(false);
    let passthrough_data = input_record.data.passthrough_data.take();

    // Early return if skip_processing is true (before prompt rendering)
    if skip_processing {
        return Ok(WorkOutput {
            id,
            status: WorkStatus::Skipped,
            errors: vec![],
            estimated_cost: None,
            token_usage: None,
            data: ChatOutput {
                response: None,
                passthrough_data,
            },
        });
    }

    // Render our prompt.
    trace!(
        template_bindings = ?input_record.data.template_bindings,
        "Template bindings"
    );
    let prompt = state.prompt.render(&input_record.data.template_bindings)?;

    // Release the input data, because it adds up, especially for images.
    input_record.data.template_bindings = Map::default();

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
        state.model_info,
        result,
        passthrough_data,
    ))
}

/// Process the data portion of a record.
#[instrument(level = "debug", skip_all)]
async fn run_chat_inner(
    state: Arc<ProcessorState>,
    prompt: ChatPrompt<Rendered>,
) -> LlmRetryResult<ChatCompletionResponse> {
    // If we have a rate limiter, acquire a permit for one request.
    if let Some(rate_limiter) = state.rate_limiter.as_ref() {
        rate_limiter.acquire(1).await;
    }

    // Call OpenAI.
    let completion_response = try_retry_result!(
        state
            .driver
            .chat_completion(
                &state.model,
                state.model_info,
                &prompt,
                state.schema.clone(),
                &state.llm_opts,
            )
            .await
    );

    // Validate the result using JSON Schema. Schema validation failure is
    // treated as a transient retry failure, because it may be caused by a dodgy
    // implementation of `response_format` by a specific LLM endpoint.
    try_transient!(
        // Invalid JSON means the model didn't follow the schema. Let it try
        // again using `try_transient!`.
        state
            .validator
            .validate(&completion_response.response)
            .map_err(|err| err.to_owned())
            .with_context(|| format!(
                "Failed to validate {}:",
                completion_response.response
            ))
    );

    retry_result_ok(completion_response)
}
