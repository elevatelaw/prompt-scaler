//! The `chat` subcommand.

use std::sync::Arc;

use futures::StreamExt;
use keen_retry::{ExponentialJitter, RetryResult};
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

    let state = Arc::new(ProcessorState { prompt, validator });
    let futures = Box::into_pin(input)
        .map(move |map| {
            let state = state.clone();
            async move {
                let map = map?;
                process_record(state, map).await
            }
        })
        .boxed();
    let output = futures.buffered(8).boxed();
    write_output(output_path, output).await?;
    Ok(())
}

/// Shared processor state.
#[derive(Debug)]
struct ProcessorState {
    /// The prompt to use.
    prompt: ChatPrompt,

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
                input: (attempt_number, state, bindings),
                error,
            };
        }
    };
    debug!(%prompt, "Prompt");

    // Placeholder implementation.
    let response = json!({
        "punchline": "To get to the other side!",
    });

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
