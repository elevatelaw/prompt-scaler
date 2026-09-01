//! OCR engine wrapping a [`RawDriver`] for unstructured text completion.
//!
//! Used for models like GLM-OCR that accept a single image with a fixed text
//! prompt and return raw text (no JSON schema). These are not _really_ LLMs,
//! but they pretend to be LLMs, and they often provide excellent OCR at low
//! parameter counts. See also PaddleOCR, which could likely share much of this.

use std::sync::Arc;

use leaky_bucket::RateLimiter;

use crate::{
    async_utils::JoinWorker, cmd::ocr::OcrOpts, drivers::RawDriver,
    mem_limit::MemLimiter, prelude::*,
};

use super::page::{OcrPageEngine, OcrPageInput, OcrPageOutput, create_rate_limiter};

/// OCR engine wrapping a [`RawDriver`] for raw-text completion requests.
pub struct RawDriverOcrPageEngine {
    /// The raw driver to use for completion.
    driver: Box<dyn RawDriver>,

    /// The fixed prompt text sent with each request.
    prompt_text: String,

    /// A rate limiter to avoid hitting API limits.
    rate_limiter: RateLimiter,

    /// A memory limiter for image loading.
    mem_limiter: MemLimiter,

    /// OCR options (model name, LLM opts, etc.).
    ocr_opts: Arc<OcrOpts>,
}

impl RawDriverOcrPageEngine {
    /// Create a new raw-driver OCR engine.
    #[allow(clippy::new_ret_no_self)]
    pub async fn new(
        concurrency_limit: usize,
        prompt_text: impl Into<String>,
        ocr_opts: Arc<OcrOpts>,
    ) -> Result<(Arc<dyn OcrPageEngine>, JoinWorker)> {
        use crate::drivers::native::NativeDriver;

        let driver = Box::new(NativeDriver::new().await?) as Box<dyn RawDriver>;
        let rate_limiter = create_rate_limiter(concurrency_limit, &ocr_opts.llm_opts);
        let mem_limiter = ocr_opts.llm_opts.mem_limiter();

        Ok((
            Arc::new(Self {
                driver,
                prompt_text: prompt_text.into(),
                rate_limiter,
                mem_limiter,
                ocr_opts,
            }),
            JoinWorker::noop(),
        ))
    }
}

#[async_trait]
impl OcrPageEngine for RawDriverOcrPageEngine {
    #[instrument(level = "debug", skip_all)]
    async fn ocr_page(&self, input: OcrPageInput) -> Result<OcrPageOutput> {
        // Rate limit the request.
        self.rate_limiter.acquire_one().await;

        // Call the raw driver (it loads the image internally).
        let response = self
            .driver
            .raw_completion(
                &self.ocr_opts.model,
                &self.prompt_text,
                &input.image,
                &self.ocr_opts.llm_opts,
                &self.mem_limiter,
            )
            .await;

        match response {
            Ok(crate::drivers::RawCompletionResponse { text, token_usage }) => {
                Ok(OcrPageOutput {
                    text: Some(text),
                    errors: Vec::new(),
                    analysis: None,
                    estimated_cost: None,
                    token_usage,
                })
            }
            Err(e) => Ok(OcrPageOutput {
                text: None,
                errors: vec![format!("Raw driver error: {e}")],
                analysis: None,
                estimated_cost: None,
                token_usage: None,
            }),
        }
    }
}
