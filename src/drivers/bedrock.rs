//! AWS Bedrock driver.

use aws_sdk_bedrockruntime::{
    Client,
    error::SdkError,
    operation::converse::ConverseError,
    primitives::Blob,
    types::{
        ContentBlock, ConversationRole, ImageBlock, ImageFormat, ImageSource,
        InferenceConfiguration, Message as BedrockMessage, StopReason,
        SystemContentBlock,
    },
};
use aws_smithy_runtime_api::http::StatusCode as AwsStatusCode;
use reqwest::StatusCode;

use crate::{
    aws::load_aws_config,
    data_url::parse_data_url,
    drivers::{ChatCompletionResponse, LlmOpts, LlmRetryResult, TokenUsage},
    litellm::LiteLlmModel,
    prelude::*,
    prompt::{ChatPrompt, Message, Rendered},
    retry::{
        IsKnownTransient, retry_result_ok, retry_result_transient, try_fatal,
        try_potentially_transient, try_transient,
    },
};

use super::Driver;

/// Our OpenAI driver, which we also use for LiteLLM, Ollama and other
/// compatible gateways.
#[derive(Debug)]
pub struct BedrockDriver {
    /// The Bedrock client.
    pub client: Client,
}

impl BedrockDriver {
    /// Create a new native driver.
    pub async fn new() -> Result<Self> {
        let config = load_aws_config().await?;
        Ok(Self {
            client: Client::new(&config),
        })
    }
}

#[async_trait]
impl Driver for BedrockDriver {
    #[instrument(level = "debug", skip_all)]
    async fn chat_completion(
        &self,
        model: &str,
        _model_info: Option<&LiteLlmModel>,
        prompt: &ChatPrompt<Rendered>,
        // TODO: Why do we get this separately from the copy in `prompt`?
        _schema: Value,
        llm_opts: &LlmOpts,
    ) -> LlmRetryResult<ChatCompletionResponse> {
        // Figure out of inference configuration.
        let mut inf_conf_builder = InferenceConfiguration::builder();
        if let Some(max_tokens) = llm_opts.max_completion_tokens {
            inf_conf_builder = inf_conf_builder.max_tokens(max_tokens as i32);
        }
        if let Some(temperature) = llm_opts.temperature {
            inf_conf_builder = inf_conf_builder.temperature(temperature);
        }
        if let Some(top_p) = llm_opts.top_p {
            inf_conf_builder = inf_conf_builder.top_p(top_p);
        }
        let inf_conf = inf_conf_builder.build();

        // Convert our prompt to a Bedrock request.
        let req = try_fatal!(prompt.to_bedrock_request().await);

        // Send the request.
        let output = try_potentially_transient!(
            self.client
                .converse()
                .model_id(model)
                .inference_config(inf_conf)
                .system(req.system)
                .set_messages(Some(req.messages))
                .send()
                .await
        );

        // Check for odd stop reasons.
        if output.stop_reason() != &StopReason::EndTurn {
            return LlmRetryResult::Transient {
                input: (),
                error: anyhow!("Unexpected stop reason: {}", output.stop_reason()),
            };
        }

        // Get the token usage.
        let token_usage = output.usage().map(|usage| TokenUsage {
            prompt_tokens: u64::try_from(usage.input_tokens).unwrap_or(0),
            completion_tokens: u64::try_from(usage.output_tokens).unwrap_or(0),
        });

        // Parse our converse output. This is an annoyingly multi-step process.
        let converse_output = try_transient!(
            output
                .output()
                .ok_or_else(|| anyhow!("Bedrock response did not contain any output"))
        );
        let message = try_transient!(
            converse_output
                .as_message()
                .map_err(|_| anyhow!("Bedrock response did not contain a message"))
        );
        let blocks = message.content();
        if blocks.len() != 1 {
            return retry_result_transient(anyhow!(
                "Bedrock response contained {} content blocks, expected 1",
                blocks.len()
            ));
        }
        if let ContentBlock::Text(text) = &blocks[0] {
            // Parse our response as JSON.
            let response =
                try_transient!(serde_json::from_str(text).map_err(|e| anyhow!(
                    "Failed to parse Bedrock response as JSON: {e}"
                )));
            debug!(%response, "Response");
            retry_result_ok(ChatCompletionResponse {
                response,
                token_usage,
            })
        } else {
            try_transient!(Err(anyhow!(
                "Bedrock response contained unexpected content block: {blocks:?}"
            )))
        }
    }
}

impl IsKnownTransient for SdkError<ConverseError> {
    fn is_known_transient(&self) -> bool {
        match self {
            SdkError::TimeoutError(_) => true,
            SdkError::DispatchFailure(dispatch) => {
                dispatch.is_io() || dispatch.is_timeout()
            }
            SdkError::ResponseError(response) => {
                response.raw().status().is_known_transient()
            }
            SdkError::ServiceError(service_err) => service_err.err().is_known_transient(),
            _ => false,
        }
    }
}

impl IsKnownTransient for AwsStatusCode {
    fn is_known_transient(&self) -> bool {
        // Convert this to a regular `StatusCode`, and use the standard implementation.
        match StatusCode::from_u16(self.as_u16()) {
            Ok(status) => status.is_known_transient(),
            Err(_) => false, // If we can't convert, assume it's not transient.
        }
    }
}

impl IsKnownTransient for ConverseError {
    fn is_known_transient(&self) -> bool {
        matches!(
            self,
            ConverseError::InternalServerException(_)
                | ConverseError::ModelNotReadyException(_)
                | ConverseError::ModelTimeoutException(_)
                | ConverseError::ServiceUnavailableException(_)
                | ConverseError::ThrottlingException(_)
        )
    }
}

/// Information needed for a Bedrock request.
struct BedrockRequest {
    /// The system prompt.
    system: SystemContentBlock,
    /// The messages to send.
    messages: Vec<BedrockMessage>,
}

/// Convert a type to a Bedrock request.
#[async_trait]
trait ToBedrockRequest {
    type Output;

    /// Convert to a Bedrock request.
    async fn to_bedrock_request(&self) -> Result<Self::Output>;
}

#[async_trait]
impl ToBedrockRequest for ChatPrompt<Rendered> {
    type Output = BedrockRequest;

    async fn to_bedrock_request(&self) -> Result<Self::Output> {
        // Build our system prompt.
        let mut system = String::new();
        if let Some(developer) = &self.developer {
            system.push_str(developer);
            system.push('\n');
        }

        // Append our schema to the system prompt. We have a choice between
        // including the schema in the text we send the model, or of using
        // function calling. In theory, function calling works slightly better
        // with some models, including Claude Haiku <= 3.5. But with s
        let schema = self.response_schema.to_json_schema().await?;
        system.push_str(
            "Your response should be a JSON object with the following schema:\n",
        );
        system.push_str(&serde_json::to_string(&schema)?);
        system.push('\n');

        // Convert our messages.
        let mut messages = vec![];
        for message in &self.messages {
            messages.push(message.to_bedrock_request().await?);
        }

        Ok(BedrockRequest {
            system: SystemContentBlock::Text(system),
            messages,
        })
    }
}

#[async_trait]
impl ToBedrockRequest for Message {
    type Output = BedrockMessage;

    async fn to_bedrock_request(&self) -> Result<Self::Output> {
        let mut builder = BedrockMessage::builder();
        match self {
            Message::User { text, images } => {
                builder = builder.role(ConversationRole::User);
                if let Some(text) = text {
                    builder = builder.content(ContentBlock::Text(text.clone()));
                }
                for image in images {
                    if let Some((mime_type, data)) = parse_data_url(image) {
                        let image_block = ImageBlock::builder()
                            .format(ImageFormat::try_parse(&mime_type)?)
                            .source(ImageSource::Bytes(Blob::new(data)))
                            .build()
                            .context("Cannot build Bedrock image block")?;
                        builder = builder.content(ContentBlock::Image(image_block));
                    } else {
                        return Err(anyhow!(
                            "Don't know how to get content type for {:?}",
                            image
                        ));
                    }
                }
            }
            Message::Assistant { json } => {
                builder = builder
                    .role(ConversationRole::Assistant)
                    .content(ContentBlock::Text(json.to_string()))
            }
        }
        builder.build().context("Cannot build Bedrock message")
    }
}
