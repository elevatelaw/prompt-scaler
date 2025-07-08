//! Schema support.

use std::{borrow::Cow, collections::HashMap};

use schemars::{JsonSchema, SchemaGenerator, r#gen::SchemaSettings};
use serde_json::Map;
use toml_span::{
    DeserError,
    de_helpers::{TableHelper, expected},
    value::ValueInner,
};

use crate::{
    async_utils::io::read_json_or_toml_as_json_value,
    prelude::*,
    toml_utils::{JsonValue, custom_deser_error},
};

/// Get the title of a JSON Schema part, or `"ResponseFormat"` if not present.
pub fn get_schema_title(schema: &Value) -> String {
    schema
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("ResponseFormat")
        .to_owned()
}

/// Either an external or an internal schema.
#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq)]
#[serde(untagged, deny_unknown_fields, rename_all = "snake_case")]
pub enum Schema {
    /// An internal schema (one stored in the prompt file), using a very
    /// simplified version of JSON Schema format. If this is insufficient for
    /// your needs, consider using an external schema.
    Internal(InternalSchema),

    /// An external schema, provided as a file path.
    External(ExternalSchema),

    /// A schema provided as a [`Value`]. This is only used by Rust code
    /// that already has the schema in memory, and it cannot be deserialized.
    #[serde(skip_deserializing)]
    JsonValue(Value),
}

impl Schema {
    /// Create a [`Schema`] from a Rust type using [`schemars`].
    pub fn from_type<T>() -> Self
    where
        T: JsonSchema,
    {
        // Gemini 2.0 Flash doesn't like `definitions` in the schema,
        // so inline all subschemas.
        let mut settings = SchemaSettings::draft07();
        settings.inline_subschemas = true;
        let generator = SchemaGenerator::new(settings);

        let schema = generator.into_root_schema_for::<T>();
        let json =
            serde_json::to_value(schema).expect("failed to convert schema to JSON");
        Self::JsonValue(json)
    }

    /// Convert to a JSON Schema.
    pub async fn to_json_schema(&self) -> Result<Value> {
        match self {
            Schema::Internal(schema) => {
                let mut schema_json = schema.to_json_schema()?;
                schema_json["$schema"] =
                    Value::String("http://json-schema.org/draft-07/schema#".to_string());
                Ok(schema_json)
            }
            Schema::External(schema) => schema.to_json_schema().await,
            Schema::JsonValue(json) => Ok(json.clone()),
        }
    }
}

// We implement [`toml_span::Deserialize`] because it provides much better error messages
// than [`serde`].
impl<'de> toml_span::Deserialize<'de> for Schema {
    fn deserialize(value: &mut toml_span::Value<'de>) -> Result<Self, DeserError> {
        if value.has_key("path") {
            let external =
                <ExternalSchema as toml_span::Deserialize>::deserialize(value)?;
            Ok(Schema::External(external))
        } else {
            let internal =
                <InternalSchema as toml_span::Deserialize>::deserialize(value)?;
            Ok(Schema::Internal(internal))
        }
    }
}

/// An external schema, provided as a file path.
#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ExternalSchema {
    /// The path to the schema.
    path: PathBuf,
}

impl ExternalSchema {
    /// Convert to a JSON Schema.
    pub async fn to_json_schema(&self) -> Result<Value> {
        read_json_or_toml_as_json_value(&self.path).await
    }
}

impl<'de> toml_span::Deserialize<'de> for ExternalSchema {
    fn deserialize(value: &mut toml_span::Value<'de>) -> Result<Self, DeserError> {
        let mut helper = TableHelper::new(value)?;
        let spanned_path = helper.required_s::<String>("path")?;
        let path = PathBuf::from(spanned_path.value);
        if !path.exists() {
            let err_kind = toml_span::ErrorKind::Custom(Cow::Owned(format!(
                "File not found: {}",
                path.display()
            )));
            let err = toml_span::Error::from((err_kind, spanned_path.span));
            return Err(DeserError::from(err));
        }
        helper.finalize(None)?;
        Ok(ExternalSchema { path })
    }
}

/// A simplified version of JSON Schema, used for validation.
#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct InternalSchema {
    /// A description of this value.
    pub description: String,

    /// The details of this schema.
    #[serde(flatten)]
    pub details: InternalSchemaDetails,
}

impl<'de> toml_span::Deserialize<'de> for InternalSchema {
    fn deserialize(value: &mut toml_span::Value<'de>) -> Result<Self, DeserError> {
        if value.has_key("items") {
            let mut helper = TableHelper::new(value)?;
            let description = helper.required::<String>("description")?;
            let items = helper.required::<InternalSchema>("items")?;
            helper.finalize(None)?;
            Ok(InternalSchema {
                description,
                details: InternalSchemaDetails::Array {
                    items: Box::new(items),
                },
            })
        } else if value.has_key("properties") {
            // Recursively deserialize `properties`, which is a bit clunky.
            let value_inner = value.take();
            let ValueInner::Table(mut value_table) = value_inner else {
                return Err(expected("a table", value_inner, value.span).into());
            };
            let toml_properties = value_table
                .remove("properties")
                .expect("properties should be present")
                .take();
            let ValueInner::Table(toml_properties) = toml_properties else {
                return Err(expected("a table", toml_properties, value.span).into());
            };
            let mut properties = HashMap::new();
            for (k, mut v) in toml_properties {
                properties.insert(
                    k.name.into_owned(),
                    <InternalSchema as toml_span::Deserialize>::deserialize(&mut v)?,
                );
            }
            // Put back the `value_table` without the `"properties"` key.
            value.set(ValueInner::Table(value_table));

            // Handle our other fields normally.
            let mut helper = TableHelper::new(value)?;
            let description = helper.required::<String>("description")?;
            let title = helper.optional::<String>("title");
            helper.finalize(None)?;
            Ok(InternalSchema {
                description,
                details: InternalSchemaDetails::Object { properties, title },
            })
        } else {
            let mut helper = TableHelper::new(value)?;
            let description = helper.required::<String>("description")?;
            let r#type = helper.optional::<ScalarType>("type");
            let r#enum = helper.optional::<Vec<JsonValue>>("enum");
            helper.finalize(None)?;
            Ok(InternalSchema {
                description,
                details: InternalSchemaDetails::Scalar {
                    r#type: r#type.unwrap_or_default(),
                    r#enum: r#enum
                        .map(|v| v.into_iter().map(|v| v.into_json()).collect()),
                },
            })
        }
    }
}

/// The details of a schema.
#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq)]
#[serde(untagged, deny_unknown_fields, rename_all = "snake_case")]
pub enum InternalSchemaDetails {
    /// An array.
    Array {
        /// The items in the array.
        items: Box<InternalSchema>,
    },
    /// A JSON object.
    ///
    /// All fields will be automatically marked as required, and
    /// `additionalProperties` will be set to `false`.
    Object {
        /// The properties of the object.
        properties: HashMap<String, InternalSchema>,

        /// The title of this object.
        #[serde(default)]
        title: Option<String>,
    },
    Scalar {
        /// The type of this scalar.
        #[serde(default)]
        r#type: ScalarType,

        /// Allowed enum values.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        r#enum: Option<Vec<Value>>,
    },
}

/// Basic types we support.
#[derive(Debug, Default, Clone, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum ScalarType {
    /// A string.
    #[default]
    String,

    /// A number.
    Number,

    /// A boolean.
    Boolean,
}

impl<'de> toml_span::Deserialize<'de> for ScalarType {
    fn deserialize(value: &mut toml_span::Value<'de>) -> Result<Self, DeserError> {
        let r#type = value.as_str().ok_or_else(|| {
            custom_deser_error(value.span, "Expected a string for type")
        })?;
        match r#type {
            "string" => Ok(ScalarType::String),
            "number" => Ok(ScalarType::Number),
            "boolean" => Ok(ScalarType::Boolean),
            _ => Err(custom_deser_error(
                value.span,
                format!("Unsupported JSON Schema scalar type: {}", r#type),
            )),
        }
    }
}

/// Convert to a JSON Schema.
pub trait ToJsonSchema {
    /// Convert this schema to a JSON Schema.
    fn to_json_schema(&self) -> Result<Value>;
}

impl ToJsonSchema for InternalSchema {
    fn to_json_schema(&self) -> Result<Value> {
        let description = Value::String(self.description.clone());
        match &self.details {
            InternalSchemaDetails::Array { items } => {
                let mut schema = json!({
                    "type": "array",
                    "items": items.to_json_schema()?,
                });
                schema["description"] = description;
                Ok(schema)
            }
            InternalSchemaDetails::Object { title, properties } => {
                let mut schema = json!({
                    "type": "object",
                    "properties": properties.to_json_schema()?,
                    // OpenAI requires `additionalProperties` to be false.
                    "additionalProperties": false,
                    // OpenAI requires all properties to be required.
                    "required": properties.keys().cloned().collect::<Vec<_>>(),
                });
                if let Some(title) = title {
                    schema["title"] = Value::String(title.clone());
                }
                schema["description"] = description;
                Ok(schema)
            }
            InternalSchemaDetails::Scalar { r#type, r#enum } => {
                let mut schema = json!({
                    "type": r#type.to_json_schema()?,
                });
                if let Some(enum_values) = r#enum {
                    schema["enum"] = Value::Array(enum_values.clone());
                }
                schema["description"] = description;
                Ok(schema)
            }
        }
    }
}

impl ToJsonSchema for HashMap<String, InternalSchema> {
    fn to_json_schema(&self) -> Result<Value> {
        let mut properties = Map::new();
        for (key, value) in self {
            properties.insert(key.clone(), value.to_json_schema()?);
        }
        Ok(Value::Object(properties))
    }
}

impl ToJsonSchema for ScalarType {
    fn to_json_schema(&self) -> Result<Value> {
        let r#type = match self {
            ScalarType::String => "string",
            ScalarType::Number => "number",
            ScalarType::Boolean => "boolean",
        };
        Ok(Value::String(r#type.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use crate::toml_utils::from_toml_str;

    use super::*;

    #[test]
    fn test_external_schema() {
        let schema = json!({
            "path": "tests/fixtures/external_schemas/schema_ts.json",
        });
        let schema: Schema = serde_json::from_value(schema).unwrap();
        let expected = Schema::External(ExternalSchema {
            path: "tests/fixtures/external_schemas/schema_ts.json".into(),
        });
        assert_eq!(schema, expected);
    }

    #[test]
    fn test_internal_schema() {
        let schema_toml = r#"
description = "Information to extract from each image."

[properties.sign_text]
description = "Text appearing on the sign in the image."

[properties.sign_holder]
description = "A one-word description of the entity holding the sign."
"#;
        let schema = from_toml_str::<InternalSchema>(schema_toml).unwrap();
        let mut schema_json = schema.to_json_schema().unwrap();
        // Sort `required` properties to make the test deterministic.
        if let required @ Value::Array(_) = &mut schema_json["required"] {
            let mut strs =
                serde_json::from_value::<Vec<String>>(required.clone()).unwrap();
            strs.sort();
            *required = serde_json::to_value(strs).unwrap();
        }
        let expected_json = json!({
            "description": "Information to extract from each image.",
            "type": "object",
            "properties": {
                "sign_text": {
                    "description": "Text appearing on the sign in the image.",
                    "type": "string"
                },
                "sign_holder": {
                    "description": "A one-word description of the entity holding the sign.",
                    "type": "string"
                }
            },
            "additionalProperties": false,
            "required": ["sign_holder", "sign_text"],
        });
        assert_eq!(schema_json, expected_json);
    }

    #[test]
    fn test_internal_schema_error() {
        let schema_toml = r#"
description = "Information to extract from each image."

[properties.sign_text]
# No description.
"#;
        let result = from_toml_str::<InternalSchema>(schema_toml);
        assert!(result.is_err(), "Expected error, got: {:?}", result);
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("description"),
            "Unexpected error message: {}",
            msg
        );
    }
}
