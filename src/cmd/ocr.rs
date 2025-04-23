//! The `ocr` subcommand.

use clap::Args;
use futures::StreamExt;

use crate::{
    async_utils::io::read_json_or_toml,
    page_iter::PageIterOptions,
    prelude::*,
    prompt::ChatPrompt,
    queues::{
        ocr::{
            OcrInput, OcrOutput, OcrStreamInfo, engines::llm::default_ocr_prompt,
            ocr_files,
        },
        work::{WorkInput as _, WorkOutput as _},
    },
    ui::{ProgressConfig, Ui},
};

/// Command line arguments for the `ocr` subcommand.
#[derive(Debug, Args)]
pub struct OcrOpts {
    /// Input data, in CSV or JSONL format. Defaults to standard input.
    pub input_path: Option<PathBuf>,

    /// DPI to use for PDF files when converting to images.
    #[clap(flatten)]
    pub page_iter_opts: PageIterOptions,

    /// Model to use by default.
    #[clap(short = 'm', long, default_value = "gemini-2.0-flash")]
    pub model: String,

    /// Prompt, in TOML or JSON format. The `response_schema` field will be
    /// ignored. Defaults to a generic OCR prompt.
    #[clap(short = 'p', long = "prompt")]
    pub prompt_path: Option<PathBuf>,

    /// Output location, in CSV or JSONL format. Defaults to standard output.
    #[clap(short = 'o', long = "out")]
    pub output_path: Option<PathBuf>,

    /// Stream-related options.
    #[clap(flatten)]
    pub stream_opts: super::StreamOpts,
}

/// The `ocr` subcommand.
#[instrument(level = "debug", skip_all)]
#[allow(clippy::too_many_arguments)]
pub async fn cmd_ocr(ui: Ui, opts: &OcrOpts) -> Result<()> {
    // Get our OCR prompt.
    let prompt = match opts.prompt_path.as_deref() {
        Some(path) => read_json_or_toml::<ChatPrompt>(path).await?,
        None => default_ocr_prompt(),
    };

    // Open up our input stream and parse into records.
    let input = OcrInput::read_stream(ui.clone(), opts.input_path.as_deref()).await?;
    let input = opts.stream_opts.apply_stream_input_opts(input);

    // Configure our progress bar.
    let pb = ui.new_from_size_hint(
        &ProgressConfig {
            emoji: "📄",
            msg: "OCRing files",
            done_msg: "OCRed files",
        },
        input.size_hint(),
    );

    let OcrStreamInfo { stream, worker } = ocr_files(
        input,
        opts.page_iter_opts.to_owned(),
        opts.stream_opts.job_count,
        prompt,
        opts.model.to_owned(),
    )
    .await?;
    let output = pb
        .wrap_stream(stream.buffered(opts.stream_opts.job_count))
        .boxed();

    OcrOutput::write_stream(
        &ui,
        opts.output_path.as_deref(),
        output,
        opts.stream_opts.allowed_failure_rate,
    )
    .await?;

    worker.join().await
}
