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
    data_url::parse_data_url,
    litellm::LiteLlmModel,
    prelude::*,
    prompt::{ChatPrompt, Message, Rendered},
    retry::{
        IsKnownTransient, retry_result_ok, try_fatal, try_potentially_transient,
        try_transient,
    },
    schema::get_schema_title,
};

use super::{ChatCompletionResponse, Driver, LlmOpts, LlmRetryResult, TokenUsage};

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
    ) -> LlmRetryResult<ChatCompletionResponse> {
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
            let schema = try_fatal!(
                schema
                    .as_object_mut()
                    .ok_or_else(|| { anyhow!("Expected schema to be an object") })
            );
            schema.remove("$schema");
        }

        // Convert our prompt to a genai request and build our options.
        let req = try_fatal!(prompt.to_genai_request());
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

        // Run our LLM request with a timeout.
        let future =
            llm_opts.apply_timeout(self.client.exec_chat(model, req, Some(&opts)));
        let chat_res = try_potentially_transient!(future.await);

        // Extract our response content.
        let content = try_fatal!(
            chat_res
                .content
                .as_ref()
                .ok_or_else(|| anyhow!("No content in response: {:?}", chat_res))
        );
        let content_str = try_fatal!(content.text_as_str().ok_or_else(|| anyhow!(
            "Expected text content in response, found: {:?}",
            content
        )));

        // Extract JSON from our content.
        let response = try_transient!(
            // If we didn't get JSON here, it's because the model didn't
            // generate JSON. So give it another chance with `try_transient!`.
            serde_json::from_str::<Value>(content_str).with_context(|| format!(
                "Error parsing OpenAI response content: {content:?}"
            ))
        );
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

        retry_result_ok(ChatCompletionResponse {
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
pub trait ToGenaiRequest {
    /// The type of the output.
    type Output;

    /// Convert this value to something compatible with [`genai`].
    fn to_genai_request(&self) -> Result<Self::Output>;
}

impl ToGenaiRequest for ChatPrompt<Rendered> {
    type Output = ChatRequest;

    fn to_genai_request(&self) -> Result<Self::Output> {
        let messages = self
            .messages
            .iter()
            .map(|m| m.to_genai_request())
            .collect::<Result<Vec<_>>>()?;

        Ok(ChatRequest {
            system: self.developer.clone(),
            messages,
            ..ChatRequest::default()
        })
    }
}

impl ToGenaiRequest for Message {
    type Output = ChatMessage;

    fn to_genai_request(&self) -> Result<Self::Output> {
        match self {
            // We have images and maybe text.
            Message::User { text, images } if !images.is_empty() => {
                let mut parts = vec![];
                if let Some(text) = text {
                    parts.push(ContentPart::Text(text.clone()));
                }
                for image in images {
                    if let Some((mime_type, data)) = parse_data_url(image) {
                        parts.push(ContentPart::Image {
                            content_type: mime_type,
                            // TODO: Avoid this copy by representing file paths
                            // directly in messages.
                            source: ImageSource::Base64(Arc::from(data)),
                        });
                    } else {
                        return Err(anyhow!(
                            "Don't know how to get content type for {:?}",
                            image
                        ));
                    }
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
