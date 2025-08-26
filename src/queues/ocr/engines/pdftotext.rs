//! An "OCR" engine that calls `pdftotext`.

use std::{fs::read_to_string, sync::Arc};

use tokio::process::Command;

use crate::{
    async_utils::{JoinWorker, check_for_command_failure},
    page_iter::{PageIterOptions, get_mime_type},
    prelude::*,
    queues::{
        ocr::{OcrInput, OcrOutput, engines::file::OcrFileEngine},
        work::{WorkInput, WorkOutput, WorkStatus},
    },
};

/// "OCR" engine wrapping the `pdftotext` CLI tool from `poppler-utils`.
///
/// This will miss any "non-searchable" text in a PDF, but sometimes you just
/// want cheap and fast.
#[non_exhaustive]
pub struct PdfToTextOcrEngine {
    include_page_breaks: bool,
    page_iter_opts: PageIterOptions,
}

impl PdfToTextOcrEngine {
    /// Create a new `pdftotext` engine.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        include_page_breaks: bool,
        page_iter_opts: &PageIterOptions,
    ) -> Result<(Arc<dyn OcrFileEngine>, JoinWorker)> {
        if page_iter_opts.rasterize {
            Err(anyhow!("pdftotext does not work with --rasterize"))
        } else {
            Ok((
                Arc::new(Self {
                    include_page_breaks,
                    page_iter_opts: page_iter_opts.clone(),
                }),
                JoinWorker::noop(),
            ))
        }
    }
}

#[async_trait]
impl OcrFileEngine for PdfToTextOcrEngine {
    #[instrument(level = "debug", skip_all, fields(id = %ocr_input.id, page = %ocr_input.data.path.display()))]
    async fn ocr_file(
        &self,
        ocr_input: WorkInput<OcrInput>,
    ) -> Result<WorkOutput<OcrOutput>> {
        // Fail all non-PDF files immediately.
        let mime_type = get_mime_type(&ocr_input.data.path)?;
        if mime_type != "application/pdf" {
            return Ok(WorkOutput {
                id: ocr_input.id,
                status: WorkStatus::Failed,
                estimated_cost: None,
                token_usage: None,
                errors: vec!["pdftotext only works with PDFs".to_string()],
                data: OcrOutput {
                    path: ocr_input.data.path.clone(),
                    text: None,
                    page_count: None,
                    analysis: None,
                },
            });
        }

        // Run pdftotext on the input file.
        let tmpdir = tempfile::TempDir::with_prefix("pdftotext")?;
        let output_path = tmpdir.path().join("output.txt");
        let mut cmd = Command::new("pdftotext");
        cmd.arg("-layout")
            .arg(&ocr_input.data.path)
            .arg(&output_path);
        if !self.include_page_breaks {
            cmd.arg("-nopgbrk");
        }
        if let Some(max_pages) = self.page_iter_opts.max_pages {
            // I verified that `-l` does nothing if it's larger than the total
            // number of pages.
            cmd.arg("-l").arg(max_pages.to_string());
        }
        if let Some(password) = &ocr_input.data.password {
            cmd.arg("-upw").arg(password);
        }
        let output = cmd.output().await.context("cannot run pdftotext")?;
        check_for_command_failure("pdftotext", &output, None)?;

        // Read the output file, trimming the final page feed if present for consistency
        // with our other drivers.
        let mut text =
            read_to_string(&output_path).context("cannot read pdftotext output file")?;
        if self.include_page_breaks && text.ends_with("\u{0c}") {
            // Strip only one, in case of empty pages or whatever. I have verified that
            // the page feed is the very last character.
            text = text[..text.len() - 1].to_string();
        }

        let errors = vec![];
        Ok(WorkOutput {
            id: ocr_input.id,
            status: WorkStatus::Ok,
            estimated_cost: None,
            token_usage: None,
            errors,
            data: OcrOutput {
                path: ocr_input.data.path.clone(),
                text: Some(text),
                page_count: None,
                analysis: None,
            },
        })
    }
}
