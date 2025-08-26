//! An OCR engine that splits a document into pages, and OCRs each page.

use std::{env, sync::Arc};

use futures::StreamExt as _;

use super::{
    super::{OcrInput, OcrOutput},
    file::OcrFileEngine,
    page::{OcrPageEngine, OcrPageInput},
};
use crate::{
    async_utils::blocking_iter_streams::BlockingIterStream,
    drivers::TokenUsage,
    page_iter::{PageIter, PageIterOptions},
    prelude::*,
    queues::{
        ocr::OcrAnalysis,
        work::{WorkInput, WorkOutput, WorkStatus},
    },
};

/// An OCR engine that splits a document into pages, and OCRs each page.
pub struct SplitPagesOcrEngine {
    page_iter_opts: PageIterOptions,
    concurrency_limit: usize,
    include_page_breaks: bool,
    engine: Arc<dyn OcrPageEngine>,
}

impl SplitPagesOcrEngine {
    /// Create a new `SplitPagesOcrEngine`.
    pub fn new(
        page_iter_opts: PageIterOptions,
        concurrency_limit: usize,
        include_page_breaks: bool,
        engine: Arc<dyn OcrPageEngine>,
    ) -> Self {
        Self {
            page_iter_opts,
            concurrency_limit,
            include_page_breaks,
            engine,
        }
    }
}

#[async_trait]
impl OcrFileEngine for SplitPagesOcrEngine {
    #[instrument(level = "debug", skip_all, fields(id = %ocr_input.id))]
    async fn ocr_file(
        &self,
        ocr_input: WorkInput<OcrInput>,
    ) -> Result<WorkOutput<OcrOutput>> {
        let id = ocr_input.id.clone();

        // Create a page stream, using BlockingIterStream to avoid blocking the
        // async executor with slow PDF processing.
        let page_iter = PageIter::from_path(
            &ocr_input.data.path,
            &self.page_iter_opts,
            ocr_input.data.password.as_deref(),
        )
        .await
        .with_context(|| {
            format!("Failed to separate {:?} into pages", ocr_input.data.path)
        })?;
        let check_complete_result = page_iter.check_complete();
        let warnings = page_iter.warnings().to_owned();
        let page_stream = BlockingIterStream::new(page_iter);

        let page_outputs = page_stream
            .enumerate()
            .map(move |(page_idx, page)| {
                let id = ocr_input.id.clone();
                let engine = self.engine.clone();
                async move {
                    let page = page?;
                    engine.ocr_page(OcrPageInput { id, page_idx, page }).await
                }
            })
            // Process all the pages concurrently, up to the concurrency limit.
            .buffered(self.concurrency_limit)
            // Collect all the results (including fatal errors) into a single vector.
            .collect::<Vec<_>>()
            .await
            // Convert from `Vec<Result<ChatOutput>>` to `Result<Vec<ChatOutput>>`,
            // and exit early if we have any fatal errors.
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        // Turn our `ChatResponse`s into a `PdfOutput` record.
        let mut errors = vec![];
        errors.extend(warnings);
        let mut pages = vec![];
        let mut analysis = OcrAnalysis::default();
        let mut analysis_present = false;
        let mut estimated_cost = 0.0;
        let mut token_usage = TokenUsage::default();
        for page_output in page_outputs {
            errors.extend(page_output.errors);
            if let Some(text) = page_output.text {
                pages.push(Some(text));
                if let Some(a) = page_output.analysis {
                    analysis = analysis.merge(&a);
                    analysis_present = true;
                }
                if let Some(cost) = page_output.estimated_cost {
                    estimated_cost += cost;
                }
                if let Some(usage) = page_output.token_usage {
                    token_usage += usage;
                }
            } else {
                // No LLM response.
                pages.push(None);
            }
        }
        if let Err(err) = &check_complete_result {
            errors.push(err.to_string());
        }

        // Decide how to represent page breaks.
        let page_break = if self.include_page_breaks {
            "\n\x0C\n"
        } else {
            "\n\n"
        };

        let good_page_count = pages.iter().filter(|p| p.is_some()).count();
        let total_page_count = pages.len();
        let text = pages
            .into_iter()
            .map(|p| p.unwrap_or_else(|| "**COULD_NOT_OCR_PAGE**".to_owned()))
            .collect::<Vec<String>>()
            .join(page_break);
        Ok(WorkOutput {
            id,
            status: if check_complete_result.is_ok()
                && good_page_count == total_page_count
            {
                WorkStatus::Ok
            } else if good_page_count > 0 {
                WorkStatus::Incomplete
            } else {
                WorkStatus::Failed
            },
            errors,
            estimated_cost: if estimated_cost > 0.0 {
                Some(estimated_cost)
            } else {
                None
            },
            token_usage: if token_usage.is_zero() {
                None
            } else {
                Some(token_usage)
            },
            data: OcrOutput {
                path: ocr_input.data.path,
                text: if good_page_count > 0 {
                    Some(text)
                } else {
                    None
                },
                page_count: Some(total_page_count),
                analysis: if analysis_present
                    && env::var("EXPERIMENTAL_OCR_ANALYSIS").is_ok()
                {
                    Some(analysis)
                } else {
                    None
                },
            },
        })
    }
}
