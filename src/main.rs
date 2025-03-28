use std::str::FromStr;

use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, filter::Directive, fmt::format::FmtSpan};

use self::prelude::*;

mod cmd;
mod io;
mod prelude;
mod prompt;

/// Run LLM prompts at scale.
#[derive(Debug, Parser)]
#[clap(version, author)]
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

        /// Prompt, in TOML or JSON format.
        #[clap(short = 'p', long = "prompt")]
        prompt_path: PathBuf,

        /// Output schema, in TOML or JSON format.
        #[clap(short = 's', long = "schema")]
        schema_path: PathBuf,

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
    // Parse command-line arguments.
    let opts = Opts::parse();
    debug!("Parsed options: {:?}", opts);

    // Run the appropriate subcommand.
    match &opts.subcmd {
        Cmd::Chat {
            input_path,
            prompt_path,
            schema_path,
            output_path,
        } => {
            cmd::chat::cmd_chat(
                input_path.as_deref(),
                prompt_path,
                schema_path,
                output_path.as_deref(),
            )
            .await?;
        }
    }
    Ok(())
}
