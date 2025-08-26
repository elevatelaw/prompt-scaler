//! Interface for OCRing a document.
//!
//! Some OCR work better on entire documents. Other OCR engines work better one
//! page at a time. This file contains a trait [`OcrFileEngine`], and an
//! implementation [`SplitPagesOcrEngine`].

use super::super::{OcrInput, OcrOutput};
use crate::{
    prelude::*,
    queues::work::{WorkInput, WorkOutput},
};

/// Interface for OCRing a document.
#[async_trait]
pub trait OcrFileEngine: Send + Sync + 'static {
    /// Process a PDF file and extract text from it.
    async fn ocr_file(
        &self,
        ocr_input: WorkInput<OcrInput>,
    ) -> Result<WorkOutput<OcrOutput>>;
}
