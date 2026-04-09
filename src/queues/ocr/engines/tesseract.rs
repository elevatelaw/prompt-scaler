//! Tesseract OCR engine.

use std::{fs::read_to_string, sync::Arc};

use tokio::process::Command;

use crate::{
    async_utils::{JoinWorker, check_for_command_failure},
    cmd::ocr::OcrOpts,
    prelude::*,
};

use super::page::{OcrPageEngine, OcrPageInput, OcrPageOutput};

/// OCR engine wrapping the `tesseract` CLI tool.
#[non_exhaustive]
pub struct TesseractOcrPageEngine {}

impl TesseractOcrPageEngine {
    /// Create a new `tesseract` engine.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(ocr_opts: Arc<OcrOpts>) -> Result<(Arc<dyn OcrPageEngine>, JoinWorker)> {
        if ocr_opts.page_iter_opts.rasterize {
            Ok((Arc::new(Self {}), JoinWorker::noop()))
        } else {
            Err(anyhow!("tesseract requires --rasterize"))
        }
    }
}

#[async_trait]
impl OcrPageEngine for TesseractOcrPageEngine {
    #[instrument(level = "debug", skip_all, fields(id = %input.id, page = %input.page_idx))]
    async fn ocr_page(&self, input: OcrPageInput) -> Result<OcrPageOutput> {
        // Use the image file path directly — no need to load into memory.
        let output_dir = tempfile::TempDir::with_prefix("tesseract")?;
        let output_path = output_dir.path().join("output.txt");

        // Run tesseract on the image file.
        let output = Command::new("tesseract")
            .arg(&input.image.path)
            .arg(output_path.with_extension(""))
            .output()
            .await
            .context("cannot run tesseract")?;
        check_for_command_failure("tesseract", &output, None)?;

        // Read the output file.
        let text =
            read_to_string(&output_path).context("cannot read tesseract output file")?;
        let errors = vec![];
        Ok(OcrPageOutput {
            text: Some(text),
            errors,
            analysis: None,
            estimated_cost: None,
            token_usage: None,
        })
    }
}
