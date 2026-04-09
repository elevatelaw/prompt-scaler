//! Vertex AI LLM driver.

use std::env;

use async_trait::async_trait;
use google_cloud_aiplatform_v1 as vertexai;
use google_cloud_gax::error::rpc::Code;
use vertexai::{
    client::PredictionService,
    model::{Blob, Content, GenerationConfig, Part},
};

use crate::{
    drivers::{ChatCompletionResponse, Driver, DriverError, LlmOpts, TokenUsage},
    images::{ImageEncoding, ImageFile},
    litellm::LiteLlmModel,
    mem_limit::MemLimiter,
    prelude::*,
    prompt::{ChatPrompt, Message, Rendered},
    retry::IsKnownTransient,
};

/// Our Vertex AI driver.
#[derive(Debug)]
pub struct VertexDriver {
    /// The Vertex AI client.
    pub client: PredictionService,

    /// Our GCP project ID.
    pub project_id: String,
}

impl VertexDriver {
    /// Create a new Vertex AI driver.
    pub async fn new() -> Result<Self> {
        let client = PredictionService::builder()
            .build()
            .await
            .context("Failed to create Vertex AI client")?;
        let project_id = env::var("GCP_PROJECT")
            .context("GCP_PROJECT environment variable is not set")?;
        Ok(Self { client, project_id })
    }
}

#[async_trait]
impl Driver for VertexDriver {
    #[instrument(level = "debug", skip_all)]
    async fn chat_completion(
        &self,
        model: &str,
        _model_info: Option<&LiteLlmModel>,
        prompt: &ChatPrompt<Rendered>,
        schema: Value,
        llm_opts: &LlmOpts,
        mem_limiter: &MemLimiter,
    ) -> Result<ChatCompletionResponse, DriverError> {
        // Convert our prompt to Vertex AI format.
        let (system_instruction, contents) = prompt
            .to_vertex_contents(mem_limiter)
            .await
            .map_err(DriverError::invalid_input)?;
        trace!(?contents, "Vertex request");

        // Set up generation config.
        let mut generation_config = GenerationConfig::new()
            .set_response_mime_type("application/json")
            .set_response_json_schema(schema);
        if let Some(max_tokens) = llm_opts.max_completion_tokens {
            generation_config =
                generation_config.set_max_output_tokens(max_tokens as i32);
        }
        if let Some(temperature) = llm_opts.temperature {
            generation_config = generation_config.set_temperature(temperature);
        }
        if let Some(top_p) = llm_opts.top_p {
            generation_config = generation_config.set_top_p(top_p);
        }

        // Get our full model name.
        let model_path = format!(
            "projects/{project_id}/locations/global/publishers/google/models/{model}",
            project_id = self.project_id,
        );

        // Send the request.
        let request = self
            .client
            .generate_content()
            .set_model(model_path)
            .set_or_clear_system_instruction(system_instruction)
            .set_contents(contents)
            .set_generation_config(generation_config);

        let response = request.send().await.map_err(DriverError::api)?;
        trace!(?response, "Vertex response");

        // Extract the response. Some of these fatal errors might in fact be transients, but
        // we'll only find that out by running the code on large numbers of inputs.
        let candidate = response
            .candidates
            .first()
            .ok_or_else(|| anyhow!("Vertex AI response did not contain any candidates"))
            .map_err(DriverError::invalid_output)?;
        let response_content = candidate
            .content
            .as_ref()
            .ok_or_else(|| anyhow!("Vertex AI response did not contain any content"))
            .map_err(DriverError::invalid_output)?;

        // Find the assistant's text response.
        let response_text = extract_assistant_text(response_content)
            .map_err(DriverError::invalid_output_transient)?;

        // Parse the response as JSON. If this fails, it means Google didn't
        // follow our schema, which is weird. But we'll retry it.
        let response_json = serde_json::from_str::<Value>(&response_text)
            .with_context(|| {
                format!(
                    "Failed to parse Vertex AI response as JSON: {}",
                    response_text
                )
            })
            .map_err(DriverError::invalid_output_transient)?;
        debug!(json = %response_json, "Vertex response JSON");

        // Get token usage if available.
        let token_usage = response.usage_metadata.map(|usage| TokenUsage {
            prompt_tokens: usage.prompt_token_count.try_into().unwrap_or(0),
            completion_tokens: usage.thoughts_token_count.try_into().unwrap_or(0)
                + usage.candidates_token_count.try_into().unwrap_or(0),
        });

        Ok(ChatCompletionResponse {
            response: response_json,
            token_usage,
        })
    }
}

impl IsKnownTransient for vertexai::Error {
    fn is_known_transient(&self) -> bool {
        if let Some(status) = self.status()
            && status.code.is_known_transient()
        {
            true
        } else {
            self.is_timeout() || self.is_exhausted()
        }
    }
}

impl IsKnownTransient for Code {
    fn is_known_transient(&self) -> bool {
        matches!(
            self,
            Code::DeadlineExceeded
                | Code::ResourceExhausted
                | Code::Internal
                | Code::Unavailable
        )
    }
}

/// Convert a [`ChatPrompt<Rendered>`] to Vertex AI contents.
impl ChatPrompt<Rendered> {
    async fn to_vertex_contents(
        &self,
        mem_limiter: &MemLimiter,
    ) -> Result<(Option<Content>, Vec<Content>)> {
        let mut contents = vec![];

        // Add developer/system message if present
        let system_instruction = self.developer.as_ref().map(|d| {
            Content::new()
                .set_role("model")
                .set_parts([Part::new().set_text(d)])
        });

        // Convert our messages
        for message in &self.messages {
            contents.extend(message.to_vertex_content(mem_limiter).await?);
        }

        Ok((system_instruction, contents))
    }
}

impl Message {
    async fn to_vertex_content(&self, mem_limiter: &MemLimiter) -> Result<Vec<Content>> {
        let mut contents = vec![];
        match self {
            Message::User { text, images } => {
                let mut parts = vec![];

                // Add text if present.
                if let Some(text) = text {
                    // TODO: We may want to move this validation into the prompt.
                    if text.trim().is_empty() {
                        return Err(anyhow!("User message has empty text"));
                    }
                    parts.push(Part::new().set_text(text));
                }

                // Add images if present.
                for image in images {
                    let image_file = ImageFile::from_url(image)?;
                    let image_data =
                        image_file.load(ImageEncoding::Binary, mem_limiter).await?;
                    parts.push(
                        Part::new().set_inline_data(
                            Blob::new()
                                .set_mime_type(image_data.mime_type())
                                .set_data(image_data.data().to_vec()),
                        ),
                    );
                }
                assert!(
                    !parts.is_empty(),
                    "User message has no content, which should be caught by prompt validation"
                );
                contents.push(Content::new().set_role("user").set_parts(parts));
            }
            Message::Assistant { json } => {
                // Convert JSON to string for the assistant response
                let json_str = serde_json::to_string(json)
                    .context("Failed to serialize assistant response as JSON")?;
                contents.push(
                    Content::new()
                        .set_role("model")
                        .set_parts([Part::new().set_text(&json_str)]),
                );
            }
        }
        Ok(contents)
    }
}

/// Extract text content from assistant messages.
fn extract_assistant_text(content: &Content) -> Result<String> {
    if content.role != "model" {
        return Err(anyhow!(
            "Vertex AI response content role is not 'model': {}",
            content.role
        ));
    }
    for part in &content.parts {
        if let Some(text) = part.text() {
            return Ok(text.clone());
        }
    }
    Err(anyhow!("No text response found in Vertex AI response"))
}
