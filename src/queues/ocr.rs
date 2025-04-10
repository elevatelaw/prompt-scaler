//! PDF utilities.

use std::vec;

use futures::{FutureExt as _, StreamExt as _};
use schemars::JsonSchema;
use serde_json::Map;

use crate::{
    async_utils::{
        blocking_iter_streams::BlockingIterStream,
        io::{BoxedFuture, BoxedStream},
    },
    data_url::data_url,
    page_iter::{Page, PageIter, PageIterOptions},
    prelude::*,
    prompt::ChatPrompt,
    queues::{
        chat::{ChatInput, ChatOutput, create_chat_work_queue},
        work::{WorkItemProcessor, WorkQueue, WorkQueueHandle},
    },
    schema::Schema,
};

use super::work::{WorkInput, WorkOutput};

/// The default OCR prompt, used if no prompt is provided.
const DEFAULT_OCR_PROMPT: &str = include_str!("ocr/default_ocr_prompt.toml");

/// Our example PNG input.
const EXAMPLE_INPUT: &[u8] = include_bytes!("ocr/example_input.png");

/// Our example Markdown output.
const EXAMPLE_OUTPUT: &str = include_str!("ocr/example_output.md");

/// Get our default OCR prompt.
pub fn default_ocr_prompt() -> ChatPrompt {
    toml::from_str::<ChatPrompt>(DEFAULT_OCR_PROMPT)
        .expect("failed to parse built-in OCR prompt")
}

/// Information to extract from each page
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PageChatResponse {
    /// The complete text of the page, in Markdown format.
    full_markdown: String,
}

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
    pub queue: WorkQueue<ChatInput, ChatOutput>,
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
    mut prompt: ChatPrompt,
    model: String,
) -> Result<OcrStreamInfo> {
    // Add our schema to our prompt.
    prompt.response_schema = Schema::from_type::<PageChatResponse>();

    // Create a new chat queue to handle all our LLM requests.
    let chat_queue = create_chat_work_queue(job_count, prompt, model).await?;
    let chat_handle = chat_queue.handle();

    let output = input
        .map(move |pdf_input| {
            let page_iter_opts = page_iter_opts.clone();
            let chat_handle = chat_handle.clone();
            async move {
                let pdf_input = pdf_input?;
                ocr_file(pdf_input, &page_iter_opts, job_count, chat_handle).await
            }
            .boxed()
        })
        .boxed();

    Ok(OcrStreamInfo {
        stream: output,
        queue: chat_queue,
    })
}

/// Process a PDF file and extract text from it. The text is returned as an array of pages.
#[instrument(level = "debug", skip_all, fields(id = %ocr_input.id))]
async fn ocr_file(
    ocr_input: OcrInput,
    page_iter_opts: &PageIterOptions,
    concurrency_limit: usize,
    chat_handle: WorkQueueHandle<ChatInput, ChatOutput>,
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
            let chat_handle = chat_handle.clone();
            async move {
                let page = page?;
                ocr_page(id, page_idx, page, chat_handle).await
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
        if let Some(response) = chat_output.response {
            let page_response = serde_json::from_value::<PageChatResponse>(response)?;
            extracted_text.push(Some(page_response.full_markdown));
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

/// Process a single page of a PDF file.
#[instrument(level = "debug", skip_all, fields(id, page_idx))]
async fn ocr_page(
    id: Value,
    page_idx: usize,
    page: Page,
    chat_handle: WorkQueueHandle<ChatInput, ChatOutput>,
) -> Result<ChatOutput> {
    let mut template_bindings = Map::new();
    template_bindings.insert(
        "page_data_url".to_string(),
        Value::String(page.to_data_url()),
    );
    template_bindings.insert(
        "example_input_data_url".to_string(),
        Value::String(data_url("image/png", EXAMPLE_INPUT)),
    );
    template_bindings.insert(
        "example_output".to_string(),
        Value::String(EXAMPLE_OUTPUT.to_string()),
    );

    let input = ChatInput {
        id: Value::Array(vec![id.clone(), Value::Number(page_idx.into())]),
        template_bindings,
    };
    chat_handle.process_blocking(input).await
}
