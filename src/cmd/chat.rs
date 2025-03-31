//! The `chat` subcommand.

use std::{
    iter,
    sync::{Arc, Mutex},
};

use anyhow::anyhow;
use async_openai::{Client, config::OpenAIConfig, types::CreateChatCompletionResponse};
use futures::StreamExt;
use keen_retry::{ExponentialJitter, ResolvedResult, RetryResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::field;

use crate::{
    io::{JsonObject, read_json_or_toml, read_jsonl_or_csv, write_output},
    prelude::*,
    prompt::ChatPrompt,
    retry::{
        IntoRetryResult as _, is_known_openai_transient, retry_result_fatal,
        retry_result_ok, try_with_retry_result,
    },
};

/// Run the `chat` subcommand.
#[instrument(level = "debug", skip_all)]
pub async fn cmd_chat(
    input_path: Option<&Path>,
    job_count: usize,
    model: &str,
    prompt_path: &Path,
    schema_path: &Path,
    output_path: Option<&Path>,
) -> Result<()> {
    // Open up our input stream.
    let input = read_jsonl_or_csv(input_path).await?;

    // Read our prompt.
    let prompt = read_json_or_toml::<ChatPrompt>(prompt_path).await?;

    // Read our schema.
    //
    // TODO: Make sure `description` fields are present?
    let schema = read_json_or_toml::<Value>(schema_path).await?;
    let validator = jsonschema::async_validator_for(&schema).await?;

    // Create our OpenAI client.
    let mut client_config = OpenAIConfig::new();
    if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        client_config = client_config.with_api_key(api_key);
    }
    if let Ok(api_base) = std::env::var("OPENAI_API_BASE") {
        client_config = client_config.with_api_base(api_base);
    }
    let client = Client::with_config(client_config);

    // Process each record in the input stream, using
    // `futures::StreamExt::buffered` to limit the number of concurrent jobs.
    let state = Arc::new(ProcessorState {
        client,
        model: model.to_string(),
        prompt,
        schema,
        validator,
    });
    let futures = Box::into_pin(input)
        .map(move |map| {
            let state = state.clone();
            async move {
                let map = map?;
                process_record(state, map).await
            }
        })
        .boxed();
    let output = futures.buffered(job_count).boxed();
    write_output(output_path, output).await?;
    Ok(())
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
#[instrument(level = "debug", skip_all, fields(id = field::Empty))]
async fn process_record(
    state: Arc<ProcessorState>,
    object: JsonObject,
) -> Result<JsonObject> {
    let input_record = serde_json::from_value::<InputRecord>(Value::Object(object))?;
    let id = input_record.id.clone();
    tracing::Span::current().record("id", field::display(&id));

    // Render our prompt.
    let prompt = state
        .prompt
        .render_prompt(&input_record.template_bindings)
        .context("Error rendering prompt")?;
    debug!(%prompt, "Prompt");

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

    let output_record = OutputRecord::from_resolved_result(id, result);
    Ok(serde_json::to_value(&output_record)?
        .as_object()
        .expect("output record should be an object")
        .clone())
}

/// An input record.
#[derive(Debug, Deserialize)]
struct InputRecord {
    /// The record's unique identifier.
    id: Value,

    /// Other fields. We keep these "flattened" in the record because they're
    /// under the control of the caller, and because our input format may be a
    /// CSV file, which is "flat".
    #[serde(flatten)]
    template_bindings: JsonObject,
}

/// An output record.
#[derive(Debug, Serialize)]
struct OutputRecord {
    /// The record's unique identifier.
    id: Value,

    /// The response from the LLM. If this is present, the request succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<Value>,

    /// Any errors that occurred. Some errors may be present, even on success,
    /// if the LLM recovered from a transient error.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<String>,
}

impl OutputRecord {
    /// Create a new output record from a [`ResolvedResult`].
    fn from_resolved_result(
        id: Value,
        result: ResolvedResult<(), (), Value, anyhow::Error>,
    ) -> Self {
        match result {
            ResolvedResult::Ok { output, .. } => OutputRecord {
                id,
                response: Some(output),
                errors: vec![],
            },
            ResolvedResult::Fatal { error, .. } => OutputRecord {
                id,
                response: None,
                errors: vec![error.to_string()],
            },
            ResolvedResult::Recovered {
                output,
                retry_errors,
                ..
            } => OutputRecord {
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
            } => OutputRecord {
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

/// Process the data portion of a record.
#[instrument(level = "debug", skip(state, prompt))]
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
                "name": &state.schema.get("title"),
                "schema": &state.schema,
                "strict": true,
            },
        },
        "messages": prompt,
    });
    trace!(?chat_request, "OpenAI request");

    // Call OpenAI.
    let chat_result = try_with_retry_result!(
        state
            .client
            .chat()
            .create_byot(chat_request)
            .await
            .into_retry_result(is_known_openai_transient)
    );
    trace!(?chat_result, "OpenAI response");
    let response = try_with_retry_result!(
        serde_json::from_value::<CreateChatCompletionResponse>(chat_result)
            .context("Error parsing OpenAI response")
            .into_fatal()
    );

    // Get the content from our response & parse as JSON.
    let content = match response.choices.first() {
        Some(choice) => &choice.message.content.as_deref().unwrap_or_default(),
        None => {
            return retry_result_fatal(anyhow!("No choices in OpenAI response"));
        }
    };
    let response = try_with_retry_result!(
        serde_json::from_str::<Value>(content)
            .context("Error parsing OpenAI response content")
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
