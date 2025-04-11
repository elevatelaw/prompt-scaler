//! OCR engine interface.

pub mod llm;
pub mod pdftotext;
//pub mod tesseract;

use std::sync::Arc;

use crate::{
    async_utils::JoinWorker,
    page_iter::{Page, PageIterOptions},
    prelude::*,
    prompt::ChatPrompt,
};

/// Input record describing a file to OCR.
pub struct OcrPageInput {
    /// The ID of the document.
    pub id: Value,

    /// The index of the page within the document.
    pub page_idx: usize,

    /// The page to OCR.
    pub page: Page,
}

/// Output record describing the result of OCRing a page.
pub struct OcrPageOutput {
    /// The text, if the OCR succeeded for this page.
    pub text: Option<String>,

    /// Any errors that occurred during OCR.
    pub errors: Vec<String>,
}

/// Interface to an OCR engine.
#[async_trait]
pub trait OcrEngine: Send + Sync + 'static {
    /// OCR a single page.
    async fn ocr_page(&self, input: OcrPageInput) -> Result<OcrPageOutput>;
}

/// Get the OCR engine for the specified model.
///
/// For non-LLM models, `prompt` will be ignored.
pub async fn ocr_engine_for_model(
    concurrency_limit: usize,
    prompt: ChatPrompt,
    model: String,
    page_iter_opts: &PageIterOptions,
) -> Result<(Arc<dyn OcrEngine>, JoinWorker)> {
    let (ocr_engine, worker) = match model.as_str() {
        "pdftotext" => pdftotext::PdfToTextOcrEngine::new(page_iter_opts)?,
        _ => llm::LlmOcrEngine::new(concurrency_limit, prompt, model).await?,
    };
    Ok((ocr_engine, worker))
}
