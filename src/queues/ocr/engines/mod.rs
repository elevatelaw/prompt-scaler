//! OCR engine interface.

use std::sync::Arc;

use crate::{
    async_utils::JoinWorker, drivers::LlmOpts, page_iter::PageIterOptions, prelude::*,
    prompt::ChatPrompt,
};

use self::{file::OcrFileEngine, split_pages::SplitPagesOcrEngine};

pub mod file;
pub mod llm;
pub mod page;
pub mod pdftotext;
pub mod split_pages;
pub mod tesseract;
pub mod textract;

/// Get the OCR engine for the specified model.
///
/// For non-LLM models, `prompt` will be ignored.
pub async fn ocr_engine_for_model(
    concurrency_limit: usize,
    prompt: ChatPrompt,
    model: String,
    include_page_breaks: bool,
    page_iter_opts: &PageIterOptions,
    llm_opts: LlmOpts,
) -> Result<(Arc<dyn OcrFileEngine>, JoinWorker)> {
    // Helper function wrap an OcrPageEngine.
    let split_pages = |(page_engine, worker)| {
        (
            Arc::new(SplitPagesOcrEngine::new(
                page_iter_opts.clone(),
                concurrency_limit,
                include_page_breaks,
                page_engine,
            )) as Arc<dyn OcrFileEngine>,
            worker,
        )
    };

    // Choose our engine.
    let (file_engine, worker) = match model.as_str() {
        "pdftotext" => {
            pdftotext::PdfToTextOcrFileEngine::new(include_page_breaks, page_iter_opts)?
        }
        "tesseract" => {
            split_pages(tesseract::TesseractOcrPageEngine::new(page_iter_opts)?)
        }
        "textract" => split_pages(
            textract::TextractOcrPageEngine::new(concurrency_limit, &llm_opts).await?,
        ),
        // Assume all other OCR models are LLMs.
        _ => split_pages(
            llm::LlmOcrPageEngine::new(concurrency_limit, prompt, model, llm_opts)
                .await?,
        ),
    };
    Ok((file_engine, worker))
}
