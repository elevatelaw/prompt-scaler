//! Concurrent chat requests implemented as an async stream.

use std::{
    error, fmt, iter,
    ops::AddAssign,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_openai::{
    Client,
    config::OpenAIConfig,
    error::OpenAIError,
    types::{
        CreateChatCompletionRequest, CreateChatCompletionRequestArgs,
        CreateChatCompletionResponse, ResponseFormat, ResponseFormatJsonSchema,
    },
};
use clap::Args;
use futures::{FutureExt as _, TryFutureExt};
use keen_retry::{ExponentialJitter, ResolvedResult, RetryResult};
use schemars::JsonSchema;
use serde_json::Map;
use tokio::time;

use super::work::{WorkInput, WorkOutput, WorkQueue, WorkStatus};
use crate::{
    async_utils::{BoxedFuture, BoxedStream, JoinWorker, io::JsonObject},
    llm_client::{LiteLlmModel, create_llm_client, litellm_model_info},
    prelude::*,
    prompt::ChatPrompt,
    retry::{
        IntoRetryResult as _, is_known_openai_transient, retry_result_fatal,
        retry_result_ok, try_with_retry_result,
    },
};

/// Our chat-related options.
#[derive(Args, Clone, Debug)]
pub struct LlmOpts {
    /// An upper limit on the number of completion tokens to generate. This may
    /// help prevent runaway responses, but it may also cause incomplete
    /// results. For English, many models have around 4 bytes per token.
    #[clap(long)]
    pub max_completion_tokens: Option<u32>,

    /// The temperature to use for sampling, between 0.0 and 2.0. Higher values
    /// may the output more random, while lower values may make it more
    /// deterministic. Defaults to the model's default.
    #[clap(long)]
    pub temperature: Option<f32>,

    /// The top-p sampling value to use, between 0.0 and 1.0. This is an
    /// alternative to temperature sampling. See your model's API docs for an
    /// explanation.
    #[clap(long)]
    pub top_p: Option<f32>,

    /// A timeout, in seconds, for the LLM to return a complete response.
    /// Note that even if a request times out, you'll probably still be charged.
    /// Useful dealing with runaway responses and overloaded servers.
    #[clap(long)]
    pub timeout: Option<u64>,
}

/// An input record.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct ChatInput {
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
}

impl WorkOutput<ChatOutput> {
    /// Create a new output record from a [`ResolvedResult`].
    fn from_resolved_result(
        id: Value,
        model: Option<&LiteLlmModel>,
        result: ResolvedResult<(), (), (Option<TokenUsage>, Value), anyhow::Error>,
    ) -> Self {
        let estimate_cost =
            |usage: Option<&TokenUsage>| usage.and_then(|u| u.estimate_cost(model));
        let full_err = |err: anyhow::Error| format!("{:?}", err);
        match result {
            ResolvedResult::Ok {
                output: (token_usage, response),
                ..
            } => WorkOutput {
                id,
                status: WorkStatus::Ok,
                errors: vec![],
                estimated_cost: estimate_cost(token_usage.as_ref()),
                token_usage,
                data: ChatOutput {
                    response: Some(response),
                },
            },
            ResolvedResult::Fatal { error, .. } => WorkOutput {
                id,
                status: WorkStatus::Failed,
                errors: vec![full_err(error)],
                estimated_cost: None,
                token_usage: None,
                data: ChatOutput { response: None },
            },
            ResolvedResult::Recovered {
                output: (token_usage, response),
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
            } => WorkOutput {
                id,
                status: WorkStatus::Failed,
                errors: retry_errors
                    .into_iter()
                    .map(full_err)
                    .chain(iter::once(full_err(fatal_error)))
                    .collect(),
                estimated_cost: None,
                token_usage: None,
                data: ChatOutput { response: None },
            },
        }
    }
}

/// Token usage.
#[derive(Clone, Debug, Default, JsonSchema, Serialize)]
pub struct TokenUsage {
    /// How many tokens were used in the prompt?
    pub prompt_tokens: u64,

    /// How many tokens were used in the response?
    pub completion_tokens: u64,
}

impl TokenUsage {
    /// Was our token usage zero?
    pub fn is_zero(&self) -> bool {
        self.prompt_tokens == 0 && self.completion_tokens == 0
    }

    /// Estimate the cost of this token usage.
    pub fn estimate_cost(&self, model: Option<&LiteLlmModel>) -> Option<f64> {
        if let Some(model) = model {
            let input_cost =
                self.prompt_tokens as f64 * model.model_info.input_cost_per_token;
            let output_cost =
                self.completion_tokens as f64 * model.model_info.output_cost_per_token;
            Some(input_cost + output_cost)
        } else {
            None
        }
    }
}

impl AddAssign for TokenUsage {
    fn add_assign(&mut self, other: Self) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
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
    let client = create_llm_client()?;

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

    let state = Arc::new(ProcessorState {
        client,
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
    /// Our OpenAI client.
    client: Client<OpenAIConfig>,

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

/// An error which occurred while calling an LLM.
#[derive(Debug)]
enum LlmError {
    /// An OpenAI error.
    OpenAI(OpenAIError),

    /// A timeout error.
    Timeout,
}

impl LlmError {
    /// Is this a known transient error?
    fn is_known_transient(&self) -> bool {
        match self {
            LlmError::OpenAI(err) => is_known_openai_transient(err),
            // Runaway LLM responses and some kinds of network timeouts can be retried
            // with hope of a better result.
            LlmError::Timeout => true,
        }
    }
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LlmError::OpenAI(err) => write!(f, "OpenAI error: {}", err),
            LlmError::Timeout => write!(f, "LLM request timed out"),
        }
    }
}

impl error::Error for LlmError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            LlmError::OpenAI(err) => Some(err),
            LlmError::Timeout => None,
        }
    }
}

/// Process a single JSON Object.
#[instrument(level = "debug", skip_all, fields(id = %input_record.id))]
async fn run_chat(
    state: Arc<ProcessorState>,
    mut input_record: WorkInput<ChatInput>,
) -> Result<WorkOutput<ChatOutput>> {
    let id = input_record.id.clone();

    // Render our prompt.
    trace!(
        template_bindings = ?input_record.data.template_bindings,
        "Template bindings"
    );
    let prompt = state
        .prompt
        .render_prompt(&input_record.data.template_bindings)
        .context("Error rendering prompt")?;

    // Build our JSON Schema options.
    let json_schema = ResponseFormatJsonSchema {
        name: state
            .schema
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("ResponseFormat")
            .to_owned(),
        schema: Some(state.schema.clone()),
        strict: Some(true),
        description: None,
    };

    // Turn our prompt into a chat request.
    let mut req = CreateChatCompletionRequestArgs::default();
    req.model(state.model.clone())
        .messages(prompt)
        .response_format(ResponseFormat::JsonSchema { json_schema });
    let mut need_to_disable_store = true;
    if let Some(model_info) = state.model_info {
        eprintln!("Provider: {}", model_info.model_info.litellm_provider);
        if model_info.model_info.litellm_provider == "anthropic" {
            // For OpenAI (and possibly other providers with the same API), we
            // need to set `store` to false to prevent the API from storing
            // responses for later REST calls. But LiteLLM doesn't know about
            // this parameter, and doesn't remove it when calling Anthropic
            // models.
            need_to_disable_store = false;
        }
    }
    if need_to_disable_store {
        req.store(false);
    }
    if let Some(max_completion_tokens) = state.llm_opts.max_completion_tokens {
        req.max_completion_tokens(max_completion_tokens);
    }
    if let Some(temperature) = state.llm_opts.temperature {
        req.temperature(temperature);
    }
    if let Some(top_p) = state.llm_opts.top_p {
        req.top_p(top_p);
    }
    let req = req.build().context("Error building request")?;
    trace!(?req, "Request");

    // Release the input data, because it adds up, especially for images.
    input_record.data.template_bindings = Map::default();

    // If we have a transient failure, back off exponentially.
    let jitter = ExponentialJitter::FromBackoffRange {
        backoff_range_millis: 1..=30_000,
        re_attempts: 5,
        jitter_ratio: 0.2,
    };

    // Do our real work, retrying as specified.
    let attempt_number = Mutex::new(0);
    let result = run_chat_inner(&attempt_number, state.as_ref(), &req)
        .await
        .retry_with_async(|_| async {
            run_chat_inner(&attempt_number, state.as_ref(), &req).await
        })
        .with_exponential_jitter(|| jitter)
        .await
        .inspect_fatal(|_, fatal_error| {
            error!(
                "FAILED with error {fatal_error:?}"
            )
        })
        .inspect_recovered(|_, _, retry_errors_list| {
            warn!(
                "suceeded after retrying {} times (failed attempts: [{}])",
                retry_errors_list.len(),
                keen_retry::loggable_retry_errors(retry_errors_list)
            )
        })
        .inspect_given_up(|_, retry_errors_list, fatal_error| {
            error!(
                "FAILED after exhausting all {} retrying attempts with error {fatal_error:?}. Previous transient failures: [{}]",
                retry_errors_list.len(),
                keen_retry::loggable_retry_errors(retry_errors_list)
            )
        });

    Ok(WorkOutput::<ChatOutput>::from_resolved_result(
        id,
        state.model_info,
        result,
    ))
}

/// Process the data portion of a record.
#[instrument(level = "debug", skip_all, fields(attempt_number = %*attempt_number.lock().expect("lock poisoned")))]
async fn run_chat_inner(
    attempt_number: &Mutex<u64>,
    state: &ProcessorState,
    req: &CreateChatCompletionRequest,
) -> RetryResult<(), (), (Option<TokenUsage>, Value), anyhow::Error> {
    // Increment our attempt number.
    let _current_attempt = {
        let mut attempt_number = attempt_number.lock().expect("lock poisoned");
        let current_attempt = *attempt_number;
        *attempt_number += 1;
        current_attempt
    };

    // Call OpenAI.
    let chat = state.client.chat();
    let mut chat_future = chat.create_byot(req).map_err(LlmError::OpenAI).boxed();
    if let Some(timeout) = state.llm_opts.timeout {
        // If we have a timeout, wrap our future in a timeout, and merge the errors
        // from the `Result<Result<_, LlmError>, Elapsed>` into a single level.
        chat_future = time::timeout(Duration::from_secs(timeout), chat_future)
            .map(|result| match result {
                Ok(inner) => inner,
                Err(_) => Err(LlmError::Timeout),
            })
            .boxed();
    }
    let chat_result: Value = try_with_retry_result!(
        chat_future
            .await
            .into_retry_result(LlmError::is_known_transient)
    );
    debug!(%chat_result, "OpenAI response");
    let response = try_with_retry_result!(
        serde_json::from_value::<CreateChatCompletionResponse>(chat_result)
            .context("Error parsing OpenAI response")
            .into_fatal()
    );

    // How many tokens did we use?
    let token_usage = response.usage.map(|usage| TokenUsage {
        prompt_tokens: u64::from(usage.prompt_tokens),
        completion_tokens: u64::from(usage.completion_tokens),
    });

    // Get the content from our response & parse as JSON.
    let choice = match response.choices.first() {
        Some(choice) => choice,
        None => {
            return retry_result_fatal(anyhow!("No choices in OpenAI response"));
        }
    };
    if choice.finish_reason == Some(async_openai::types::FinishReason::ContentFilter) {
        return retry_result_fatal(anyhow!(
            "Content filter triggered (may also be a RECITATION error for Gemini models)"
        ));
    }
    let content = choice.message.content.as_deref().unwrap_or_default();
    let response = try_with_retry_result!(
        serde_json::from_str::<Value>(content)
            .with_context(|| format!(
                "Error parsing OpenAI response content: {:?}",
                content
            ))
            // If we didn't get JSON here, it's because the model didn't
            // generate JSON. So give it another chance.
            .into_transient()
    );
    debug!(%content, "Response");

    // Validate the result using JSON Schema. Schema validation failure is
    // treated as a transient retry failure, because it may be caused by a dodgy
    // implementation of `response_format` by a specific LLM endpoint.
    try_with_retry_result!(
        state
            .validator
            .validate(&response)
            .map_err(|err| err.to_owned())
            .with_context(|| format!("Failed to validate {}:", response))
            // Invalid JSON means the model didn't follow the schema. Let it try
            // again.
            .into_transient()
    );

    retry_result_ok((token_usage, response))
}
