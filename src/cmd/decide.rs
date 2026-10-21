//! The `decide` subcommand.

use clap::Args;
use futures::StreamExt;

use crate::{
    async_utils::io::read_json,
    drivers::decide::DecideDriverOpts,
    mem_limit::MemLimiter,
    prelude::*,
    prompts::decide::DecidePrompt,
    queues::{
        decide::{DecideInput, DecideStreamInfo, process_decide_stream},
        work::{WorkInput, WorkOutput},
    },
    ui::{ProgressConfig, Ui},
};

/// Decide command line arguments.
#[derive(Debug, Args)]
pub struct DecideOpts {
    /// Input data, in CSV or JSONL format. Defaults to standard input.
    pub input_path: Option<PathBuf>,

    /// Model to ask. Decision servers usually take this per-request, so this
    /// also tells a router which model to load.
    #[clap(short = 'm', long)]
    pub model: String,

    /// Prompt, in JSON format. TOML prompts are not supported for `decide`.
    #[clap(short = 'p', long = "prompt")]
    pub prompt_path: PathBuf,

    /// Output location, in JSONL format. Defaults to standard output.
    #[clap(short = 'o', long = "out")]
    pub output_path: Option<PathBuf>,

    /// Stream-related options.
    #[clap(flatten)]
    pub stream_opts: super::StreamOpts,

    /// Our decide driver options.
    #[clap(flatten)]
    pub decide_opts: DecideDriverOpts,
}

/// Run the `decide` subcommand.
#[instrument(level = "debug", skip_all)]
pub async fn cmd_decide(ui: &Ui, opts: &mut DecideOpts) -> Result<()> {
    // Compute our canonical base directory for relative paths in input and prompts.
    opts.decide_opts
        .driver_opts
        .compute_canonical_base_dir(opts.input_path.as_deref())?;

    // Open up our input stream and convert to records.
    let input =
        WorkInput::<DecideInput>::read_stream(ui.clone(), opts.input_path.as_deref())
            .await?;
    let input = opts.stream_opts.apply_stream_input_opts(input);

    // Read our prompt. Prompts are JSON only: unlike `ChatPrompt`,
    // `DecidePrompt` has no `toml_span::Deserialize` impl.
    let prompt = read_json::<DecidePrompt>(opts.prompt_path.as_ref()).await?;

    // Configure our progress bar.
    let pb = ui.new_from_size_hint(
        &ProgressConfig {
            emoji: "🧠",
            msg: "Running decision prompts",
            done_msg: "Ran decision prompts",
        },
        input.size_hint(),
    );

    // Build our decide stream.
    let DecideStreamInfo {
        stream: futures,
        worker,
    } = process_decide_stream(
        opts.stream_opts.job_count,
        input,
        prompt,
        opts.model.to_owned(),
        opts.decide_opts.to_owned(),
        // Decision models answer from text state only for now, so no image
        // memory is held while a request is in flight. Item 13 (image support)
        // is where this needs the same treatment as `chat`: see the note on
        // `MemLimiter::unlimited()` in `cmd_chat`.
        MemLimiter::unlimited(),
    )
    .await?;

    // Resolve our individual decision requests concurrently, and convert them
    // back to JSON.
    let output = pb
        .wrap_stream(opts.stream_opts.apply_stream_buffering_opts(futures))
        .boxed();

    // Write out our output.
    WorkOutput::write_stream(ui, opts.output_path.as_deref(), output, &opts.stream_opts)
        .await?;

    // Wait for our work queue's background task to exit.
    worker.join().await
}
