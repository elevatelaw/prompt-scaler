use std::str::FromStr;

use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, filter::Directive, fmt::format::FmtSpan};

use self::{page_iter::PageIterOptions, prelude::*};

mod async_utils;
mod cmd;
mod data_url;
mod llm_client;
mod page_iter;
mod prelude;
mod prompt;
mod queues;
mod retry;
mod schema;

/// Run LLM prompts at scale.
#[derive(Debug, Parser)]
#[clap(
    version,
    author,
    after_help = r#"
Environment Variables:
  - OPENAI_API_BASE (optional): Override the server URL.
  - OPENAI_API_KEY: The OpenAI key to use.

  These variables may be set in a standard `.env` file.
"#
)]
struct Opts {
    #[clap(subcommand)]
    subcmd: Cmd,
}

/// The subcommands we support.
#[derive(Debug, Subcommand)]
enum Cmd {
    /// Prompt using the "/chat/completions" endpoint.
    Chat {
        /// Input data, in CSV or JSONL format. Defaults to standard input.
        input_path: Option<PathBuf>,

        /// Max number of requests to process at a time.
        #[clap(short = 'j', long = "jobs", default_value = "8")]
        job_count: usize,

        /// Model to use by default.
        #[clap(short = 'm', long, default_value = "gpt-4o-mini")]
        model: String,

        /// Prompt, in TOML or JSON format.
        #[clap(short = 'p', long = "prompt")]
        prompt_path: PathBuf,

        /// What portion of inputs should we allow to fail? Specified as a
        /// number between 0.0 and 1.0.
        #[clap(long, default_value = "0.01")]
        allowed_failure_rate: f32,

        /// Output location, in CSV or JSONL format. Defaults to standard output.
        #[clap(short = 'o', long = "out")]
        output_path: Option<PathBuf>,
    },
    /// OCR images and PDFs. The input file should have `id` and `path` fields.
    Ocr {
        /// Input data, in CSV or JSONL format. Defaults to standard input.
        input_path: Option<PathBuf>,

        /// DPI to use for PDF files when converting to images.
        #[clap(flatten)]
        page_iter_opts: PageIterOptions,

        /// Max number of requests to process at a time.
        #[clap(short = 'j', long = "jobs", default_value = "8")]
        job_count: usize,

        /// Model to use by default.
        #[clap(short = 'm', long, default_value = "gemini-2.0-flash")]
        model: String,

        /// Prompt, in TOML or JSON format. The `response_schema` field will be
        /// ignored. Defaults to a generic OCR prompt.
        #[clap(short = 'p', long = "prompt")]
        prompt_path: Option<PathBuf>,

        /// What portion of inputs should we allow to fail? Specified as a
        /// number between 0.0 and 1.0.
        #[clap(long, default_value = "0.01")]
        allowed_failure_rate: f32,

        /// Output location, in CSV or JSONL format. Defaults to standard output.
        #[clap(short = 'o', long = "out")]
        output_path: Option<PathBuf>,
    },
}

/// Our entry point, which can return an error. [`anyhow::Result`] will
/// automatically print a nice error message with optional backtrace.
#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing.
    let directive =
        Directive::from_str("info").expect("built-in directive should be valid");
    let env_filter = EnvFilter::builder()
        .with_default_directive(directive)
        .from_env_lossy();
    tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(env_filter)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .with_writer(std::io::stderr)
        .init();

    // Call our real `main` function now that logging is set up.
    real_main().await
}

/// Our real entry point.
#[instrument(level = "debug", name = "main")]
async fn real_main() -> Result<()> {
    // Load environment variables from a `.env` file, if it exists.
    dotenvy::dotenv().ok();

    // Parse command-line arguments.
    let opts = Opts::parse();
    debug!("Parsed options: {:?}", opts);

    // Run the appropriate subcommand.
    match &opts.subcmd {
        Cmd::Chat {
            input_path,
            job_count,
            model,
            prompt_path,
            allowed_failure_rate,
            output_path,
        } => {
            cmd::chat::cmd_chat(
                input_path.as_deref(),
                *job_count,
                model,
                prompt_path,
                *allowed_failure_rate,
                output_path.as_deref(),
            )
            .await?;
        }
        Cmd::Ocr {
            input_path,
            page_iter_opts,
            job_count,
            model,
            prompt_path,
            allowed_failure_rate,
            output_path,
        } => {
            cmd::ocr::cmd_ocr(
                input_path.as_deref(),
                page_iter_opts,
                *job_count,
                model,
                prompt_path.as_deref(),
                *allowed_failure_rate,
                output_path.as_deref(),
            )
            .await?;
        }
    }
    Ok(())
}
