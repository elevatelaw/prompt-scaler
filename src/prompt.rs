//! Our prompt data type.

use std::fs;

use base64::prelude::BASE64_STANDARD;
use handlebars::{
    Context, Handlebars, Helper, HelperResult, Output, RenderContext, RenderErrorReason,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{io::JsonObject, prelude::*};

/// Handlebars helper for converting a path to an image data URL.
fn image_data_url_helper(
    h: &Helper,
    _: &Handlebars,
    _: &Context,
    _: &mut RenderContext,
    out: &mut dyn Output,
) -> HelperResult {
    // Get our path parameter.
    let path = h
        .param(0)
        .ok_or_else(|| RenderErrorReason::ParamNotFoundForIndex("image-data-url", 0))?
        .value()
        .as_str()
        .ok_or_else(|| RenderErrorReason::InvalidParamType("string"))?;

    // Get the MIME type using `infer`.
    let mime = infer::get_from_path(path)
        .map_err(|err| {
            RenderErrorReason::Other(format!(
                "error inferring MIME type for {}: {}",
                path, err
            ))
        })?
        .ok_or_else(|| {
            RenderErrorReason::Other(format!("unknown MIME type for {}", path))
        })?;

    // Base64 encode the file.
    let bytes = fs::read(path).map_err(|err| {
        RenderErrorReason::Other(format!("error reading {}: {}", path, err))
    })?;
    let data_url = format!(
        "data:{};base64,{}",
        mime.mime_type(),
        base64::display::Base64Display::new(bytes.as_slice(), &BASE64_STANDARD),
    );
    out.write(&data_url)?;
    Ok(())
}

/// Render a prompt as a JSON object, filling in template values for any string
/// fields.
pub trait RenderTemplate {
    type Output;

    /// Render the template.
    fn render_template(
        &self,
        handlebars: &Handlebars,
        bindings: &JsonObject,
    ) -> Result<Self::Output>;
}

/// A chat completion prompt.
#[derive(Debug, Deserialize)]
pub struct ChatPrompt {
    /// The developer (aka "system") message, if any.
    pub developer: Option<String>,

    /// Messages.
    pub messages: Vec<Message>,
}

impl ChatPrompt {
    /// Render the prompt as a JSON object.
    pub fn render_prompt(&self, bindings: &JsonObject) -> Result<Value> {
        let mut handlebars = Handlebars::new();
        handlebars.register_helper("image-data-url", Box::new(image_data_url_helper));
        self.render_template(&handlebars, bindings)
    }
}

impl RenderTemplate for ChatPrompt {
    type Output = Value;

    fn render_template(
        &self,
        handlebars: &Handlebars,
        bindings: &JsonObject,
    ) -> Result<Self::Output> {
        let mut messages = Vec::new();
        if let Some(developer) = &self.developer {
            messages.push(json!({
                "role": "system",
                "content": handlebars.render_template(developer, bindings)?,
            }));
        }
        for message in &self.messages {
            messages.extend(message.render_template(handlebars, bindings)?);
        }
        Ok(Value::Array(messages))
    }
}

/// A message, and optionally a response (represented as a JSON object).
#[derive(Debug, Deserialize)]
pub struct Message {
    /// The user message.
    pub user: String,

    /// Images to include with the user message, provided as URLs.
    #[serde(default)]
    pub images: Vec<String>,

    /// The assistant response (optional). This is always a JSON object.
    pub assistant: Option<JsonObject>,
}

impl RenderTemplate for Message {
    type Output = Vec<Value>;

    fn render_template(
        &self,
        handlebars: &Handlebars,
        bindings: &JsonObject,
    ) -> Result<Self::Output> {
        let user = handlebars.render_template(&self.user, bindings)?;
        let images = self
            .images
            .iter()
            .map(|value| handlebars.render_template(value, bindings))
            .collect::<Result<Vec<_>, _>>()?;
        let mut messages = if images.is_empty() {
            vec![json!({
                "role": "user",
                "content": user
            })]
        } else {
            let mut parts = vec![json!({ "type": "text", "text": user })];
            for image in images {
                parts.push(json!({
                    "type": "image_url",
                    "image_url": { "url": image }
                }));
            }
            vec![json!({
                "role": "user",
                "content": parts
            })]
        };
        if let Some(assistant) = &self.assistant {
            let assistant = assistant.render_template(handlebars, bindings)?;
            messages
                .push(json!({ "role": "assistant", "content": assistant.to_string() }));
        }
        Ok(messages)
    }
}

impl RenderTemplate for Value {
    type Output = Value;

    fn render_template(
        &self,
        handlebars: &Handlebars,
        bindings: &JsonObject,
    ) -> Result<Self::Output> {
        match self {
            Value::String(s) => {
                let rendered = handlebars.render_template(s, bindings)?;
                Ok(Value::String(rendered))
            }
            Value::Object(obj) => obj.render_template(handlebars, bindings),
            Value::Array(arr) => {
                let mut output = Vec::new();
                for value in arr {
                    let rendered = value.render_template(handlebars, bindings)?;
                    output.push(rendered);
                }
                Ok(Value::Array(output))
            }
            _ => Ok(self.clone()),
        }
    }
}

impl RenderTemplate for JsonObject {
    type Output = Value;

    fn render_template(
        &self,
        handlebars: &Handlebars,
        bindings: &JsonObject,
    ) -> Result<Self::Output> {
        let mut output = Map::new();
        for (key, value) in self {
            let rendered_key = handlebars.render_template(key, bindings)?;
            let rendered_value = value.render_template(handlebars, bindings)?;
            output.insert(rendered_key, rendered_value);
        }
        Ok(Value::Object(output))
    }
}
