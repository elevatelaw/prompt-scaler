//! Concurrent chat requests implemented as an async stream.

use std::{
    iter,
    sync::{Arc, Mutex},
};

use async_openai::{Client, config::OpenAIConfig, types::CreateChatCompletionResponse};
use futures::FutureExt as _;
use keen_retry::{ExponentialJitter, ResolvedResult, RetryResult};

use super::work::{WorkInput, WorkOutput, WorkQueue};
use crate::{
    async_utils::io::{BoxedFuture, BoxedStream, JsonObject},
    llm_client::create_llm_client,
    prelude::*,
    prompt::ChatPrompt,
    retry::{
        IntoRetryResult as _, is_known_openai_transient, retry_result_fatal,
        retry_result_ok, try_with_retry_result,
    },
};

/// An input record.
#[derive(Clone, Debug, Deserialize)]
pub struct ChatInput {
    /// The record's unique identifier.
    pub id: Value,

    /// Other fields. We keep these "flattened" in the record because they're
    /// under the control of the caller, and because our input format may be a
    /// CSV file, which is inherently "flat".
    #[serde(flatten)]
    pub template_bindings: JsonObject,
}

impl WorkInput for ChatInput {}

/// An output record.
#[derive(Clone, Debug, Serialize)]
pub struct ChatOutput {
    /// The record's unique identifier.
    pub id: Value,

    /// The response from the LLM. If this is present, the request succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,

    /// Any errors that occurred. Some errors may be present, even on success,
    /// if the LLM recovered from a transient error.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

impl ChatOutput {
    /// Create a new output record from a [`ResolvedResult`].
    fn from_resolved_result(
        id: Value,
        result: ResolvedResult<(), (), Value, anyhow::Error>,
    ) -> Self {
        match result {
            ResolvedResult::Ok { output, .. } => ChatOutput {
                id,
                response: Some(output),
                errors: vec![],
            },
            ResolvedResult::Fatal { error, .. } => ChatOutput {
                id,
                response: None,
                errors: vec![error.to_string()],
            },
            ResolvedResult::Recovered {
                output,
                retry_errors,
                ..
            } => ChatOutput {
                id,
                response: Some(output),
                errors: retry_errors.into_iter().map(|e| e.to_string()).collect(),
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
            } => ChatOutput {
                id,
                response: None,
                errors: retry_errors
                    .into_iter()
                    .map(|e| e.to_string())
                    .chain(iter::once(fatal_error.to_string()))
                    .collect(),
            },
        }
    }
}

impl WorkOutput for ChatOutput {
    fn is_failure(&self) -> bool {
        self.response.is_none()
    }
}

/// Return value of [`process_chat_stream`].
pub struct ChatStreamInfo {
    pub stream: BoxedStream<BoxedFuture<Result<ChatOutput>>>,
    pub queue: WorkQueue<ChatInput, ChatOutput>,
}

/// Process a stream of input records, using `prompt` and `model` to generate
/// responses.
///
/// We take our arguments by value, not reference, because we'll need to hold
/// onto them while we process the stream.
#[instrument(level = "debug", skip(input, prompt))]
pub async fn process_chat_stream(
    concurrency_limit: usize,
    input: BoxedStream<Result<ChatInput>>,
    prompt: ChatPrompt,
    model: String,
) -> Result<ChatStreamInfo> {
    // Create our work queue.
    let queue = create_chat_work_queue(concurrency_limit, prompt, model).await?;
    let handle = queue.handle();
    Ok(ChatStreamInfo {
        stream: handle.process_stream(input).await,
        queue,
    })
}

/// Make a [`WorkQueue`] that handles chats.
pub async fn create_chat_work_queue(
    concurrency_limit: usize,
    prompt: ChatPrompt,
    model: String,
) -> Result<WorkQueue<ChatInput, ChatOutput>> {
    // Create our OpenAI client.
    let client = create_llm_client()?;

    // Read our schema.
    //
    // TODO: Make sure `description` fields are present?
    let schema = prompt.response_schema.to_json_schema().await?;
    let validator = jsonschema::async_validator_for(&schema).await?;

    let state = Arc::new(ProcessorState {
        client,
        model,
        prompt,
        schema,
        validator,
    });

    // Define worker function.
    let work_fn = move |input| {
        let state = state.clone();
        process_record(state, input).boxed()
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
}

/// Process a single JSON Object.
#[instrument(level = "debug", skip_all, fields(id = %input_record.id))]
async fn process_record(
    state: Arc<ProcessorState>,
    input_record: ChatInput,
) -> Result<ChatOutput> {
    let id = input_record.id.clone();

    // Render our prompt.
    trace!(
        template_bindings = ?input_record.template_bindings,
        "Template bindings"
    );
    let prompt = state
        .prompt
        .render_prompt(&input_record.template_bindings)
        .context("Error rendering prompt")?;
    trace!(%prompt, "Prompt");

    // If we have a transient failure, back off exponentially.
    let jitter = ExponentialJitter::FromBackoffRange {
        backoff_range_millis: 1..=30_000,
        re_attempts: 5,
        jitter_ratio: 0.2,
    };

    // Do our real work, retrying as specified.
    let attempt_number = Mutex::new(0);
    let result = process_data(&attempt_number, state.as_ref(), &prompt)
        .await
        .retry_with_async(|_| async {
            process_data(&attempt_number, state.as_ref(), &prompt).await
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

    Ok(ChatOutput::from_resolved_result(id, result))
}

/// Process the data portion of a record.
#[instrument(level = "debug", skip_all, fields(attempt_number = %*attempt_number.lock().expect("lock poisoned")))]
async fn process_data(
    attempt_number: &Mutex<u64>,
    state: &ProcessorState,
    prompt: &Value,
) -> RetryResult<(), (), Value, anyhow::Error> {
    // Increment our attempt number.
    let _current_attempt = {
        let mut attempt_number = attempt_number.lock().expect("lock poisoned");
        let current_attempt = *attempt_number;
        *attempt_number += 1;
        current_attempt
    };

    // Create our request.
    let chat_request = json!({
        "model": &state.model,
        "store": false,
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": &state.schema.get("title").cloned().unwrap_or_else(|| json!("ResponseFormat")),
                "schema": &state.schema,
                "strict": true,
            },
        },
        "messages": prompt,
    });
    trace!(%chat_request, "OpenAI request");

    // Call OpenAI.
    let chat_result: Value = try_with_retry_result!(
        state
            .client
            .chat()
            .create_byot(chat_request)
            .await
            .into_retry_result(is_known_openai_transient)
    );
    debug!(%chat_result, "OpenAI response");
    let response = try_with_retry_result!(
        serde_json::from_value::<CreateChatCompletionResponse>(chat_result)
            .context("Error parsing OpenAI response")
            .into_fatal()
    );

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

    retry_result_ok(response)
}
