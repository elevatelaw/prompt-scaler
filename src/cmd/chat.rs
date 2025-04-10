//! The `chat` subcommand.

use futures::StreamExt;

use crate::{
    async_utils::io::read_json_or_toml,
    prelude::*,
    prompt::ChatPrompt,
    queues::{
        chat::{ChatInput, ChatOutput, ChatStreamInfo, process_chat_stream},
        work::{WorkInput as _, WorkOutput as _},
    },
};

/// Run the `chat` subcommand.
#[instrument(level = "debug", skip_all)]
pub async fn cmd_chat(
    input_path: Option<&Path>,
    job_count: usize,
    model: &str,
    prompt_path: &Path,
    allowed_failure_rate: f32,
    output_path: Option<&Path>,
) -> Result<()> {
    // Open up our input stream and convert to records.
    let input = ChatInput::read_stream(input_path).await?;

    // Read our prompt.
    let prompt = read_json_or_toml::<ChatPrompt>(prompt_path).await?;

    // Build our chat stream.
    let ChatStreamInfo {
        stream: futures,
        queue,
    } = process_chat_stream(job_count, input, prompt, model.to_owned()).await?;

    // Resolve our individual LLM requests concurrently, and convert them back to JSON.
    let output = futures.buffered(job_count).boxed();

    // Write out our output.
    ChatOutput::write_stream(output_path, output, allowed_failure_rate).await?;

    // Wait for our work queue's background task to exit.
    queue.close().await
}
