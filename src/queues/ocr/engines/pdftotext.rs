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

/// "OCR" engine wrapping the `pdftotext` CLI tool from `poppler-utils`.
///
/// This will miss any "non-searchable" text in a PDF, but sometimes you just
/// want cheap and fast.
#[non_exhaustive]
pub struct PdfToTextOcrEngine {}

impl PdfToTextOcrEngine {
    /// Create a new `pdftotext` engine.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        page_iter_opts: &PageIterOptions,
    ) -> Result<(Arc<dyn OcrEngine>, JoinWorker)> {
        if page_iter_opts.rasterize {
            Err(anyhow!("pdftotext does not work with --rasterize"))
        } else {
            Ok((Arc::new(Self {}), JoinWorker::noop()))
        }
    }
}

#[async_trait]
impl OcrEngine for PdfToTextOcrEngine {
    #[instrument(level = "debug", skip_all, fields(id = %input.id, page = %input.page_idx))]
    async fn ocr_page(&self, input: OcrPageInput) -> Result<OcrPageOutput> {
        // Fail all non-PDF files immediately.
        if input.page.mime_type != "application/pdf" {
            return Ok(OcrPageOutput {
                text: None,
                errors: vec!["pdftotext only works with PDFs".to_string()],
            });
        }

        // Write our input to a temporary file.
        let tmpdir = tempfile::TempDir::with_prefix("pdftotext")?;
        let input_path = tmpdir.path().join("input.pdf");
        let output_path = tmpdir.path().join("output.txt");
        let mut input_file =
            File::create(&input_path).context("cannot create pdftotext input file")?;
        input_file
            .write_all(&input.page.data)
            .context("cannot write pdftotext input file")?;
        input_file
            .flush()
            .context("cannot flush pdftotext input file")?;

        // Run pdftotext on the input file.
        let status = Command::new("pdftotext")
            .arg("-layout")
            .arg(input_path)
            .arg(&output_path)
            .status()
            .await
            .context("cannot run pdftotext")?;
        check_for_command_failure("pdftotext", status)?;

        // Read the output file.
        let text =
            read_to_string(&output_path).context("cannot read pdftotext output file")?;
        let errors = vec![];
        Ok(OcrPageOutput {
            text: Some(text),
            errors,
        })
    }
}
