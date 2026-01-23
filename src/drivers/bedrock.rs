//! AWS Bedrock driver.

use std::collections::HashMap;

use aws_sdk_bedrockruntime::{
    Client,
    operation::converse::ConverseError,
    primitives::Blob,
    types::{
        AnyToolChoice, ContentBlock, ConversationRole, ImageBlock, ImageFormat,
        ImageSource, InferenceConfiguration, Message as BedrockMessage, StopReason,
        SystemContentBlock, Tool, ToolChoice, ToolConfiguration, ToolInputSchema,
        ToolResultBlock, ToolResultContentBlock, ToolSpecification, ToolUseBlock,
    },
};
use aws_smithy_types::{Document, Number};
use base64::{Engine as _, prelude::BASE64_STANDARD};
use uuid::Uuid;

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

/// The name of the tool we tell Bedrock to use for reporting results.
static OUTPUT_TOOL_NAME: &str = "report_result";

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
                .tool_config(req.tool_config)
                .set_system(req.system.map(|s| vec![s]))
                .set_messages(Some(req.messages))
                .send()
                .await
        );

        // Check for odd stop reasons.
        if output.stop_reason() != &StopReason::ToolUse {
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
        if let ContentBlock::ToolUse(tool_use) = &blocks[0] {
            if tool_use.name != OUTPUT_TOOL_NAME {
                return retry_result_transient(anyhow!(
                    "Bedrock response contained unexpected tool name: {}",
                    tool_use.name
                ));
            }
            let response = try_transient!(aws_document_to_value(&tool_use.input));
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
    system: Option<SystemContentBlock>,
    /// Our tool configuration.
    tool_config: ToolConfiguration,
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
        // Convert our messages.
        let mut messages = vec![];
        for message in &self.messages {
            messages.extend(message.to_bedrock_request().await?);
        }

        // Set up our tool configuration, and for
        let tool_config = ToolConfiguration::builder()
            .tools(Tool::ToolSpec(
                ToolSpecification::builder()
                    .name(OUTPUT_TOOL_NAME.to_string())
                    .description("Report the requested data".to_string())
                    .input_schema(ToolInputSchema::Json(
                        value_to_aws_document(
                            &self.response_schema.to_json_schema().await?,
                        )
                        .context("Cannot convert JSON to AWS Document")?,
                    ))
                    .build()
                    .context("Cannot build Bedrock tool specification")?,
            ))
            // We have only one tool, so force the model to _some_ tool, and it
            // has to call ours. This is more portable than SpecificToolChoice.
            .tool_choice(ToolChoice::Any(AnyToolChoice::builder().build()))
            .build()
            .context("Cannot build Bedrock tool configuration")?;

        Ok(BedrockRequest {
            system: self
                .developer
                .as_ref()
                .map(|developer| SystemContentBlock::Text(developer.to_owned())),
            tool_config,
            messages,
        })
    }
}

#[async_trait]
impl ToBedrockRequest for Message {
    type Output = Vec<BedrockMessage>;

    async fn to_bedrock_request(&self) -> Result<Self::Output> {
        let mut messages = vec![];
        match self {
            Message::User { text, images } => {
                let mut builder = BedrockMessage::builder().role(ConversationRole::User);
                if let Some(text) = text {
                    if text.is_empty() || text.chars().all(|c| c.is_ascii_whitespace()) {
                        // The Bedrock models we've tested don't like blank user messages
                        // and they will return an error, so bail on it now.
                        return Err(anyhow!(
                            "User message is blank, which is not supported: {:?}",
                            text
                        ));
                    }
                    builder = builder.content(ContentBlock::Text(text.clone()));
                }
                for image in images {
                    if let Some((mime_type, data)) = parse_data_url(image) {
                        let format =
                            mime_type.strip_prefix("image/").unwrap_or(&mime_type);
                        let decoded_bytes = BASE64_STANDARD
                            .decode(data)
                            .context("Cannot decode base64 image data")?;
                        let image_block = ImageBlock::builder()
                            .format(ImageFormat::try_parse(format)?)
                            .source(ImageSource::Bytes(Blob::new(decoded_bytes)))
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
                messages.push(builder.build().context("Cannot build Bedrock message")?);
            }
            Message::Assistant { json } => {
                // We need to generate a tool use and a tool result, because Bedrock
                let id = Uuid::new_v4().to_string();
                messages.push(
                    BedrockMessage::builder()
                        .role(ConversationRole::Assistant)
                        .content(ContentBlock::ToolUse(
                            ToolUseBlock::builder()
                                .tool_use_id(id.clone())
                                .name(OUTPUT_TOOL_NAME.to_string())
                                .input(
                                    value_to_aws_document(json)
                                        .context("Cannot convert JSON to AWS Document")?,
                                )
                                .build()
                                .context("Cannot build Bedrock tool use block")?,
                        ))
                        .build()
                        .context("Cannot build Bedrock message")?,
                );
                messages.push(
                    BedrockMessage::builder()
                        .role(ConversationRole::User)
                        .content(ContentBlock::ToolResult(
                            ToolResultBlock::builder()
                                .tool_use_id(id)
                                .content(ToolResultContentBlock::Json(Document::Object(
                                    HashMap::from([(
                                        "status".to_string(),
                                        Document::from("ok"),
                                    )]),
                                )))
                                .build()
                                .context("Cannot build Bedrock message")?,
                        ))
                        .build()
                        .context("Cannot build Bedrock message")?,
                );
            }
        }
        Ok(messages)
    }
}

/// Convert a [`serde_json::Value`] into an [`aws_smithy_types::Document`].
fn value_to_aws_document(value: &serde_json::Value) -> Result<Document> {
    match value {
        serde_json::Value::Object(map) => {
            let mut obj = HashMap::new();
            for (key, val) in map {
                obj.insert(key.clone(), value_to_aws_document(val)?);
            }
            Ok(Document::Object(obj))
        }
        serde_json::Value::Array(arr) => {
            let docs = arr
                .iter()
                .map(value_to_aws_document)
                .collect::<Result<Vec<_>>>()?;
            Ok(Document::from(docs))
        }
        Value::Null => Ok(Document::Null),
        Value::Bool(b) => Ok(Document::from(*b)),
        Value::String(s) => Ok(Document::from(s.clone())),
        Value::Number(num) => {
            if let Some(i) = num.as_i64() {
                Ok(Document::from(i))
            } else if let Some(u) = num.as_u64() {
                Ok(Document::from(u))
            } else if let Some(f) = num.as_f64() {
                Ok(Document::from(f))
            } else {
                Err(anyhow!("Unsupported number type: {}", num))
            }
        }
    }
}

// Convert a [`aws_smithy_types::Document`] into a [`serde_json::Value`].
fn aws_document_to_value(doc: &Document) -> Result<serde_json::Value> {
    match doc {
        Document::Object(map) => {
            let mut obj = serde_json::Map::new();
            for (key, val) in map {
                obj.insert(key.clone(), aws_document_to_value(val)?);
            }
            Ok(serde_json::Value::Object(obj))
        }
        Document::Array(arr) => {
            let vals = arr
                .iter()
                .map(aws_document_to_value)
                .collect::<Result<Vec<_>>>()?;
            Ok(serde_json::Value::Array(vals))
        }
        Document::Null => Ok(serde_json::Value::Null),
        Document::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        Document::String(s) => Ok(serde_json::Value::String(s.clone())),
        Document::Number(num) => match num {
            Number::PosInt(value) => {
                Ok(serde_json::Value::Number(serde_json::Number::from(*value)))
            }
            Number::NegInt(value) => {
                Ok(serde_json::Value::Number(serde_json::Number::from(*value)))
            }
            Number::Float(value) => Ok(serde_json::Value::Number(
                serde_json::Number::from_f64(*value).ok_or_else(|| {
                    anyhow!("Cannot convert f64 to JSON number: {}", value)
                })?,
            )),
        },
    }
}
