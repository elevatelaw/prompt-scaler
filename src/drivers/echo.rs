//! Echo driver for testing.
//!
//! This driver echoes back the last user message as `{ "echo": <text> }`.
//! It validates that the response schema is an internal schema with a single
//! "echo" string property.

use async_trait::async_trait;
use serde_json::Map;

use crate::{
    prelude::*,
    prompt::{ChatPrompt, Message, Rendered},
    schema::{InternalSchema, InternalSchemaDetails, ScalarType, Schema},
};

use super::{
    ChatCompletionResponse, Driver, DriverError, LlmOpts, MemLimiter, TokenUsage,
};

/// Echo driver for testing.
#[derive(Debug)]
pub struct EchoDriver;

/// Validate that the schema matches the expected format:
/// An object with a single "echo" property of type string.
fn validate_schema(schema: &Schema) -> Result<()> {
    // Extract the properties.
    let properties = match schema {
        Schema::Internal(InternalSchema {
            details: InternalSchemaDetails::Object { properties, .. },
            ..
        }) => properties,
        _ => {
            return Err(anyhow!(
                "Echo driver requires an internal schema of type Object, not {schema:?}"
            ));
        }
    };

    // Check that there's exactly one property named "echo"
    if properties.len() != 1 {
        return Err(anyhow!(
            "Echo driver requires exactly one property in the schema, found {}",
            properties.len()
        ));
    }
    let echo_property = properties.get("echo").ok_or_else(|| {
        anyhow!("Echo driver requires a property named 'echo' in the schema")
    })?;

    // Check that the "echo" property is a string
    match &echo_property.details {
        InternalSchemaDetails::Scalar {
            r#type: ScalarType::String,
            r#enum: None,
        } => Ok(()),
        _ => Err(anyhow!(
            "Echo driver requires the 'echo' property to be a string type"
        )),
    }
}

/// Extract the text from the last user message.
fn extract_last_user_message(messages: &[Message]) -> Result<String> {
    // Find the last user message
    let last_user_message = messages
        .iter()
        .rev()
        .find_map(|msg| match msg {
            Message::User { text, .. } => text.as_ref(),
            _ => None,
        })
        .ok_or_else(|| anyhow!("No user message found in prompt"))?;

    Ok(last_user_message.clone())
}

impl EchoDriver {
    /// Create a new echo driver.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for EchoDriver {
    async fn chat_completion(
        &self,
        _model: &str,
        prompt: &ChatPrompt<Rendered>,
        _schema: Value,
        _llm_opts: &LlmOpts,
        _mem_limiter: &MemLimiter,
    ) -> Result<ChatCompletionResponse, DriverError> {
        // Validate the schema
        validate_schema(&prompt.response_schema).map_err(DriverError::invalid_input)?;

        // Extract the last user message
        let text = extract_last_user_message(&prompt.messages)
            .map_err(DriverError::invalid_input)?;

        // Build the response JSON
        let mut response = Map::new();
        response.insert("echo".to_string(), Value::String(text));

        Ok(ChatCompletionResponse {
            response: Value::Object(response),
            token_usage: Some(TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
            }),
        })
    }
}

// We focus on testing the "sad paths", because the happy path is tested by our
// integration tests.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_schema_valid() {
        let schema_toml = r#"
            description = "Echo response"

            [properties.echo]
            description = "The echoed text"
            type = "string"
        "#;
        let schema: Schema = crate::toml_utils::from_toml_str(schema_toml).unwrap();
        assert!(validate_schema(&schema).is_ok());
    }

    #[test]
    fn test_validate_schema_invalid_cases() {
        let cases = [
            (
                r#"
                    description = "Multiple properties"
                    [properties.echo]
                    description = "Echo field"
                    type = "string"
                    [properties.extra]
                    description = "Extra field"
                    type = "string"
                "#,
                "exactly one property",
            ),
            (
                r#"
                    description = "Wrong property name"
                    [properties.wrong_name]
                    description = "Wrong field"
                    type = "string"
                "#,
                "property named 'echo'",
            ),
            (
                r#"
                    description = "Wrong type"
                    [properties.echo]
                    description = "Echo as number"
                    type = "number"
                "#,
                "string type",
            ),
            (
                r#"
                    description = "Array not object"
                    [items]
                    description = "Item"
                    type = "string"
                "#,
                "internal schema of type Object",
            ),
        ];

        for (schema_toml, expected_error) in cases {
            let schema: Schema = crate::toml_utils::from_toml_str(schema_toml).unwrap();
            let err = validate_schema(&schema).unwrap_err().to_string();
            assert!(
                err.contains(expected_error),
                "Expected error to contain '{}', but got: {}",
                expected_error,
                err
            );
        }

        // Also test JsonValue variant (can't be deserialized from TOML)
        let schema = Schema::JsonValue(json!({"type": "object"}));
        let err = validate_schema(&schema).unwrap_err();
        assert!(err.to_string().contains("internal schema"));
    }

    #[test]
    fn test_extract_last_user_message_no_user_message() {
        let messages = vec![Message::Assistant {
            json: json!({"echo": "test"}),
        }];
        let err = extract_last_user_message(&messages).unwrap_err();
        assert!(err.to_string().contains("No user message"));
    }
}
