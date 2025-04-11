//! PDF utilities.

pub mod engines;

use std::{sync::Arc, vec};

use engines::ocr_engine_for_model;
use futures::{FutureExt as _, StreamExt as _};

use self::engines::OcrPageInput;
use crate::{
    async_utils::{
        BoxedFuture, BoxedStream, JoinWorker, blocking_iter_streams::BlockingIterStream,
    },
    page_iter::{PageIter, PageIterOptions},
    prelude::*,
    prompt::ChatPrompt,
};

use super::work::{WorkInput, WorkOutput};

/// A input record describing a file to OCR.
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct OcrOutput {
    /// The ID of the record.
    pub id: Value,

    /// The text extracted from the PDF. If errors occur on specific pages,
    /// those pages will be `None`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extracted_text: Vec<Option<String>>,

    /// How many pages failed to OCR?
    pub failed_page_count: usize,

    /// Any errors that occurred during processing. Note that because of retries,
    /// even successfully processed documents may have errors.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

impl WorkOutput for OcrOutput {
    fn is_failure(&self) -> bool {
        self.failed_page_count > 0
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

    let chat_outputs = page_stream
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

    // Turn out `ChatResponse`s ito a `PdfOutput` record.
    let mut errors = vec![];
    let mut failed_page_count = 0;
    let mut extracted_text = vec![];
    for chat_output in chat_outputs {
        errors.extend(chat_output.errors);
        if let Some(text) = chat_output.text {
            extracted_text.push(Some(text));
        } else {
            // No LLM response.
            failed_page_count += 1;
            extracted_text.push(None);
        }
    }
    Ok(OcrOutput {
        id,
        extracted_text,
        failed_page_count,
        errors,
    })
}
