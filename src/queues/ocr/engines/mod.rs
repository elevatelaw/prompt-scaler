//! OCR engine interface.

use std::sync::Arc;

use crate::{async_utils::JoinWorker, cmd::ocr::OcrOpts, prelude::*, prompt::ChatPrompt};

use self::{file::OcrFileEngine, split_pages::SplitPagesOcrEngine};

pub mod file;
pub mod llm;
pub mod page;
pub mod pdftotext;
pub mod raw;
pub mod split_pages;
pub mod tesseract;
pub mod textract;

/// Get the OCR engine for the specified model.
///
/// For non-LLM models, `prompt` will be ignored.
pub async fn ocr_engine_for_model(
    concurrency_limit: usize,
    prompt: ChatPrompt,
    ocr_opts: Arc<OcrOpts>,
) -> Result<(Arc<dyn OcrFileEngine>, JoinWorker)> {
    // Helper function to wrap an OcrPageEngine in a SplitPagesOcrEngine.
    let ocr_opts_clone = ocr_opts.clone();
    let split_pages = |(page_engine, worker)| {
        (
            Arc::new(SplitPagesOcrEngine::new(
                concurrency_limit,
                page_engine,
                ocr_opts_clone,
            )) as Arc<dyn OcrFileEngine>,
            worker,
        )
    };

    // Choose our engine.
    let (file_engine, worker) = match ocr_opts.model.as_str() {
        "pdftotext" => pdftotext::PdfToTextOcrFileEngine::new(ocr_opts)?,
        "tesseract" => split_pages(tesseract::TesseractOcrPageEngine::new(ocr_opts)?),
        "textract" => split_pages(
            textract::TextractOcrPageEngine::new(concurrency_limit, ocr_opts).await?,
        ),
        "textract-async" => {
            textract::TextractOcrFileEngine::new(concurrency_limit, ocr_opts).await?
        }
        // KLUDGE: There's a class of specialized OCR models that pretend to be
        // LLMs but which need special drivers with fixed prompts. We recognize
        // these by looking for known strings in their model names, at least for
        // now. We may regret this later. We only plan to handle a small number
        // of open models with very good benchmark scores.
        //
        // Note that running these models standalone _without_ a page segmentation
        // algorithm produces mediocre results.
        model_name if model_name.to_lowercase().contains("glm-ocr") => split_pages(
            raw::RawDriverOcrPageEngine::new(
                concurrency_limit,
                "Text Recognition:",
                ocr_opts,
            )
            .await?,
        ),
        // Assume all other OCR models are LLMs.
        _ => split_pages(
            llm::LlmOcrPageEngine::new(concurrency_limit, prompt, ocr_opts).await?,
        ),
    };
    Ok((file_engine, worker))
}
