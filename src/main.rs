use std::str::FromStr;

use clap::{Parser, Subcommand};
use tracing_subscriber::{
    EnvFilter, Layer as _, filter::Directive, fmt::format::FmtSpan, layer::SubscriberExt,
    util::SubscriberInitExt as _,
};

use self::{prelude::*, ui::Ui};

mod async_utils;
mod cmd;
mod data_url;
mod drivers;
mod litellm;
mod page_iter;
mod prelude;
mod prompt;
mod queues;
mod rate_limit;
mod retry;
mod schema;
mod ui;

/// Run LLM prompts at scale.
#[derive(Debug, Parser)]
#[clap(
    version,
    author,
    after_help = r#"
Environment Variables:
  - OPENAI_API_BASE (optional): Override the server URL.
  - OPENAI_API_KEY: The OpenAI key to use.

  Standard AWS environment variables and credential files
  are used for AWS-based tools like Textract.

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
    Chat(cmd::chat::ChatOpts),
    /// OCR images and PDFs. The input file should have `id` and `path` fields.
    Ocr(cmd::ocr::OcrOpts),
    /// Print schemas for input and output formats.
    Schema(cmd::schema::SchemaOpts),
}

impl Cmd {
    /// Are we using stdout for output?
    fn using_stdout_for_output(&self) -> bool {
        match self {
            Cmd::Chat(opts) => opts.output_path.is_none(),
            Cmd::Ocr(opts) => opts.output_path.is_none(),
            Cmd::Schema(opts) => opts.output_path.is_none(),
        }
    }
}

/// Our entry point, which can return an error. [`anyhow::Result`] will
/// automatically print a nice error message with optional backtrace.
#[tokio::main]
async fn main() -> Result<()> {
    let ui = Ui::init();

    // Initialize tracing.
    let directive =
        Directive::from_str("info").expect("built-in directive should be valid");
    let env_filter = EnvFilter::builder()
        .with_default_directive(directive)
        .from_env_lossy();

    let subscriber = tracing_subscriber::fmt::layer()
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .with_writer(ui.get_stderr_writer())
        .with_filter(env_filter);

    // We can stack multiple layers here if we need to.
    tracing_subscriber::registry().with(subscriber).init();

    // Call our real `main` function now that logging is set up.
    real_main(ui).await
}

/// Our real entry point.
#[instrument(level = "debug", name = "main", skip_all)]
async fn real_main(ui: Ui) -> Result<()> {
    // Load environment variables from a `.env` file, if it exists.
    dotenvy::dotenv().ok();

    // Parse command-line arguments.
    let opts = Opts::parse();
    debug!("Parsed options: {:?}", opts);

    // Hide the progress bar if we're using stdout for output.
    if opts.subcmd.using_stdout_for_output() {
        ui.hide_progress_bars();
    }

    // Run the appropriate subcommand.
    match &opts.subcmd {
        Cmd::Chat(opts) => {
            cmd::chat::cmd_chat(ui, opts).await?;
        }
        Cmd::Ocr(opts) => {
            cmd::ocr::cmd_ocr(ui, opts).await?;
        }
        Cmd::Schema(schema_opts) => {
            cmd::schema::cmd_schema(schema_opts).await?;
        }
    }
    Ok(())
}
