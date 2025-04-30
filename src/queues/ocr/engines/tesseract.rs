//! Tesseract OCR engine.

//! An "OCR" engine that calls `pdftotext`.

use std::{
    fs::{File, read_to_string},
    io::Write as _,
    sync::Arc,
};

use tokio::process::Command;

use crate::{
    async_utils::{JoinWorker, check_for_command_failure},
    page_iter::PageIterOptions,
    prelude::*,
};

use super::{OcrEngine, OcrPageInput, OcrPageOutput};

/// OCR engine wrapping the `tesseract` CLI tool.
#[non_exhaustive]
pub struct TesseractOcrEngine {}

impl TesseractOcrEngine {
    /// Create a new `tesseract` engine.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        page_iter_opts: &PageIterOptions,
    ) -> Result<(Arc<dyn OcrEngine>, JoinWorker)> {
        if page_iter_opts.rasterize {
            Ok((Arc::new(Self {}), JoinWorker::noop()))
        } else {
            Err(anyhow!("tesseract requires --rasterize"))
        }
    }
}

#[async_trait]
impl OcrEngine for TesseractOcrEngine {
    #[instrument(level = "debug", skip_all, fields(id = %input.id, page = %input.page_idx))]
    async fn ocr_page(&self, input: OcrPageInput) -> Result<OcrPageOutput> {
        let extension = mime_guess::get_mime_extensions_str(&input.page.mime_type)
            .and_then(|o| o.first())
            .ok_or_else(|| {
                anyhow!("cannot determine extension for {}", input.page.mime_type)
            })?;

        // Write our input to a temporary file.
        let tmpdir = tempfile::TempDir::with_prefix("tesseract")?;
        let input_path = tmpdir.path().join(format!("input.{}", extension));
        let output_path = tmpdir.path().join("output.txt");
        let mut input_file =
            File::create(&input_path).context("cannot create tesseract input file")?;
        input_file
            .write_all(&input.page.data)
            .context("cannot write tesseract input file")?;
        input_file
            .flush()
            .context("cannot flush tesseract input file")?;

        // Run tesseract on the input file.
        let output = Command::new("tesseract")
            .arg(input_path)
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
