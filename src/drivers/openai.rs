//! Our OpenAI driver, which we also use for LiteLLM, Ollama and other
//! compatible gateways.

use std::time::Duration;

use async_openai::{
    Client,
    config::OpenAIConfig,
    types::{
        CreateChatCompletionRequestArgs, CreateChatCompletionResponse, ResponseFormat,
        ResponseFormatJsonSchema,
    },
};
use futures::{FutureExt as _, TryFutureExt as _};
use tokio::time;

use crate::{
    drivers::{LlmError, TokenUsage},
    llm_client::{LiteLlmModel, create_llm_client},
    prelude::*,
    prompt::{ChatPrompt, Rendered, ToOpenAiPrompt as _},
    retry::{
        IntoRetryResult as _, IsKnownTransient as _, retry_result_fatal, retry_result_ok,
        try_with_retry_result,
    },
};

use super::{ChatCompletionResponse, Driver, LlmOpts, LlmRetryResult};

/// Our OpenAI driver, which we also use for LiteLLM, Ollama and other
/// compatible gateways.
#[derive(Debug)]
pub struct OpenAiDriver {
    /// The OpenAI client.
    pub client: Client<OpenAIConfig>,
}

impl OpenAiDriver {
    /// Create a new OpenAI driver.
    pub async fn new() -> Result<Self> {
        let client = create_llm_client()?;
        Ok(Self { client })
    }
}

#[async_trait]
impl Driver for OpenAiDriver {
    async fn chat_completion(
        &self,
        model: &str,
        model_info: Option<&LiteLlmModel>,
        prompt: &ChatPrompt<Rendered>,
        schema: Value,
        llm_opts: &LlmOpts,
    ) -> LlmRetryResult<ChatCompletionResponse> {
        let messages = try_with_retry_result!(prompt.to_openai_prompt().into_fatal());

        // Build our JSON Schema options.
        let json_schema = ResponseFormatJsonSchema {
            name: schema
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("ResponseFormat")
                .to_owned(),
            schema: Some(schema),
            strict: Some(true),
            description: None,
        };

        // Turn our prompt into a chat request.
        let mut req = CreateChatCompletionRequestArgs::default();
        req.model(model.to_owned())
            .messages(messages)
            .response_format(ResponseFormat::JsonSchema { json_schema });
        let mut need_to_disable_store = true;
        if let Some(model_info) = model_info {
            eprintln!("Provider: {}", model_info.model_info.litellm_provider);
            if model_info.model_info.litellm_provider == "anthropic" {
                // For OpenAI (and possibly other providers with the same API), we
                // need to set `store` to false to prevent the API from storing
                // responses for later REST calls. But LiteLLM doesn't know about
                // this parameter, and doesn't remove it when calling Anthropic
                // models.
                need_to_disable_store = false;
            }
        }
        if need_to_disable_store {
            req.store(false);
        }
        if let Some(max_completion_tokens) = llm_opts.max_completion_tokens {
            req.max_completion_tokens(max_completion_tokens);
        }
        if let Some(temperature) = llm_opts.temperature {
            req.temperature(temperature);
        }
        if let Some(top_p) = llm_opts.top_p {
            req.top_p(top_p);
        }
        let req = try_with_retry_result!(
            req.build().context("Error building request").into_fatal()
        );
        trace!(?req, "Request");

        // Call OpenAI.
        let chat = self.client.chat();
        let mut chat_future = chat.create_byot(req).map_err(LlmError::Native).boxed();
        if let Some(timeout) = llm_opts.timeout {
            // If we have a timeout, wrap our future in a timeout, and merge the errors
            // from the `Result<Result<_, LlmError>, Elapsed>` into a single level.
            chat_future = time::timeout(Duration::from_secs(timeout), chat_future)
                .map(|result| match result {
                    Ok(inner) => inner,
                    Err(_) => Err(LlmError::Timeout),
                })
                .boxed();
        }
        let chat_result: Value = try_with_retry_result!(
            chat_future
                .await
                .into_retry_result(LlmError::is_known_transient)
        );
        debug!(%chat_result, "OpenAI response");
        let response = try_with_retry_result!(
            serde_json::from_value::<CreateChatCompletionResponse>(chat_result)
                .context("Error parsing OpenAI response")
                .into_fatal()
        );

        // How many tokens did we use?
        let token_usage = response.usage.map(|usage| TokenUsage {
            prompt_tokens: u64::from(usage.prompt_tokens),
            completion_tokens: u64::from(usage.completion_tokens),
        });

        // Get the content from our response & parse as JSON.
        let choice = match response.choices.first() {
            Some(choice) => choice,
            None => {
                return retry_result_fatal(anyhow!("No choices in OpenAI response"));
            }
        };
        if choice.finish_reason == Some(async_openai::types::FinishReason::ContentFilter)
        {
            return retry_result_fatal(anyhow!(
                "Content filter triggered (may also be a RECITATION error for Gemini models)"
            ));
        }
        let content = choice.message.content.as_deref().unwrap_or_default();
        let response = try_with_retry_result!(
            serde_json::from_str::<Value>(content)
                .with_context(|| format!(
                    "Error parsing OpenAI response content: {:?}",
                    content
                ))
                // If we didn't get JSON here, it's because the model didn't
                // generate JSON. So give it another chance.
                .into_transient()
        );
        debug!(%content, "Response");
        retry_result_ok(ChatCompletionResponse {
            response,
            token_usage,
        })
    }
}
