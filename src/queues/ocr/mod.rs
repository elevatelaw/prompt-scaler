//! PDF utilities.

pub mod engines;

use std::{env, sync::Arc, vec};

use engines::ocr_engine_for_model;
use futures::{FutureExt as _, StreamExt as _};
use schemars::JsonSchema;

use self::engines::OcrPageInput;
use crate::{
    async_utils::{
        BoxedFuture, BoxedStream, JoinWorker, blocking_iter_streams::BlockingIterStream,
    },
    page_iter::{PageIter, PageIterOptions},
    prelude::*,
    prompt::ChatPrompt,
};

use super::{
    chat::TokenUsage,
    work::{WorkInput, WorkOutput},
};

/// A input record describing a file to OCR.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct OcrInput {
    /// The ID of the record.
    pub id: Value,

    /// The path to the PDF file.
    pub path: PathBuf,

    /// The password to decrypt the PDF file, if any.
    #[serde(default)]
    pub password: Option<String>,
}

impl WorkInput for OcrInput {}

/// An output record describing an OCRed PDF.
#[derive(Debug, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct OcrOutput {
    /// The ID of the record.
    pub id: Value,

    /// The input path.
    pub path: PathBuf,

    /// The text extracted from the PDF. If errors occur on specific pages,
    /// those pages will be `None`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<Option<String>>,

    /// How many pages failed to OCR?
    pub failed_page_count: usize,

    /// Any errors that occurred during processing. Note that because of retries,
    /// even successfully processed documents may have errors.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,

    /// Our estimated cost.
    pub estimated_cost: Option<f64>,

    /// The token usage for this request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,

    /// Any defects in the page that make it difficult to OCR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analysis: Option<OcrAnalysis>,
}

impl WorkOutput for OcrOutput {
    fn is_failure(&self) -> bool {
        self.failed_page_count > 0
    }

    fn cost_estimate(&self) -> Option<f64> {
        self.estimated_cost
    }

    fn token_usage(&self) -> Option<&TokenUsage> {
        self.token_usage.as_ref()
    }

    fn errors(&self) -> &[String] {
        &self.errors
    }
}

/// How was this image generated?
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Deserialize,
    JsonSchema,
    PartialOrd,
    Ord,
    PartialEq,
    Eq,
    Serialize,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum ImageSource {
    // The image appears to be a photo, video, or rendering. This includes
    // images from cameras, videos, and video games.
    PhotoOrVideo,

    // The image appears to have been scanned.
    Scan,

    // The image appears to be a native digital document.
    #[default]
    Digital,
}

impl ImageSource {
    /// Merge two image sources, taking the worst.
    fn merge(self, other: Self) -> Self {
        if self < other { self } else { other }
    }
}

/// Flags describing defects in the page that make it difficult to OCR.
#[derive(Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OcrAnalysis {
    /// The source of this image.
    pub image_source: ImageSource,

    /// The document contains handwriting.
    pub contains_handwriting: bool,

    /// The document contains text that may not have been OCRed correctly.
    pub contains_unreadable_or_ambiguous_text: bool,

    /// The background behind the text is noisy.
    pub background_is_noisy: bool,

    /// The document contains text that is faint or low-contrast.
    pub contains_faint_text: bool,

    /// The document contains text is blurred or out of focus.
    pub contains_blurred_text: bool,

    /// The document contains distorted text, including from crinkled paper,
    /// perspective distortion, or other artifacts.
    pub contains_distorted_text: bool,

    /// The document contains text that is cut off.
    pub contains_cutoff_text: bool,

    /// The image contains glare obscuring the text.
    pub glare_on_some_text: bool,
}

impl OcrAnalysis {
    /// Merge two sets of flags using OR.
    fn merge(&mut self, other: &Self) -> Self {
        Self {
            image_source: self.image_source.merge(other.image_source),
            contains_handwriting: self.contains_handwriting || other.contains_handwriting,
            contains_unreadable_or_ambiguous_text: self
                .contains_unreadable_or_ambiguous_text
                || other.contains_unreadable_or_ambiguous_text,
            background_is_noisy: self.background_is_noisy || other.background_is_noisy,
            contains_faint_text: self.contains_faint_text || other.contains_faint_text,
            contains_blurred_text: self.contains_blurred_text
                || other.contains_blurred_text,
            contains_distorted_text: self.contains_distorted_text
                || other.contains_distorted_text,
            contains_cutoff_text: self.contains_cutoff_text || other.contains_cutoff_text,
            glare_on_some_text: self.glare_on_some_text || other.glare_on_some_text,
        }
    }
}

/// Return value of [`process_chat_stream`].
pub struct OcrStreamInfo {
    pub stream: BoxedStream<BoxedFuture<Result<OcrOutput>>>,
    pub worker: JoinWorker,
}

/// OCR a stream of PDFs.
///
/// This function takes a stream of [`PdfInputRecord`]s as input and returns a
/// stream of [`PdfOutputRecord`]s as output. Internally, it creates and manages
/// a single [`ChatStream`] instance for all requests, which it tries to keep
/// filled at all times.
#[instrument(level = "debug", skip_all)]
pub async fn ocr_files(
    input: BoxedStream<Result<OcrInput>>,
    page_iter_opts: PageIterOptions,
    job_count: usize,
    prompt: ChatPrompt,
    model: String,
) -> Result<OcrStreamInfo> {
    // Create an OCR engine.
    let (engine, worker) =
        ocr_engine_for_model(job_count, prompt, model, &page_iter_opts).await?;

    let output = input
        .map(move |pdf_input| {
            let page_iter_opts = page_iter_opts.clone();
            let engine = engine.clone();
            async move {
                let pdf_input = pdf_input?;
                ocr_file(pdf_input, &page_iter_opts, job_count, engine).await
            }
            .boxed()
        })
        .boxed();

    Ok(OcrStreamInfo {
        stream: output,
        worker,
    })
}

/// Process a PDF file and extract text from it. The text is returned as an array of pages.
#[instrument(level = "debug", skip_all, fields(id = %ocr_input.id))]
async fn ocr_file(
    ocr_input: OcrInput,
    page_iter_opts: &PageIterOptions,
    concurrency_limit: usize,
    engine: Arc<dyn engines::OcrEngine>,
) -> Result<OcrOutput> {
    let id = ocr_input.id.clone();

    // Create a page stream, using BlockingIterStream to avoid blocking the
    // async executor with slow PDF processing.
    let page_stream = BlockingIterStream::new(
        PageIter::from_path(
            &ocr_input.path,
            page_iter_opts,
            ocr_input.password.as_deref(),
        )
        .await
        .with_context(|| {
            format!("failed to create page iterator for {:?}", ocr_input.path)
        })?,
    );

    let page_outputs = page_stream
        .enumerate()
        .map(move |(page_idx, page)| {
            let id = ocr_input.id.clone();
            let engine = engine.clone();
            async move {
                let page = page?;
                engine.ocr_page(OcrPageInput { id, page_idx, page }).await
            }
        })
        // Process all the pages concurrently, up to the concurrency limit.
        .buffered(concurrency_limit)
        // Collect all the results (including fatal errors) into a single vector.
        .collect::<Vec<_>>()
        .await
        // Convert from `Vec<Result<ChatOutput>>` to `Result<Vec<ChatOutput>>`,
        // and exit early if we have any fatal errors.
        .into_iter()
        .collect::<Result<Vec<_>>>()?;

    // Turn out `ChatResponse`s into a `PdfOutput` record.
    let mut errors = vec![];
    let mut failed_page_count = 0;
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
            failed_page_count += 1;
            pages.push(None);
        }
    }
    Ok(OcrOutput {
        id,
        path: ocr_input.path,
        pages,
        failed_page_count,
        errors,
        analysis: if analysis_present && env::var("EXPERIMENTAL_OCR_ANALYSIS").is_ok() {
            Some(analysis)
        } else {
            None
        },
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
    })
}
