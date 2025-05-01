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
        io::write_output_csv,
    },
    cmd::StreamOpts,
    page_iter::{PageIter, PageIterOptions},
    prelude::*,
    prompt::ChatPrompt,
    ui::Ui,
};

use super::{
    chat::{LlmOpts, TokenUsage},
    work::{
        WorkInput, WorkItemCounterExt as _, WorkOutput, WorkOutputCounters, WorkStatus,
    },
};

/// A input record describing a file to OCR.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct OcrInput {
    /// The path to the PDF file.
    pub path: PathBuf,

    /// The password to decrypt the PDF file, if any.
    #[serde(default)]
    pub password: Option<String>,
}

/// An output record describing an OCRed PDF.
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct OcrOutput {
    /// The input path.
    pub path: PathBuf,

    /// The text extracted from the PDF. If errors occur on specific pages,
    /// those pages will be replaced with `**COULD_NOT_OCR_PAGE**`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,

    /// Any defects in the page that make it difficult to OCR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analysis: Option<OcrAnalysis>,
}

impl WorkOutput<OcrOutput> {
    /// Convert this output record to a flat version for CSV output.
    fn to_flat(&self) -> FlatOcrOutput {
        FlatOcrOutput {
            id: if let Value::String(id) = &self.id {
                id.clone()
            } else {
                serde_json::to_string(&self.id).expect("failed to convert ID to string")
            },
            status: self.status,
            path: self.data.path.clone(),
            errors: if self.errors.is_empty() {
                None
            } else {
                Some(self.errors.join("\n\n"))
            },
            text: self.data.text.clone(),
        }
    }

    /// Write a stream of outputs to a [`Path`] or to standard output.
    pub async fn write_stream_to_csv(
        ui: &Ui,
        path: Option<&Path>,
        stream: BoxedStream<Result<Self>>,
        stream_opts: &StreamOpts,
    ) -> Result<()> {
        let (stream, counters) = WorkOutputCounters::wrap_stream(stream);
        let output = stream.map(|output| Ok(output?.to_flat())).boxed();
        write_output_csv(path, output).await?;
        counters.finish(ui, stream_opts)
    }
}

/// Flat version of [`WorkOutput<OcrOutput>`], for CSV output.
///
/// Does not contain anything but essential fields.
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct FlatOcrOutput {
    /// The ID of the input record.
    pub id: String,

    /// The status of the output record.
    pub status: WorkStatus,

    /// Any errors that occurred during processing.
    pub errors: Option<String>,

    /// The path to the PDF file.
    pub path: PathBuf,

    /// The text extracted from the PDF. If errors occur on specific pages,
    /// those pages will be replaced with `**COULD_NOT_OCR_PAGE**`.
    pub text: Option<String>,
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
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
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
    pub stream: BoxedStream<BoxedFuture<Result<WorkOutput<OcrOutput>>>>,
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
    input: BoxedStream<Result<WorkInput<OcrInput>>>,
    job_count: usize,
    prompt: ChatPrompt,
    model: String,
    page_iter_opts: PageIterOptions,
    llm_opts: LlmOpts,
) -> Result<OcrStreamInfo> {
    // Create an OCR engine.
    let (engine, worker) =
        ocr_engine_for_model(job_count, prompt, model, &page_iter_opts, llm_opts).await?;

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
pub async fn ocr_file(
    ocr_input: WorkInput<OcrInput>,
    page_iter_opts: &PageIterOptions,
    concurrency_limit: usize,
    engine: Arc<dyn engines::OcrEngine>,
) -> Result<WorkOutput<OcrOutput>> {
    let id = ocr_input.id.clone();
    let path = ocr_input.data.path.clone();

    // Perform the actual work.
    let result =
        ocr_file_inner(ocr_input, page_iter_opts, concurrency_limit, engine).await;

    // If we have an error, output an appropriate record and continue.
    // This is necessary to avoid aborting an entire batch of work if one
    // PDF file is corrupt.
    match result {
        Ok(output) => Ok(output),
        Err(err) => {
            let errors = vec![format!("{:?}", err)];
            Ok(WorkOutput {
                id,
                status: WorkStatus::Failed,
                errors,
                estimated_cost: None,
                token_usage: None,
                data: OcrOutput {
                    path,
                    text: None,
                    analysis: None,
                },
            })
        }
    }
}

/// Perform actual work for `ocr_file`.
#[instrument(level = "debug", skip_all, fields(id = %ocr_input.id))]
async fn ocr_file_inner(
    ocr_input: WorkInput<OcrInput>,
    page_iter_opts: &PageIterOptions,
    concurrency_limit: usize,
    engine: Arc<dyn engines::OcrEngine>,
) -> Result<WorkOutput<OcrOutput>> {
    let id = ocr_input.id.clone();

    // Create a page stream, using BlockingIterStream to avoid blocking the
    // async executor with slow PDF processing.
    let page_iter = PageIter::from_path(
        &ocr_input.data.path,
        page_iter_opts,
        ocr_input.data.password.as_deref(),
    )
    .await
    .with_context(|| {
        format!(
            "failed to create page iterator for {:?}",
            ocr_input.data.path
        )
    })?;
    let check_complete_result = page_iter.check_complete();
    let warnings = page_iter.warnings().to_owned();
    let page_stream = BlockingIterStream::new(page_iter);

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

    let good_page_count = pages.iter().filter(|p| p.is_some()).count();
    let total_page_count = pages.len();
    let text = pages
        .into_iter()
        .map(|p| p.unwrap_or_else(|| "**COULD_NOT_OCR_PAGE**".to_owned()))
        .collect::<Vec<String>>()
        .join("\n\n");
    Ok(WorkOutput {
        id,
        status: if check_complete_result.is_ok() && good_page_count == total_page_count {
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
            analysis: if analysis_present && env::var("EXPERIMENTAL_OCR_ANALYSIS").is_ok()
            {
                Some(analysis)
            } else {
                None
            },
        },
    })
}
