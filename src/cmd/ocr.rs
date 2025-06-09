//! The `ocr` subcommand.

use std::ffi::OsStr;

use clap::Args;
use futures::StreamExt;

use crate::{
    async_utils::io::read_json_or_toml,
    drivers::LlmOpts,
    page_iter::PageIterOptions,
    prelude::*,
    prompt::ChatPrompt,
    queues::{
        ocr::{
            OcrInput, OcrOutput, OcrStreamInfo, engines::llm::default_ocr_prompt,
            ocr_files,
        },
        work::{WorkInput, WorkOutput},
    },
    ui::{ProgressConfig, Ui},
};

/// Command line arguments for the `ocr` subcommand.
#[derive(Debug, Args)]
pub struct OcrOpts {
    /// Input data, in CSV or JSONL format. Defaults to standard input.
    pub input_path: Option<PathBuf>,

    /// Model to use by default.
    #[clap(short = 'm', long, default_value = "gemini-2.0-flash")]
    pub model: String,

    /// Prompt, in TOML or JSON format. The `response_schema` field will be
    /// ignored. Defaults to a generic OCR prompt.
    #[clap(short = 'p', long = "prompt")]
    pub prompt_path: Option<PathBuf>,

    /// Output location, in CSV or JSONL format. Defaults to standard output and
    /// JSONL.
    #[clap(short = 'o', long = "out")]
    pub output_path: Option<PathBuf>,

    /// Should we include page breaks (^L) in the output text?
    #[clap(long)]
    pub include_page_breaks: bool,

    /// Stream-related options.
    #[clap(flatten)]
    pub stream_opts: super::StreamOpts,

    /// DPI to use for PDF files when converting to images.
    #[clap(flatten)]
    pub page_iter_opts: PageIterOptions,

    /// Our LLM options.
    #[clap(flatten)]
    pub llm_opts: LlmOpts,
}

/// The `ocr` subcommand.
#[instrument(level = "debug", skip_all)]
#[allow(clippy::too_many_arguments)]
pub async fn cmd_ocr(ui: &Ui, opts: &OcrOpts) -> Result<()> {
    // Get our OCR prompt.
    let prompt = match opts.prompt_path.as_deref() {
        Some(path) => read_json_or_toml::<ChatPrompt>(path).await?,
        None => default_ocr_prompt(),
    };

    // Open up our input stream and parse into records.
    let input =
        WorkInput::<OcrInput>::read_stream(ui.clone(), opts.input_path.as_deref())
            .await?;
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
        opts.stream_opts.job_count,
        prompt,
        opts.model.to_owned(),
        opts.include_page_breaks,
        opts.page_iter_opts.to_owned(),
        opts.llm_opts.to_owned(),
    )
    .await?;
    let output = pb
        .wrap_stream(opts.stream_opts.apply_stream_buffering_opts(stream))
        .boxed();

    match opts.output_path.as_deref() {
        Some(path) if path.extension() == Some(OsStr::new("csv")) => {
            WorkOutput::<OcrOutput>::write_stream_to_csv(
                ui,
                opts.output_path.as_deref(),
                output,
                &opts.stream_opts,
            )
            .await?;
        }
        _ => {
            WorkOutput::write_stream(
                ui,
                opts.output_path.as_deref(),
                output,
                &opts.stream_opts,
            )
            .await?;
        }
    }
    worker.join().await
}
