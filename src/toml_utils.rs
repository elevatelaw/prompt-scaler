//! Helper functions to [`toml_span`].

use std::borrow::Cow;

use toml_span::{DeserError, value::ValueInner};

use crate::prelude::*;

/// Deserialize a TOML string into a value of the specified type.
pub fn from_toml_str<T>(toml_str: &str) -> Result<T, DeserError>
where
    T: toml_span::Deserialize<'static>,
{
    let mut value = toml_span::de::parse(toml_str)?.into_static_value();
    T::deserialize(&mut value)
}

/// Convert a `toml_span::Value<'_>` to a `toml_span::Value<'static>`.
pub trait IntoStaticValue {
    /// Our output type.
    type Output;

    /// Convert to a static value.
    fn into_static_value(self) -> Self::Output;
}

impl IntoStaticValue for toml_span::Value<'_> {
    type Output = toml_span::Value<'static>;

    fn into_static_value(mut self) -> Self::Output {
        let inner = self.take().into_static_value();
        toml_span::Value::with_span(inner, self.span)
    }
}

impl IntoStaticValue for ValueInner<'_> {
    type Output = ValueInner<'static>;

    fn into_static_value(self) -> Self::Output {
        match self {
            ValueInner::String(cow) => ValueInner::String(cow.into_owned().into()),
            ValueInner::Integer(i) => ValueInner::Integer(i),
            ValueInner::Float(f) => ValueInner::Float(f),
            ValueInner::Boolean(b) => ValueInner::Boolean(b),
            ValueInner::Array(values) => {
                let values = values
                    .into_iter()
                    .map(IntoStaticValue::into_static_value)
                    .collect();
                ValueInner::Array(values)
            }
            ValueInner::Table(btree_map) => {
                let btree_map = btree_map
                    .into_iter()
                    .map(|(k, v)| (k.into_static_value(), v.into_static_value()))
                    .collect();
                ValueInner::Table(btree_map)
            }
        }
    }
}

impl IntoStaticValue for toml_span::value::Key<'_> {
    type Output = toml_span::value::Key<'static>;

    fn into_static_value(self) -> Self::Output {
        toml_span::value::Key {
            name: self.name.into_owned().into(),
            span: self.span,
        }
    }
}

/// Create a custom [`DeserError`] with a span.
pub fn custom_deser_error(
    span: toml_span::Span,
    msg: impl Into<Cow<'static, str>>,
) -> DeserError {
    let err_kind = toml_span::ErrorKind::Custom(msg.into());
    let err = toml_span::Error::from((err_kind, span));
    DeserError::from(err)
}

/// JSON [`Value`] wrapper for deserializing raw JSON from TOML.
#[derive(Debug)]
pub struct JsonValue(Value);

impl JsonValue {
    /// Convert to a [`Value`].
    pub fn into_json(self) -> Value {
        self.0
    }
}

impl<'de> toml_span::Deserialize<'de> for JsonValue {
    fn deserialize(value: &mut toml_span::Value<'de>) -> Result<Self, DeserError> {
        let inner = value.take();
        match inner {
            ValueInner::String(cow) => Ok(JsonValue(Value::String(cow.into_owned()))),
            ValueInner::Integer(i) => {
                Ok(JsonValue(Value::Number(serde_json::Number::from(i))))
            }
            ValueInner::Float(f) => Ok(JsonValue(Value::Number(
                serde_json::Number::from_f64(f).ok_or_else(|| {
                    custom_deser_error(value.span, "Invalid float value")
                })?,
            ))),
            ValueInner::Boolean(b) => Ok(JsonValue(Value::Bool(b))),
            ValueInner::Array(values) => {
                let values = values
                    .into_iter()
                    .map(|mut v| JsonValue::deserialize(&mut v).map(|v| v.into_json()))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(JsonValue(Value::Array(values)))
            }
            ValueInner::Table(btree_map) => {
                let properties = btree_map
                    .into_iter()
                    .map(|(k, mut v)| -> Result<(String, Value), DeserError> {
                        let key = k.name.into_owned();
                        let value = JsonValue::deserialize(&mut v)?.into_json();
                        Ok((key, value))
                    })
                    .collect::<Result<serde_json::Map<_, _>, _>>()?;
                Ok(JsonValue(Value::Object(properties)))
            }
        }
    }
}
