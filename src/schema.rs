//! Schema support.

use std::collections::HashMap;

use serde_json::Map;

use crate::{io::read_json_or_toml, prelude::*};

/// Either an external or an internal schema.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged, deny_unknown_fields, rename_all = "snake_case")]
pub enum Schema {
    /// An external schema, provided as a URL.
    External {
        /// The path to the schema.
        path: PathBuf,
    },

    /// An internal schema, using a very simplified version of JSON Schema
    /// format. If this is insufficient for your needs, consider using an
    /// external schema.
    Internal(SimpleSchema),
}

impl Schema {
    /// Convert to a JSON Schema.
    pub async fn to_json_schema(&self) -> Result<Value> {
        match self {
            Schema::External { path } => read_json_or_toml::<Value>(path).await,
            Schema::Internal(schema) => {
                let mut schema_json = schema.to_json_schema()?;
                schema_json["$schema"] =
                    Value::String("http://json-schema.org/draft-07/schema#".to_string());
                Ok(schema_json)
            }
        }
    }
}

/// A simplified version of JSON Schema, used for validation.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct SimpleSchema {
    /// A description of this value.
    pub description: String,

    /// The details of this schema.
    #[serde(flatten)]
    pub details: SimpleSchemaDetails,
}

/// The details of a schema.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged, deny_unknown_fields, rename_all = "snake_case")]
pub enum SimpleSchemaDetails {
    /// An array.
    Array {
        /// The items in the array.
        items: Box<SimpleSchema>,
    },
    /// A JSON object.
    ///
    /// All fields will be automatically marked as required, and
    /// `additionalProperties` will be set to `false`.
    Object {
        /// The properties of the object.
        properties: HashMap<String, SimpleSchema>,

        /// The title of this object.
        #[serde(default)]
        title: Option<String>,
    },
    Scalar {
        /// The type of this scalar.
        #[serde(default)]
        r#type: ScalarType,
    },
}

/// Basic types we support.
#[derive(Debug, Default, Clone, Deserialize, PartialEq)]
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

/// Convert to a JSON Schema.
pub trait ToJsonSchema {
    /// Convert this schema to a JSON Schema.
    fn to_json_schema(&self) -> Result<Value>;
}

impl ToJsonSchema for SimpleSchema {
    fn to_json_schema(&self) -> Result<Value> {
        let description = Value::String(self.description.clone());
        match &self.details {
            SimpleSchemaDetails::Array { items } => {
                let mut schema = json!({
                    "type": "array",
                    "items": items.to_json_schema()?,
                });
                schema["description"] = description;
                Ok(schema)
            }
            SimpleSchemaDetails::Object { title, properties } => {
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
            SimpleSchemaDetails::Scalar { r#type } => {
                let mut schema = json!({
                    "type": r#type.to_json_schema()?,
                });
                schema["description"] = description;
                Ok(schema)
            }
        }
    }
}

impl ToJsonSchema for HashMap<String, SimpleSchema> {
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
    use super::*;

    #[test]
    fn test_external_schema() {
        let schema = json!({
            "path": "tests/fixtures/external_schemas/schema_ts.json",
        });
        let schema: Schema = serde_json::from_value(schema).unwrap();
        let expected = Schema::External {
            path: "tests/fixtures/external_schemas/schema_ts.json".into(),
        };
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
        let schema: SimpleSchema = toml::from_str(schema_toml).unwrap();
        let schema_json = schema.to_json_schema().unwrap();
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
}
