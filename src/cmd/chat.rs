//! The `chat` subcommand.

use futures::StreamExt;

use crate::{
    chat_stream::{InputRecord, process_chat_stream},
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
    // Open up our input stream.
    let input = read_jsonl_or_csv(input_path).await?;

    // Parse out input into `InputRecord`s.
    let input = input
        .map(|map| {
            let map = map?;
            let input_record = serde_json::from_value::<InputRecord>(map.clone())
                .with_context(|| format!("Failed to parse input record: {}", map))?;
            Ok(input_record)
        })
        .boxed();

    // Read our prompt.
    let prompt = read_json_or_toml::<ChatPrompt>(prompt_path).await?;

    // Build our chat stream.
    let futures = process_chat_stream(input, prompt, model.to_owned()).await?;

    // Resolve our individual LLM requests concurrently, and convert them back to JSON.
    let output = futures
        .buffered(job_count)
        .map(|record| {
            let record = record?;
            serde_json::to_value(&record).with_context(|| {
                format!("Failed to serialize output record: {:?}", record)
            })
        })
        .boxed();

    // Write out our output.
    write_output(output_path, output).await?;
    Ok(())
}
