//! The `schema` subcommand.

use clap::{Args, ValueEnum};
use schemars::{
    schema::{Metadata, RootSchema},
    schema_for,
};
use tokio::io::AsyncWriteExt as _;

use crate::{
    async_utils::io::create_writer,
    prelude::*,
    prompt::ChatPrompt,
    queues::{
        chat::{ChatInput, ChatOutput},
        ocr::{OcrInput, OcrOutput},
        work::{WorkInput, WorkOutput},
    },
};

/// The different schema types we support.
///
/// We parse these as PascalCase, because they represent type names.
#[derive(Debug, Clone, Copy, ValueEnum)]
#[clap(rename_all = "PascalCase")]
pub enum SchemaType {
    /// Chat input.
    ChatInput,
    /// Chat output.
    ChatOutput,
    /// Chat prompt.
    ChatPrompt,
    /// OCR input.
    OcrInput,
    /// OCR output.
    OcrOutput,
}

/// Schema command line arguments.
#[derive(Debug, Args)]
pub struct SchemaOpts {
    /// The schema type to generate.
    #[clap(value_enum, value_name = "TYPE")]
    pub schema_type: SchemaType,

    /// The output path to write the schema to.
    #[clap(short = 'o', long = "out")]
    pub output_path: Option<PathBuf>,
}

/// The `schema` subcommand.
#[instrument(level = "debug", skip_all)]
pub async fn cmd_schema(schema_opts: &SchemaOpts) -> Result<()> {
    // Get our schema.
    let schema = match schema_opts.schema_type {
        SchemaType::ChatInput => {
            schema_for!(WorkInput<ChatInput>).with_title("ChatInput")
        }
        SchemaType::ChatOutput => {
            schema_for!(WorkOutput<ChatOutput>).with_title("ChatOutput")
        }
        SchemaType::ChatPrompt => schema_for!(ChatPrompt).with_title("ChatPrompt"),
        SchemaType::OcrInput => schema_for!(WorkInput<OcrInput>).with_title("OcrInput"),
        SchemaType::OcrOutput => {
            schema_for!(WorkOutput<OcrOutput>).with_title("OcrOutput")
        }
    };

    // Write out our schema.
    let mut wtr = create_writer(schema_opts.output_path.as_deref()).await?;
    let schema_str =
        serde_json::to_string_pretty(&schema).context("failed to serialize schema")?;
    wtr.write_all(schema_str.as_bytes())
        .await
        .context("failed to write schema")?;
    wtr.flush().await.context("failed to flush schema")?;
    Ok(())
}

/// Extensions for setting the title of a root schema.
trait WithTitle {
    /// Set the title of the schema.
    fn with_title(self, title: &str) -> Self;
}

impl WithTitle for RootSchema {
    fn with_title(mut self, title: &str) -> Self {
        match &mut self.schema.metadata {
            Some(metadata) => metadata.title = Some(title.to_string()),
            None => {
                self.schema.metadata = Some(Box::new(Metadata {
                    title: Some(title.to_string()),
                    ..Default::default()
                }))
            }
        }
        self
    }
}
