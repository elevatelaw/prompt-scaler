//! Interface for OCRing a single page.

use leaky_bucket::RateLimiter;

use crate::{
    drivers::{LlmOpts, TokenUsage},
    images::ImageFile,
    prelude::*,
    rate_limit::{RateLimit, RateLimitPeriod},
};

use super::super::OcrAnalysis;

/// Input record describing a file to OCR.
pub struct OcrPageInput {
    /// The ID of the document.
    pub id: Value,

    /// The index of the page within the document.
    pub page_idx: usize,

    /// The image file to OCR.
    pub image: ImageFile,
}

/// Output record describing the result of OCRing a page.
pub struct OcrPageOutput {
    /// The text, if the OCR succeeded for this page.
    pub text: Option<String>,

    /// Any errors that occurred during OCR.
    pub errors: Vec<String>,

    /// Any defects in the page that make it difficult to OCR.
    pub analysis: Option<OcrAnalysis>,

    /// How much do we think we spent on this page?
    pub estimated_cost: Option<f64>,

    /// How many tokens did the LLM use?
    pub token_usage: Option<TokenUsage>,
}

/// Interface to an OCR engine.
#[async_trait]
pub trait OcrPageEngine: Send + Sync + 'static {
    /// OCR a single page.
    async fn ocr_page(&self, input: OcrPageInput) -> Result<OcrPageOutput>;
}

/// Create a rate limiter, defaulting to concurrency-limit requests per second.
///
/// Shared by non-chat OcrPageEngine implementations (textract, raw driver, etc.)
/// that need per-request rate limiting but don't go through the chat work queue.
pub fn create_rate_limiter(concurrency_limit: usize, llm_opts: &LlmOpts) -> RateLimiter {
    // If we don't have a rate limit, set one based on the concurrency limit.
    //
    // TODO: FUTURE BREAKING: We may want to remove the default rate limit, but
    // that would be a breaking change.
    let rate_limit = llm_opts
        .rate_limit
        .clone()
        .unwrap_or_else(|| RateLimit::new(concurrency_limit, RateLimitPeriod::Second));
    rate_limit.to_rate_limiter()
}
