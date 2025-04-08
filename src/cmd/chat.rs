//! The `chat` subcommand.

use futures::StreamExt;

use crate::{
    chat_stream::{ChatStreamInfo, InputRecord, process_chat_stream},
    io::{read_json_or_toml, read_jsonl_or_csv, write_output},
    prelude::*,
    prompt::ChatPrompt,
};

/// Run the `chat` subcommand.
#[instrument(level = "debug", skip_all)]
pub async fn cmd_chat(
    input_path: Option<&Path>,
    job_count: usize,
    model: &str,
    prompt_path: &Path,
    output_path: Option<&Path>,
) -> Result<()> {
    // Open up our input stream and convert to records.
    let input = read_jsonl_or_csv(input_path)
        .await?
        .map(|value| InputRecord::from_json(value?))
        .boxed();

    // Read our prompt.
    let prompt = read_json_or_toml::<ChatPrompt>(prompt_path).await?;

    // Build our chat stream.
    let ChatStreamInfo {
        stream: futures,
        queue,
    } = process_chat_stream(job_count, input, prompt, model.to_owned()).await?;

    // Resolve our individual LLM requests concurrently, and convert them back to JSON.
    let output = futures
        .buffered(job_count)
        .map(|record| record?.to_json())
        .boxed();

    // Write out our output.
    write_output(output_path, output).await?;

    // Wait for our work queue's background task to exit.
    queue.close().await
}
