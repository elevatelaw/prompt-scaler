//! Native LLM driver using the [`genai`] crate, which provides a unified
//! interface to multiple LLM providers.

use std::sync::Arc;

use async_trait::async_trait;
use genai::{
    Client, ModelIden, ServiceTarget,
    adapter::AdapterKind,
    chat::{
        ChatMessage, ChatOptions, ChatRequest, ChatResponseFormat, ChatRole, ContentPart,
        JsonSpec, MessageContent, Usage,
    },
    resolver::{AuthData, Endpoint, ServiceTargetResolver},
    webc,
};

use crate::{
    images::{ImageEncoding, ImageFile},
    mem_limit::MemLimiter,
    prelude::*,
    prompt::{ChatPrompt, Message, Rendered},
    retry::IsKnownTransient,
    schema::get_schema_title,
};

use super::{ChatCompletionResponse, Driver, DriverError, LlmOpts, TokenUsage};

/// Our native driver, using the `genai` crate.
#[derive(Debug)]
pub struct NativeDriver {
    /// The genai client.
    pub client: Client,
}

impl NativeDriver {
    /// Create a new native driver.
    pub async fn new() -> Result<Self> {
        // Honor `OPENAI_API_BASE`/`OPENAI_API_KEY` for OpenAI-compatible
        // gateways (llama-server, Ollama, LiteLLM). When the base is set we
        // force every model through the OpenAI adapter at that endpoint, to
        // match the behavior the old `openai` driver offered as the default.
        let target_resolver = ServiceTargetResolver::from_resolver_fn(
            |target: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
                if let Ok(api_base) = std::env::var("OPENAI_API_BASE") {
                    let ServiceTarget { model, .. } = target;
                    Ok(ServiceTarget {
                        endpoint: Endpoint::from_owned(api_base),
                        auth: AuthData::from_env("OPENAI_API_KEY"),
                        model: ModelIden::new(AdapterKind::OpenAI, model.model_name),
                    })
                } else {
                    Ok(target)
                }
            },
        );
        let client = Client::builder()
            .with_service_target_resolver(target_resolver)
            .build();
        Ok(Self { client })
    }
}

#[async_trait]
impl Driver for NativeDriver {
    #[instrument(level = "debug", skip_all)]
    async fn chat_completion(
        &self,
        model: &str,
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
            reasoning_effort: llm_opts.reasoning_effort.clone(),
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
        let content_str = chat_res
            .first_text()
            .ok_or_else(|| anyhow!("No text content in response: {:?}", chat_res))
            .map_err(DriverError::invalid_output)?;

        // Extract JSON from our content.
        let response = serde_json::from_str::<Value>(content_str)
            .with_context(|| {
                format!("Error parsing OpenAI response content: {content_str:?}")
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
                    // TODO: Can we avoid this copy by representing file paths
                    // directly in messages? This might require reading the docs
                    // and changing how we handle permits here.
                    parts.push(ContentPart::from_binary_base64(
                        image_data.mime_type().to_owned(),
                        Arc::from(base64_str),
                        None,
                    ));
                }
                Ok(ChatMessage {
                    role: ChatRole::User,
                    content: MessageContent::from_parts(parts),
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
