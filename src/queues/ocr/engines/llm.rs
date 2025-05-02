//! LLM-based OCR engine.

use std::sync::Arc;

use schemars::JsonSchema;
use serde_json::Map;

use crate::{
    async_utils::JoinWorker,
    data_url::data_url,
    drivers::LlmOpts,
    prelude::*,
    prompt::ChatPrompt,
    queues::{
        chat::{ChatInput, ChatOutput, create_chat_work_queue},
        ocr::OcrAnalysis,
        work::{WorkInput, WorkItemProcessor as _, WorkQueue},
    },
    schema::Schema,
};

use super::{OcrEngine, OcrPageInput, OcrPageOutput};

/// The default OCR prompt, used if no prompt is provided.
const DEFAULT_OCR_PROMPT: &str = include_str!("llm/default_ocr_prompt.toml");

/// Our example PNG input.
const EXAMPLE_INPUT: &[u8] = include_bytes!("llm/example_input.png");

/// Our example Markdown output.
const EXAMPLE_OUTPUT: &str = include_str!("llm/example_output.md");

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

    /// Analysis of the page.
    analysis: OcrAnalysis,
}

/// An LLM-based OCR engine.
pub struct LlmOcrEngine {
    /// Pointer to the work queue that talks to the LLM.
    chat_queue: WorkQueue<ChatInput, ChatOutput>,
}

impl LlmOcrEngine {
    /// Create a new LLM-based OCR engine.
    #[allow(clippy::new_ret_no_self)]
    pub async fn new(
        concurrency_limit: usize,
        mut prompt: ChatPrompt,
        model: String,
        llm_opts: LlmOpts,
    ) -> Result<(Arc<dyn OcrEngine>, JoinWorker)> {
        // Add our schema to our prompt.
        prompt.response_schema = Schema::from_type::<PageChatResponse>();

        // Create a new chat queue to handle all our LLM requests.
        let (chat_queue, worker) =
            create_chat_work_queue(concurrency_limit, prompt, model, llm_opts).await?;

        Ok((Arc::new(Self { chat_queue }), worker))
    }
}

#[async_trait]
impl OcrEngine for LlmOcrEngine {
    #[instrument(level = "debug", skip_all, fields(id = %input.id, page = %input.page_idx))]
    async fn ocr_page(&self, mut input: OcrPageInput) -> Result<OcrPageOutput> {
        // Get a chat handle.
        let chat_handle = self.chat_queue.handle();

        let mut template_bindings = Map::new();
        template_bindings.insert(
            "page_data_url".to_string(),
            Value::String(input.page.to_data_url()),
        );
        input.page.data = vec![]; // Release memory, because it adds up.
        template_bindings.insert(
            "example_input_data_url".to_string(),
            Value::String(data_url("image/png", EXAMPLE_INPUT)),
        );
        template_bindings.insert(
            "example_output".to_string(),
            Value::String(EXAMPLE_OUTPUT.to_string()),
        );

        let input = WorkInput {
            id: Value::Array(vec![
                input.id.clone(),
                Value::Number(input.page_idx.into()),
            ]),
            data: ChatInput { template_bindings },
        };
        let chat_output = chat_handle.process_blocking(input).await?;
        let errors = chat_output.errors;
        let response = serde_json::from_value::<Option<PageChatResponse>>(
            chat_output.data.response.unwrap_or_default(),
        )
        .context("failed to parse LLM OCR response")?;

        Ok(OcrPageOutput {
            text: response.as_ref().map(|r| r.full_markdown.to_owned()),
            errors,
            analysis: response.map(|r| r.analysis),
            estimated_cost: chat_output.estimated_cost,
            token_usage: chat_output.token_usage,
        })
    }
}
