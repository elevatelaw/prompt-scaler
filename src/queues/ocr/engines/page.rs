//! Interface for OCRing a single page.

use crate::{drivers::TokenUsage, page_iter::Page, prelude::*};

use super::super::OcrAnalysis;

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
