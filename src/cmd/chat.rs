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
    ui::{ProgressConfig, Ui},
};

/// Run the `chat` subcommand.
#[instrument(level = "debug", skip_all)]
pub async fn cmd_chat(
    ui: Ui,
    input_path: Option<&Path>,
    job_count: usize,
    model: &str,
    prompt_path: &Path,
    allowed_failure_rate: f32,
    output_path: Option<&Path>,
) -> Result<()> {
    // Open up our input stream and convert to records.
    let input = ChatInput::read_stream(ui.clone(), input_path).await?;

    // Read our prompt.
    let prompt = read_json_or_toml::<ChatPrompt>(prompt_path).await?;

    // Configure our progress bar.
    let pb = ui.new_from_size_hint(
        &ProgressConfig {
            emoji: "💬",
            msg: "Running LLM prompts",
            done_msg: "Ran LLM prompts",
        },
        input.size_hint(),
    );

    // Build our chat stream.
    let ChatStreamInfo {
        stream: futures,
        worker,
    } = process_chat_stream(job_count, input, prompt, model.to_owned()).await?;

    // Resolve our individual LLM requests concurrently, and convert them back to JSON.
    let output = pb.wrap_stream(futures.buffered(job_count)).boxed();

    // Write out our output.
    ChatOutput::write_stream(output_path, output, allowed_failure_rate).await?;

    // Wait for our work queue's background task to exit.
    worker.join().await
}
