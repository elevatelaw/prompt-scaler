//! The `chat` subcommand.

use std::sync::Arc;

use async_openai::{
    Client, config::OpenAIConfig, error::OpenAIError, types::CreateChatCompletionResponse,
};
use futures::StreamExt;
use keen_retry::{ExponentialJitter, RetryResult};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::field;

use crate::{
    io::{JsonObject, read_json_or_toml, read_jsonl_or_csv, write_output},
    prelude::*,
    prompt::ChatPrompt,
};

/// Run the `chat` subcommand.
#[instrument(level = "debug", skip_all)]
pub async fn cmd_chat(
    input_path: Option<&Path>,
    job_count: usize,
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
    let client = Client::new();

    // Process each record in the input stream, using
    // `futures::StreamExt::buffered` to limit the number of concurrent jobs.
    let state = Arc::new(ProcessorState {
        client,
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

    // If we have a transient failure, back off exponentially.
    let jitter = ExponentialJitter::FromBackoffRange {
        backoff_range_millis: 1..=30_000,
        re_attempts: 5,
        jitter_ratio: 0.2,
    };

    // Do our real work, retrying as specified.
    let response = process_data(0, state, input_record.template_bindings)
        .await
        .retry_with_async(move |(attempt_number, state, bindings)| async move {
            process_data(attempt_number, state, bindings).await
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
        })
        .into_result()?;

    let output_record = OutputRecord { id, response };
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

    /// The response from the LLM.
    response: Value,
}

/// Process the data portion of a record.
#[instrument(level = "debug", skip(state, bindings))]
async fn process_data(
    attempt_number: u64,
    state: Arc<ProcessorState>,
    bindings: JsonObject,
) -> RetryResult<(), (u64, Arc<ProcessorState>, JsonObject), Value, anyhow::Error> {
    // Render our prompt.
    let prompt = match state.prompt.render_prompt(&bindings) {
        Ok(prompt) => prompt,
        Err(error) => {
            return RetryResult::Fatal {
                input: (attempt_number + 1, state, bindings),
                error,
            };
        }
    };
    debug!(%prompt, "Prompt");

    // Call OpenAI.
    let chat_result = state
        .client
        .chat()
        .create_byot(json!({
            "model": "gpt-4o-mini",
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
        }))
        .await;
    let response = match chat_result {
        Ok(response) => {
            let parse_result =
                serde_json::from_value::<CreateChatCompletionResponse>(response);
            match parse_result {
                Ok(response) => response,
                Err(error) => {
                    return RetryResult::Fatal {
                        input: (attempt_number + 1, state, bindings),
                        error: anyhow::Error::from(error)
                            .context("Error parsing OpenAI response"),
                    };
                }
            }
        }
        Err(error) if is_known_transient(&error) => {
            return RetryResult::Transient {
                input: (attempt_number + 1, state, bindings),
                error: anyhow::Error::from(error).context("Error calling OpenAI"),
            };
        }
        Err(error) => {
            return RetryResult::Fatal {
                input: (attempt_number, state, bindings),
                error: anyhow::Error::from(error).context("Error calling OpenAI"),
            };
        }
    };

    // Get the content from our response & parse as JSON.
    let content = match response.choices.first() {
        Some(choice) => &choice.message.content.as_deref().unwrap_or_default(),
        None => {
            return RetryResult::Fatal {
                input: (attempt_number + 1, state, bindings),
                error: anyhow::anyhow!("No choices in OpenAI response"),
            };
        }
    };
    let response = match serde_json::from_str::<Value>(content) {
        Ok(response) => response,
        Err(error) => {
            return RetryResult::Fatal {
                input: (attempt_number + 1, state, bindings),
                error: anyhow::Error::from(error)
                    .context("Error parsing OpenAI response content"),
            };
        }
    };
    debug!(%content, "Response");

    // Validate the result using JSON Schema. Schema validation failure is
    // treated as a transient retry failure, because it may be caused by a dodgy
    // implementation of `response_format` by a specific LLM endpoint.
    let validation_result = state
        .validator
        .validate(&response)
        .map_err(|err| err.to_owned())
        .with_context(|| format!("Failed to validate {}:", response));
    match validation_result {
        Ok(()) => RetryResult::Ok {
            reported_input: (),
            output: response,
        },
        Err(error) => RetryResult::Transient {
            // Pass these through to the next retry. We need to do this the hard
            // way because [`keen_retry`] doesn't want us to use `clone()`.
            input: (attempt_number.saturating_add(1), state, bindings),
            error,
        },
    }
}

/// Is this error likely to be transient?
///
/// By default, we assume errors are not transient, until they're been observed
/// in the wild, investigated and determined to be transient. The prevents us
/// from doing large numbers of retries with exponential backoff on errors that
/// will never resolve.
fn is_known_transient(error: &OpenAIError) -> bool {
    match error {
        OpenAIError::Reqwest(error) => {
            if let Some(status) = error.status() {
                let transient_failures = [
                    StatusCode::TOO_MANY_REQUESTS,
                    StatusCode::BAD_GATEWAY,
                    StatusCode::SERVICE_UNAVAILABLE,
                    StatusCode::GATEWAY_TIMEOUT,
                ];
                transient_failures.contains(&status)
            } else {
                false
            }
        }
        _ => false,
    }
}
