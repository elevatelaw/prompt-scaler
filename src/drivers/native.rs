//! Native LLM driver, for use in cases where LiteLLM isn't available or can't
//! handle the load.
//!
//! For now, we use the [`genai`] crate, which seems reasonably popular.

use std::sync::Arc;

use async_trait::async_trait;
use genai::{
    Client,
    chat::{
        ChatMessage, ChatOptions, ChatRequest, ChatResponseFormat, ChatRole, ContentPart,
        ImageSource, JsonSpec, MessageContent, Usage,
    },
    webc,
};

use crate::{
    images::{ImageEncoding, ImageFile},
    litellm::LiteLlmModel,
    mem_limit::MemLimiter,
    prelude::*,
    prompt::{ChatPrompt, Message, Rendered},
    retry::IsKnownTransient,
    schema::get_schema_title,
};

use super::{ChatCompletionResponse, Driver, DriverError, LlmOpts, TokenUsage};

/// Our OpenAI driver, which we also use for LiteLLM, Ollama and other
/// compatible gateways.
#[derive(Debug)]
pub struct NativeDriver {
    /// The OpenAI client.
    pub client: Client,
}

impl NativeDriver {
    /// Create a new native driver.
    pub async fn new() -> Result<Self> {
        Ok(Self {
            client: Client::default(),
        })
    }
}

#[async_trait]
impl Driver for NativeDriver {
    #[instrument(level = "debug", skip_all)]
    async fn chat_completion(
        &self,
        model: &str,
        _model_info: Option<&LiteLlmModel>,
        prompt: &ChatPrompt<Rendered>,
        mut schema: Value,
        llm_opts: &LlmOpts,
        mem_limiter: &MemLimiter,
    ) -> Result<ChatCompletionResponse, DriverError> {
        // Report what native driver we're using under the hood.
        if let Ok(service_target) = self.client.resolve_service_target(model).await {
            debug!(
                adapter_kind = %service_target.model.adapter_kind,
                model = model,
                "Using native driver"
            );
        }

        // Fix our schema for compatibility.
        {
            let schema = schema
                .as_object_mut()
                .ok_or_else(|| anyhow!("Expected schema to be an object"))
                .map_err(DriverError::invalid_input)?;
            schema.remove("$schema");
        }

        // Convert our prompt to a genai request and build our options.
        let req = prompt
            .to_genai_request(mem_limiter)
            .await
            .map_err(DriverError::invalid_input)?;
        let opts = ChatOptions {
            temperature: llm_opts.temperature.map(f64::from),
            max_tokens: llm_opts.max_completion_tokens,
            top_p: llm_opts.top_p.map(f64::from),
            response_format: Some(ChatResponseFormat::JsonSpec(JsonSpec {
                name: get_schema_title(&schema),
                description: None,
                schema,
            })),
            ..ChatOptions::default()
        };

        // Run our LLM request.
        let chat_res = self
            .client
            .exec_chat(model, req, Some(&opts))
            .await
            .map_err(DriverError::api)?;

        // Extract our response content.
        let content = chat_res
            .content
            .as_ref()
            .ok_or_else(|| anyhow!("No content in response: {:?}", chat_res))
            .map_err(DriverError::invalid_output)?;
        let content_str = content
            .text_as_str()
            .ok_or_else(|| {
                anyhow!("Expected text content in response, found: {:?}", content)
            })
            .map_err(DriverError::invalid_output)?;

        // Extract JSON from our content.
        let response = serde_json::from_str::<Value>(content_str)
            .with_context(|| {
                format!("Error parsing OpenAI response content: {content:?}")
            })
            .map_err(DriverError::invalid_output_transient)?;
        debug!(%response, "Response");

        // Compute our token usage.
        let token_usage = if let Usage {
            prompt_tokens: Some(prompt_tokens),
            completion_tokens: Some(completion_tokens),
            ..
        } = chat_res.usage
        {
            Some(TokenUsage {
                prompt_tokens: u64::try_from(prompt_tokens).unwrap_or_default(),
                completion_tokens: u64::try_from(completion_tokens).unwrap_or_default(),
            })
        } else {
            None
        };

        Ok(ChatCompletionResponse {
            response,
            token_usage,
        })
    }
}

impl IsKnownTransient for genai::Error {
    fn is_known_transient(&self) -> bool {
        match self {
            // These seem likely to be transient, but we have not observed them
            // in the wild yet.
            genai::Error::NoChatResponse { .. }
            | genai::Error::InvalidJsonResponseElement { .. } => true,
            genai::Error::WebAdapterCall { webc_error, .. }
            | genai::Error::WebModelCall { webc_error, .. } => {
                webc_error.is_known_transient()
            }
            // Assume other errors are fatal, until we discover otherwise in
            // production.
            _ => false,
        }
    }
}

impl IsKnownTransient for webc::Error {
    fn is_known_transient(&self) -> bool {
        match self {
            webc::Error::ResponseFailedNotJson { .. } => true,
            webc::Error::ResponseFailedStatus { status, .. } => {
                status.is_known_transient()
            }
            webc::Error::Reqwest(error) => error.is_known_transient(),
            _ => false,
        }
    }
}

/// Convert a [`ChatPrompt`] to something compatible with [`genai`].
impl ChatPrompt<Rendered> {
    async fn to_genai_request(&self, mem_limiter: &MemLimiter) -> Result<ChatRequest> {
        let mut messages = vec![];
        for message in &self.messages {
            messages.push(message.to_genai_message(mem_limiter).await?);
        }

        Ok(ChatRequest {
            system: self.developer.clone(),
            messages,
            ..ChatRequest::default()
        })
    }
}

impl Message {
    async fn to_genai_message(&self, mem_limiter: &MemLimiter) -> Result<ChatMessage> {
        match self {
            // We have images and maybe text.
            Message::User { text, images } if !images.is_empty() => {
                let mut parts = vec![];
                if let Some(text) = text {
                    parts.push(ContentPart::Text(text.clone()));
                }
                for image in images {
                    let image_file = ImageFile::from_url(image)?;
                    let image_data =
                        image_file.load(ImageEncoding::Base64, mem_limiter).await?;
                    let base64_str = std::str::from_utf8(image_data.data())
                        .context("base64 image data is not valid UTF-8")?;
                    parts.push(ContentPart::Image {
                        content_type: image_data.mime_type().to_owned(),
                        // TODO: Can we avoid this copy by representing file
                        // paths directly in messages? This might require reading
                        // the docs and changing how we handle permits here.
                        source: ImageSource::Base64(Arc::from(base64_str)),
                    });
                }
                Ok(ChatMessage {
                    role: ChatRole::User,
                    content: MessageContent::Parts(parts),
                    options: None,
                })
            }

            // We have text and no images.
            Message::User {
                text: Some(text), ..
            } => Ok(ChatMessage::user(text.clone())),

            // We have no text and no images.
            Message::User { .. } => Err(anyhow!("No text or images in user message")),

            // We have a fake assistant message, with a JSON value attached.
            Message::Assistant { json } => {
                let json = json.to_string();
                Ok(ChatMessage::assistant(json))
            }
        }
    }
}
