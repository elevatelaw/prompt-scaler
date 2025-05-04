//! The `chat` subcommand.

use clap::Args;
use futures::StreamExt;

use crate::{
    async_utils::io::read_json_or_toml,
    drivers::LlmOpts,
    prelude::*,
    prompt::ChatPrompt,
    queues::{
        chat::{ChatInput, ChatStreamInfo, process_chat_stream},
        work::{WorkInput, WorkOutput},
    },
    ui::{ProgressConfig, Ui},
};

/// Chat command line arguments.
#[derive(Debug, Args)]
pub struct ChatOpts {
    /// Input data, in CSV or JSONL format. Defaults to standard input.
    pub input_path: Option<PathBuf>,

    /// Model to use by default.
    #[clap(short = 'm', long, default_value = "gpt-4o-mini")]
    pub model: String,

    /// Prompt, in TOML or JSON format.
    #[clap(short = 'p', long = "prompt")]
    pub prompt_path: PathBuf,

    /// Output location, in JSONL format. Defaults to standard output.
    #[clap(short = 'o', long = "out")]
    pub output_path: Option<PathBuf>,

    /// Stream-related options.
    #[clap(flatten)]
    pub stream_opts: super::StreamOpts,

    /// Our LLM options.
    #[clap(flatten)]
    pub llm_opts: LlmOpts,
}

/// Run the `chat` subcommand.
#[instrument(level = "debug", skip_all)]
pub async fn cmd_chat(ui: &Ui, opts: &ChatOpts) -> Result<()> {
    // Open up our input stream and convert to records.
    let input =
        WorkInput::<ChatInput>::read_stream(ui.clone(), opts.input_path.as_deref())
            .await?;
    let input = opts.stream_opts.apply_stream_input_opts(input);

    // Read our prompt.
    let prompt = read_json_or_toml::<ChatPrompt>(opts.prompt_path.as_ref()).await?;

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
    } = process_chat_stream(
        opts.stream_opts.job_count,
        input,
        prompt,
        opts.model.to_owned(),
        opts.llm_opts.to_owned(),
    )
    .await?;

    // Resolve our individual LLM requests concurrently, and convert them back to JSON.
    let output = pb
        .wrap_stream(opts.stream_opts.apply_stream_buffering_opts(futures))
        .boxed();

    // Write out our output.
    WorkOutput::write_stream(ui, opts.output_path.as_deref(), output, &opts.stream_opts)
        .await?;

    // Wait for our work queue's background task to exit.
    worker.join().await
}
