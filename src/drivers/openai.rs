//! Our OpenAI driver, which we also use for LiteLLM, Ollama and other
//! compatible gateways.

use async_openai::{
    Client,
    config::OpenAIConfig,
    error::OpenAIError,
    types::{
        ChatCompletionRequestAssistantMessageArgs,
        ChatCompletionRequestAssistantMessageContent, ChatCompletionRequestMessage,
        ChatCompletionRequestMessageContentPartImageArgs,
        ChatCompletionRequestMessageContentPartTextArgs,
        ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestSystemMessageContent, ChatCompletionRequestUserMessageArgs,
        ChatCompletionRequestUserMessageContent,
        ChatCompletionRequestUserMessageContentPart, CreateChatCompletionRequestArgs,
        CreateChatCompletionResponse, ResponseFormat, ResponseFormatJsonSchema,
    },
};

use crate::{
    drivers::TokenUsage,
    litellm::LiteLlmModel,
    prelude::*,
    prompt::{ChatPrompt, Message, Rendered},
    retry::{
        IsKnownTransient, retry_result_fatal, retry_result_ok, try_fatal,
        try_potentially_transient, try_transient,
    },
    schema::get_schema_title,
};

use super::{ChatCompletionResponse, Driver, LlmOpts, LlmRetryResult};

/// Get OpenAI-compatible client configuration.
pub fn get_openai_client_config() -> OpenAIConfig {
    let mut client_config = OpenAIConfig::new();
    if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        client_config = client_config.with_api_key(api_key);
    }
    if let Ok(api_base) = std::env::var("OPENAI_API_BASE") {
        client_config = client_config.with_api_base(api_base);
    }
    client_config
}

/// Create an OpenAI-compatible client using the default configuration.
fn create_llm_client() -> Result<Client<OpenAIConfig>> {
    let client_config = get_openai_client_config();
    let client = Client::with_config(client_config);
    Ok(client)
}

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
    #[instrument(level = "debug", skip_all)]
    async fn chat_completion(
        &self,
        model: &str,
        model_info: Option<&LiteLlmModel>,
        prompt: &ChatPrompt<Rendered>,
        schema: Value,
        llm_opts: &LlmOpts,
    ) -> LlmRetryResult<ChatCompletionResponse> {
        let messages = try_fatal!(prompt.to_openai_prompt());

        // Build our JSON Schema options.
        let json_schema = ResponseFormatJsonSchema {
            name: get_schema_title(&schema),
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
        let req = try_fatal!(req.build().context("Error building request"));
        trace!(?req, "Request");

        // Call OpenAI.
        let chat = self.client.chat();
        let chat_future = llm_opts.apply_timeout(chat.create_byot(req));
        let chat_result: Value = try_potentially_transient!(chat_future.await);
        debug!(%chat_result, "OpenAI response");
        let response = try_fatal!(
            serde_json::from_value::<CreateChatCompletionResponse>(chat_result)
                .context("Error parsing OpenAI response")
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
        let response = try_transient!(
            // If we didn't get JSON here, it's because the model didn't
            // generate JSON. So give it another chance with `try_transient!`.
            serde_json::from_str::<Value>(content).with_context(|| format!(
                "Error parsing OpenAI response content: {:?}",
                content
            ))
        );
        debug!(%response, "Response");
        retry_result_ok(ChatCompletionResponse {
            response,
            token_usage,
        })
    }
}

impl IsKnownTransient for OpenAIError {
    fn is_known_transient(&self) -> bool {
        match self {
            OpenAIError::Reqwest(error) => error.is_known_transient(),
            _ => false,
        }
    }
}

/// Convert a [`Rendered`] version of a type to an OpenAI prompt.
pub trait ToOpenAiPrompt {
    type Output;

    /// Render the template.
    fn to_openai_prompt(&self) -> Result<Self::Output>;
}

impl ToOpenAiPrompt for ChatPrompt<Rendered> {
    type Output = Vec<ChatCompletionRequestMessage>;

    fn to_openai_prompt(&self) -> Result<Self::Output> {
        // Make sure our messages appear in the order ((user, assistant)*, user).
        if self.messages.is_empty() {
            return Err(anyhow!("No messages in prompt"));
        }
        let mut expect_user_message = true;
        for message in &self.messages {
            let ok = match message {
                Message::User { .. } if expect_user_message => true,
                Message::Assistant { .. } if !expect_user_message => true,
                _ => false,
            };
            if !ok {
                return Err(anyhow!(
                    "Expected alternating user and assistant messages in prompt, found {:?}",
                    message
                ));
            }
            expect_user_message = !expect_user_message;
        }
        if self.messages.len() % 2 == 0 {
            return Err(anyhow!("Prompt must end with a user message"));
        }

        // Render our prompt.
        let mut messages = Vec::new();
        if let Some(developer) = &self.developer {
            messages.push(system_message(developer.to_owned())?);
        }
        for message in &self.messages {
            messages.push(message.to_openai_prompt()?);
        }
        Ok(messages)
    }
}

impl ToOpenAiPrompt for Message {
    type Output = ChatCompletionRequestMessage;

    fn to_openai_prompt(&self) -> Result<Self::Output> {
        match self {
            // No user content, so we bail.
            Message::User { text: None, images } if images.is_empty() => {
                Err(anyhow!("user message must have either text or images"))
            }
            // Just text, so use the simple format.
            Message::User {
                text: Some(text),
                images,
            } if images.is_empty() => user_message(text.to_owned()),
            // We have images, and maybe text, so use the multi-part format.
            Message::User { text, images } => {
                let mut parts = Vec::with_capacity(1 + images.len());
                if let Some(text) = text {
                    parts.push(user_message_text_part(text.to_owned())?);
                }
                for image in images {
                    parts.push(user_message_image_part(image.to_owned())?);
                }
                user_message_multi_part(parts)
            }
            Message::Assistant { json } => assistant_message(json.to_string()),
        }
    }
}

/// Build a system message.
fn system_message(content: String) -> Result<ChatCompletionRequestMessage> {
    Ok(ChatCompletionRequestMessage::System(
        ChatCompletionRequestSystemMessageArgs::default()
            .content(ChatCompletionRequestSystemMessageContent::Text(content))
            .build()?,
    ))
}

/// Build a simple user message.
fn user_message(content: String) -> Result<ChatCompletionRequestMessage> {
    Ok(ChatCompletionRequestMessage::User(
        ChatCompletionRequestUserMessageArgs::default()
            .content(ChatCompletionRequestUserMessageContent::Text(content))
            .build()?,
    ))
}

/// Build a multi-part user message.
fn user_message_multi_part(
    content: Vec<ChatCompletionRequestUserMessageContentPart>,
) -> Result<ChatCompletionRequestMessage> {
    Ok(ChatCompletionRequestMessage::User(
        ChatCompletionRequestUserMessageArgs::default()
            .content(ChatCompletionRequestUserMessageContent::Array(content))
            .build()?,
    ))
}

// Build a user message text part.
fn user_message_text_part(
    text: String,
) -> Result<ChatCompletionRequestUserMessageContentPart> {
    Ok(ChatCompletionRequestUserMessageContentPart::Text(
        ChatCompletionRequestMessageContentPartTextArgs::default()
            .text(text)
            .build()?,
    ))
}

// Build a user message image part.
fn user_message_image_part(
    url: String,
) -> Result<ChatCompletionRequestUserMessageContentPart> {
    Ok(ChatCompletionRequestUserMessageContentPart::ImageUrl(
        ChatCompletionRequestMessageContentPartImageArgs::default()
            .image_url(url)
            .build()?,
    ))
}

/// Build an assistant message.
fn assistant_message(content: String) -> Result<ChatCompletionRequestMessage> {
    Ok(ChatCompletionRequestMessage::Assistant(
        ChatCompletionRequestAssistantMessageArgs::default()
            .content(ChatCompletionRequestAssistantMessageContent::Text(content))
            .build()?,
    ))
}
