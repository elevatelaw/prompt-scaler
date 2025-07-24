//! Our prompt data type.

use std::{fmt, fs, marker::PhantomData};

use handlebars::{
    Context, Handlebars, Helper, HelperResult, Output, RenderContext, RenderErrorReason,
};
use handlebars_concat::HandlebarsConcat;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Map;
use toml_span::{
    DeserError,
    de_helpers::{TableHelper, expected},
    value::ValueInner,
};

use crate::{
    async_utils::io::JsonObject, data_url::data_url, prelude::*, schema::Schema,
    toml_utils::JsonValue,
};

/// Super-type of allowable prompt states. This is using the popular "type
/// state" pattern, where we use Rust types to represent allowable states and
/// transitions for a type. This all happens at compile time, in the type
/// system, and doesn't actually generate any code.
pub trait PromptState: fmt::Debug + DeserializeOwned + JsonSchema + 'static {}

/// The state for a prompt that is still a template, which needs to be rendered.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Template;

impl PromptState for Template {}

/// The state for a prompt that has been rendered.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Rendered;

impl PromptState for Rendered {}

/// A chat completion prompt.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ChatPrompt<State: PromptState = Template> {
    /// The developer (aka "system") message, if any.
    pub developer: Option<String>,

    /// Our schema.
    pub response_schema: Schema,

    /// Messages.
    pub messages: Vec<Message>,

    /// Zero-size placeholder to keep Rust happy by using [`State`] _somewhere_
    /// in this type.
    #[serde(default, skip)]
    _phantom: PhantomData<State>,
}

impl ChatPrompt<Template> {
    /// Make sure our messages appear in the order ((user, assistant)*, user).
    fn validate(&self) -> Result<()> {
        if self.messages.is_empty() {
            return Err(anyhow!("No messages in prompt"));
        }
        let mut expect_user_message = true;
        for message in &self.messages {
            let ok = match message {
                Message::User { text: None, images } if images.is_empty() => {
                    return Err(anyhow!("User message must have either text or images"));
                }
                Message::User { .. } if expect_user_message => true,
                Message::Assistant { .. } if !expect_user_message => true,
                _ => false,
            };
            if !ok {
                return Err(anyhow!(
                    "Expected alternating user and assistant messages in prompt, found {:?}",
                    message
                ));
            }
            expect_user_message = !expect_user_message;
        }
        if self.messages.len() % 2 == 0 {
            return Err(anyhow!("Prompt must end with a user message"));
        }
        Ok(())
    }

    /// Render the prompt as a JSON object. This causes a state transition from
    /// [`Template`] to [`Rendered`].
    pub fn render(&self, bindings: &JsonObject) -> Result<ChatPrompt<Rendered>> {
        self.validate()?;
        let mut handlebars = Handlebars::new();
        handlebars.register_escape_fn(|s| s.to_owned());
        handlebars.register_helper("concat", Box::new(HandlebarsConcat));
        handlebars.register_helper("image-data-url", Box::new(image_data_url_helper));
        handlebars
            .register_helper("text-file-contents", Box::new(text_file_contents_helper));
        self.render_template(&handlebars, bindings)
            .context("Could not render prompt")
    }
}

impl<'de> toml_span::Deserialize<'de> for ChatPrompt<Template> {
    fn deserialize(value: &mut toml_span::Value<'de>) -> Result<Self, DeserError> {
        let mut th = TableHelper::new(value)?;
        let developer = th.optional("developer");
        let response_schema = th.required("response_schema")?;
        let messages = th.required("messages")?;
        th.finalize(None)?;
        Ok(ChatPrompt {
            developer,
            response_schema,
            messages,
            _phantom: PhantomData,
        })
    }
}

/// A message, and optionally a response (represented as a JSON object).
///
/// We would also have a `State: PromptState` field here, but that interacts badly
/// with the [`Deserialize`] trait from [`serde`]. So just pretend that this
/// type exists in two versions: one [`Template`] and one [`Rendered`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    /// A user message.
    User {
        /// Text provided by the user.
        #[serde(default)]
        text: Option<String>,

        /// Images to include with the user message, provided as URLs.
        #[serde(default)]
        images: Vec<String>,
    },

    /// An assistant message.
    Assistant {
        /// The assistant response. This is always a JSON [`Value`].
        json: Value,
    },
}

impl<'de> toml_span::Deserialize<'de> for Message {
    fn deserialize(value: &mut toml_span::Value<'de>) -> Result<Self, DeserError> {
        let value_inner = value.take();
        let ValueInner::Table(mut table) = value_inner else {
            return Err(expected("a table", value_inner, value.span).into());
        };
        if table.len() != 1 {
            return Err(expected(
                "a table with exactly one key ('user' or 'assistant')",
                ValueInner::Table(table),
                value.span,
            )
            .into());
        }

        if table.contains_key("user") {
            let user = table.get_mut("user").expect("no user key");

            let mut th = TableHelper::new(user)?;
            let text = th.optional("text");
            let images = th.optional::<Vec<_>>("images").unwrap_or_default();
            th.finalize(None)?;
            if images.is_empty() && text.is_none() {
                return Err(expected(
                    "a user message with either 'text' or 'images'",
                    ValueInner::Table(table),
                    value.span,
                )
                .into());
            }
            Ok(Message::User { text, images })
        } else if table.contains_key("assistant") {
            let assistant = table.get_mut("assistant").expect("no assistant key");

            let mut th = TableHelper::new(assistant)?;
            let json = th.required::<JsonValue>("json")?.into_json();
            th.finalize(None)?;
            Ok(Message::Assistant { json })
        } else {
            Err(expected(
                "a table with either a 'user' or an 'assistant' key",
                ValueInner::Table(table),
                value.span,
            )
            .into())
        }
    }
}

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
                "error inferring MIME type for {path}: {err}"
            ))
        })?
        .ok_or_else(|| {
            RenderErrorReason::Other(format!("unknown MIME type for {path}"))
        })?;

    // Base64 encode the file.
    let bytes = fs::read(path).map_err(|err| {
        RenderErrorReason::Other(format!("error reading {path}: {err}"))
    })?;
    let data_url = data_url(mime.mime_type(), &bytes);
    out.write(&data_url)?;
    Ok(())
}

/// Handlebars helper for reading the contents of a text file and returning it
/// as a string.
fn text_file_contents_helper(
    h: &Helper,
    _: &Handlebars,
    _: &Context,
    _: &mut RenderContext,
    out: &mut dyn Output,
) -> HelperResult {
    // Get our path parameter.
    let path = h
        .param(0)
        .ok_or_else(|| RenderErrorReason::ParamNotFoundForIndex("text-file-contents", 0))?
        .value()
        .as_str()
        .ok_or_else(|| RenderErrorReason::InvalidParamType("string"))?;

    // Read the file.
    let contents = fs::read_to_string(path).map_err(|err| {
        RenderErrorReason::Other(format!("error reading {path}: {err}"))
    })?;
    out.write(&contents)?;
    Ok(())
}

/// Render a [`Template`] version of a type, and return the [`Rendered`] version
trait RenderTemplate {
    type Output;

    /// Render the template.
    fn render_template(
        &self,
        handlebars: &Handlebars,
        bindings: &JsonObject,
    ) -> Result<Self::Output>;
}

impl RenderTemplate for ChatPrompt<Template> {
    type Output = ChatPrompt<Rendered>;

    fn render_template(
        &self,
        handlebars: &Handlebars,
        bindings: &JsonObject,
    ) -> Result<Self::Output> {
        Ok(ChatPrompt {
            developer: self
                .developer
                .as_deref()
                .map(|developer| render_template(handlebars, developer, bindings))
                .transpose()?,
            response_schema: self.response_schema.clone(),
            messages: self
                .messages
                .iter()
                .map(|message| message.render_template(handlebars, bindings))
                .collect::<Result<Vec<_>>>()?,
            _phantom: PhantomData,
        })
    }
}

impl RenderTemplate for Message {
    type Output = Message;

    fn render_template(
        &self,
        handlebars: &Handlebars,
        bindings: &JsonObject,
    ) -> Result<Self::Output> {
        match self {
            Message::User { text, images } => Ok(Message::User {
                text: text
                    .as_ref()
                    .map(|text| render_template(handlebars, text, bindings))
                    .transpose()?,
                images: images
                    .iter()
                    .map(|image| render_template(handlebars, image, bindings))
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            Message::Assistant { json } => {
                let json = json.render_template(handlebars, bindings)?;
                Ok(Message::Assistant { json })
            }
        }
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
                let rendered = render_template(handlebars, s, bindings)?;
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

/// Render a template, returning a helpful error message if it fails.
fn render_template(
    handlebars: &Handlebars,
    template: &str,
    bindings: &JsonObject,
) -> Result<String> {
    handlebars
        .render_template(template, bindings)
        .with_context(|| {
            let binding_keys = bindings
                .keys()
                .map(|k| &k[..])
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Error rendering template {template:?} with bindings: [{binding_keys}]",
            )
        })
}
