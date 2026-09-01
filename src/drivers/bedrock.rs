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
use genai::chat::ReasoningEffort;
use uuid::Uuid;

use crate::{
    aws::load_aws_config,
    drivers::{ChatCompletionResponse, DriverError, LlmOpts, TokenUsage},
    images::{ImageEncoding, ImageFile},
    mem_limit::MemLimiter,
    prelude::*,
    prompt::{ChatPrompt, Message, Rendered},
    retry::IsKnownTransient,
};

use super::Driver;

/// The name of the tool we tell Bedrock to use for reporting results.
static OUTPUT_TOOL_NAME: &str = "report_result";

/// Our AWS Bedrock driver.
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
        prompt: &ChatPrompt<Rendered>,
        // TODO: Why do we get this separately from the copy in `prompt`?
        _schema: Value,
        llm_opts: &LlmOpts,
        mem_limiter: &MemLimiter,
    ) -> Result<ChatCompletionResponse, DriverError> {
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
        let req = prompt
            .to_bedrock_request(mem_limiter)
            .await
            .map_err(DriverError::invalid_input)?;

        // Send the request.
        let output = self
            .client
            .converse()
            .model_id(model)
            .inference_config(inf_conf)
            .tool_config(req.tool_config)
            .set_system(req.system.map(|s| vec![s]))
            .set_messages(Some(req.messages))
            .set_additional_model_request_fields(
                reasoning_request_fields(model, llm_opts.reasoning_effort.as_ref())
                    .map_err(DriverError::invalid_input)?,
            )
            .send()
            .await
            .map_err(DriverError::api)?;

        // Check for odd stop reasons.
        match output.stop_reason() {
            StopReason::ToolUse => {}
            // Reasoning tokens count against the token cap, so retrying the
            // identical request with the identical cap will almost always
            // truncate again. Fail fast so the operator can raise the cap.
            StopReason::MaxTokens => {
                return Err(DriverError::invalid_output(anyhow!(
                    "Bedrock response hit the max token limit before completing \
                     the tool call; raise --max-completion-tokens (on reasoning \
                     models, reasoning tokens count against it)"
                )));
            }
            other => {
                return Err(DriverError::invalid_output_transient(anyhow!(
                    "Unexpected stop reason: {other}"
                )));
            }
        }

        // Get the token usage.
        let token_usage = output.usage().map(|usage| TokenUsage {
            prompt_tokens: u64::try_from(usage.input_tokens).unwrap_or(0),
            completion_tokens: u64::try_from(usage.output_tokens).unwrap_or(0),
        });

        // Parse our converse output. This is an annoyingly multi-step process.
        let converse_output = output
            .output()
            .ok_or_else(|| anyhow!("Bedrock response did not contain any output"))
            .map_err(DriverError::invalid_output_transient)?;
        let message = converse_output
            .as_message()
            .map_err(|_| anyhow!("Bedrock response did not contain a message"))
            .map_err(DriverError::invalid_output_transient)?;
        let tool_use = find_output_tool_use(message.content())
            .map_err(DriverError::invalid_output_transient)?;
        let response = aws_document_to_value(&tool_use.input)
            .map_err(DriverError::invalid_output_transient)?;
        debug!(%response, "Response");
        Ok(ChatCompletionResponse {
            response,
            token_usage,
        })
    }
}

/// Build the `additionalModelRequestFields` for a reasoning-effort request.
///
/// Bedrock passes these fields straight through to the model provider, so the
/// reasoning knob's shape is per model family and an unrecognized field is a
/// hard `ValidationException`. OpenAI models take
/// `{"reasoning": {"effort": "none"|"low"|"medium"|"high"}}` (they reject
/// `"minimal"`). Other families ignore `--reasoning-effort` with a warning
/// until we verify their shapes against live Bedrock.
fn reasoning_request_fields(
    model: &str,
    reasoning_effort: Option<&ReasoningEffort>,
) -> Result<Option<Document>> {
    let Some(effort) = reasoning_effort else {
        return Ok(None);
    };
    if !is_openai_model(model) {
        warn!(
            %model,
            "--reasoning-effort is not supported for this model family on \
             Bedrock; ignoring it"
        );
        return Ok(None);
    }
    let effort_str = match effort {
        ReasoningEffort::None => "none",
        // OpenAI models on Bedrock reject "minimal"; "low" is the closest
        // accepted level, matching how `thinking_budget` treats the two.
        ReasoningEffort::Minimal | ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Budget(budget) => {
            return Err(anyhow!(
                "OpenAI models on Bedrock take a reasoning effort level, not a \
                 token budget ({budget}); use none, low, medium or high"
            ));
        }
    };
    Ok(Some(Document::Object(HashMap::from([(
        "reasoning".to_owned(),
        Document::Object(HashMap::from([(
            "effort".to_owned(),
            Document::String(effort_str.to_owned()),
        )])),
    )]))))
}

/// Does this Bedrock model ID name an OpenAI-family model?
///
/// Matches the `openai` vendor segment in IDs like `us.openai.gpt-5.6-luna`,
/// `openai.gpt-oss-120b-1:0`, and inference-profile ARNs ending in such IDs.
fn is_openai_model(model: &str) -> bool {
    model.split('.').any(|segment| segment == "openai")
}

/// Describe a message's content blocks without their contents.
///
/// Block payloads may hold customer documents, and these descriptions end up
/// in error messages that are logged on every retry attempt, so we report only
/// block kinds, text lengths, and tool names.
fn describe_blocks(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => format!("text({} chars)", text.chars().count()),
            ContentBlock::ToolUse(tool_use) => format!("toolUse({})", tool_use.name),
            ContentBlock::ReasoningContent(_) => "reasoningContent".to_owned(),
            _ => "other".to_owned(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Find the tool use block in which the model reported our result.
///
/// Reasoning models return an opaque `reasoningContent` block next to the tool
/// call, so we search the message for a call to our tool instead of requiring
/// the message to hold a single content block.
fn find_output_tool_use(blocks: &[ContentBlock]) -> Result<&ToolUseBlock> {
    let mut tool_uses = blocks.iter().filter_map(|block| match block {
        ContentBlock::ToolUse(tool_use) if tool_use.name == OUTPUT_TOOL_NAME => {
            Some(tool_use)
        }
        _ => None,
    });
    let tool_use = tool_uses.next().ok_or_else(|| {
        anyhow!(
            "Bedrock response contained no {OUTPUT_TOOL_NAME} tool use block: [{}]",
            describe_blocks(blocks)
        )
    })?;
    // We ask for a single report, so more than one is ambiguous.
    if tool_uses.next().is_some() {
        return Err(anyhow!(
            "Bedrock response contained multiple {OUTPUT_TOOL_NAME} tool use blocks: [{}]",
            describe_blocks(blocks)
        ));
    }
    // A text block next to the tool call may carry a caveat or refusal the
    // structured payload doesn't, so leave a trace when we ignore one.
    if blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::Text(_)))
    {
        warn!(
            blocks = %describe_blocks(blocks),
            "Ignoring text block next to the Bedrock tool call"
        );
    } else if blocks.len() > 1 {
        debug!(
            blocks = %describe_blocks(blocks),
            "Ignoring extra content blocks next to the Bedrock tool call"
        );
    }
    Ok(tool_use)
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

impl ChatPrompt<Rendered> {
    async fn to_bedrock_request(
        &self,
        mem_limiter: &MemLimiter,
    ) -> Result<BedrockRequest> {
        // Convert our messages.
        let mut messages = vec![];
        for message in &self.messages {
            messages.extend(message.to_bedrock_message(mem_limiter).await?);
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

impl Message {
    async fn to_bedrock_message(
        &self,
        mem_limiter: &MemLimiter,
    ) -> Result<Vec<BedrockMessage>> {
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
                    let image_file = ImageFile::from_url(image)?;
                    let image_data =
                        image_file.load(ImageEncoding::Binary, mem_limiter).await?;
                    let format = image_data
                        .mime_type()
                        .strip_prefix("image/")
                        .unwrap_or(image_data.mime_type());
                    let image_block = ImageBlock::builder()
                        .format(ImageFormat::try_parse(format)?)
                        .source(ImageSource::Bytes(Blob::new(image_data.data().to_vec())))
                        .build()
                        .context("Cannot build Bedrock image block")?;
                    builder = builder.content(ContentBlock::Image(image_block));
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

#[cfg(test)]
mod tests {
    use aws_sdk_bedrockruntime::types::ReasoningContentBlock;

    use super::*;

    /// Build a tool use block calling `name`.
    fn tool_use(name: &str) -> ContentBlock {
        ContentBlock::ToolUse(
            ToolUseBlock::builder()
                .tool_use_id("call_1")
                .name(name)
                .input(Document::String("input".to_owned()))
                .build()
                .expect("could not build tool use block"),
        )
    }

    /// Build the opaque reasoning block that reasoning models emit.
    fn redacted_reasoning() -> ContentBlock {
        ContentBlock::ReasoningContent(ReasoningContentBlock::RedactedContent(Blob::new(
            "opaque",
        )))
    }

    #[test]
    fn finds_a_lone_tool_use() {
        let blocks = vec![tool_use(OUTPUT_TOOL_NAME)];
        let tool_use = find_output_tool_use(&blocks).expect("should find tool use");
        assert_eq!(tool_use.name, OUTPUT_TOOL_NAME);
    }

    #[test]
    fn finds_a_tool_use_after_a_reasoning_block() {
        let blocks = vec![redacted_reasoning(), tool_use(OUTPUT_TOOL_NAME)];
        let tool_use = find_output_tool_use(&blocks).expect("should find tool use");
        assert_eq!(tool_use.name, OUTPUT_TOOL_NAME);
    }

    #[test]
    fn finds_a_tool_use_next_to_a_hallucinated_tool_call() {
        let blocks = vec![tool_use("some_other_tool"), tool_use(OUTPUT_TOOL_NAME)];
        let tool_use = find_output_tool_use(&blocks).expect("should find tool use");
        assert_eq!(tool_use.name, OUTPUT_TOOL_NAME);
    }

    #[test]
    fn rejects_a_message_with_no_tool_use() {
        let blocks = vec![redacted_reasoning(), ContentBlock::Text("hi".to_owned())];
        assert!(find_output_tool_use(&blocks).is_err());
    }

    #[test]
    fn rejects_multiple_tool_uses() {
        let blocks = vec![tool_use(OUTPUT_TOOL_NAME), tool_use(OUTPUT_TOOL_NAME)];
        assert!(find_output_tool_use(&blocks).is_err());
    }

    #[test]
    fn rejects_an_unexpected_tool_name() {
        let blocks = vec![tool_use("some_other_tool")];
        assert!(find_output_tool_use(&blocks).is_err());
    }

    /// Extract `reasoning.effort` from the generated request fields.
    fn effort_field(fields: &Document) -> &str {
        let Document::Object(fields) = fields else {
            panic!("expected an object, got {fields:?}");
        };
        let Some(Document::Object(reasoning)) = fields.get("reasoning") else {
            panic!("expected a reasoning object, got {fields:?}");
        };
        let Some(Document::String(effort)) = reasoning.get("effort") else {
            panic!("expected an effort string, got {reasoning:?}");
        };
        effort
    }

    #[test]
    fn maps_reasoning_effort_for_openai_models() {
        let cases = [
            (ReasoningEffort::None, "none"),
            (ReasoningEffort::Minimal, "low"),
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::High, "high"),
        ];
        for (effort, expected) in cases {
            let fields =
                reasoning_request_fields("us.openai.gpt-5.6-luna", Some(&effort))
                    .expect("should map effort")
                    .expect("should produce request fields");
            assert_eq!(effort_field(&fields), expected);
        }
    }

    #[test]
    fn rejects_token_budgets_for_openai_models() {
        let result = reasoning_request_fields(
            "us.openai.gpt-5.6-luna",
            Some(&ReasoningEffort::Budget(1000)),
        );
        assert!(result.is_err());
    }

    #[test]
    fn ignores_reasoning_effort_for_other_model_families() {
        let fields = reasoning_request_fields(
            "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            Some(&ReasoningEffort::Low),
        )
        .expect("should not error");
        assert!(fields.is_none());
    }

    #[test]
    fn recognizes_openai_model_ids() {
        assert!(is_openai_model("us.openai.gpt-5.6-luna"));
        assert!(is_openai_model("openai.gpt-oss-120b-1:0"));
        assert!(is_openai_model(
            "arn:aws:bedrock:us-east-2:123456789012:inference-profile/us.openai.gpt-5.6-luna"
        ));
        assert!(!is_openai_model("us.anthropic.claude-sonnet-5"));
        assert!(!is_openai_model("us.meta.llama4-scout-17b-instruct-v1:0"));
    }

    #[test]
    fn errors_and_descriptions_never_contain_block_contents() {
        let secret = "CONFIDENTIAL DOCUMENT TEXT";
        let blocks = vec![
            ContentBlock::Text(secret.to_owned()),
            redacted_reasoning(),
            tool_use("some_other_tool"),
        ];
        let description = describe_blocks(&blocks);
        assert!(!description.contains(secret));
        assert_eq!(
            description,
            "text(26 chars), reasoningContent, toolUse(some_other_tool)"
        );
        let error = find_output_tool_use(&blocks)
            .expect_err("should reject a message with no report_result call");
        assert!(!error.to_string().contains(secret));
    }
}
