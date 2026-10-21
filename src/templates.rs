//! Handlebars-based engine used for rendering various kinds of prompt templates.

use std::fs;

use handlebars::{
    Context, Handlebars, Helper, HelperDef, HelperResult, Output, RenderContext,
    RenderErrorReason,
};
use handlebars_concat::HandlebarsConcat;

use crate::{async_utils::io::JsonObject, images::ImageFile, prelude::*};

/// Handlebars-based engine used for rendering various kinds of prompt templates.
pub struct TemplateEngine {
    handlebars: Handlebars<'static>,
}

impl TemplateEngine {
    /// Create a new template engine, with all our helpers hooked up.
    /// `base_dir` should point to the directory containing our template
    /// text (if present) or to some other fallback location as specified
    /// by our UI conventions.
    pub fn new(base_dir: &Path) -> Result<Self> {
        if !base_dir.is_absolute() {
            return Err(anyhow!(
                "Base directory must be an absolute path, got: {}",
                base_dir.display()
            ));
        }

        // Set up our Handlebars helpers.
        let mut handlebars = Handlebars::new();
        handlebars.register_escape_fn(|s| s.to_owned());
        handlebars.register_helper("concat", Box::new(HandlebarsConcat));
        handlebars.register_helper(
            "image-data-url",
            Box::new(ImageDataUrlHelper::new(base_dir)),
        );
        handlebars.register_helper(
            "text-file-contents",
            Box::new(TextFileContentsHelper::new(base_dir)),
        );
        handlebars.register_helper("to-string", Box::new(to_string_helper));
        Ok(Self { handlebars })
    }

    /// Render a template with the specified bindings.
    pub fn render<RT: RenderTemplate>(
        &self,
        template: &RT,
        bindings: &JsonObject,
    ) -> Result<RT::Output> {
        template.render_template(&self.handlebars, bindings)
    }
}

/// Handlebars helper for converting a path to a `file:` URL for late image
/// loading. The image is not loaded during template rendering — it will be
/// loaded later by the driver with the appropriate encoding.
struct ImageDataUrlHelper {
    base_dir: PathBuf,
}

impl ImageDataUrlHelper {
    fn new(base_dir: &Path) -> Self {
        let base_dir = base_dir.to_path_buf();
        Self { base_dir }
    }
}

impl HelperDef for ImageDataUrlHelper {
    fn call<'reg: 'rc, 'rc>(
        &self,
        h: &Helper<'rc>,
        _r: &'reg Handlebars<'reg>,
        _ctx: &'rc Context,
        _rc: &mut RenderContext<'reg, 'rc>,
        out: &mut dyn Output,
    ) -> HelperResult {
        // Get our path parameter.
        let path = h
            .param(0)
            .ok_or_else(|| RenderErrorReason::ParamNotFoundForIndex("image-data-url", 0))?
            .value()
            .as_str()
            .ok_or_else(|| RenderErrorReason::InvalidParamType("string"))?;

        // Construct the full path, by joining with our root directory.
        //
        // SECURITY: If you are processing untrusted data, you will want to
        // modify the code to to restrict file reads outside of a specific
        // directory.
        let full_path = self.base_dir.join(path);

        // Construct an ImageFile and emit a file: URL.
        let image_file = ImageFile::from_path(&full_path)
            .map_err(|err| RenderErrorReason::Other(err.to_string()))?;
        out.write(&image_file.to_url())?;
        Ok(())
    }
}

/// Handlebars helper for reading the contents of a text file and returning it
/// as a string.
struct TextFileContentsHelper {
    base_dir: PathBuf,
}

impl TextFileContentsHelper {
    fn new(base_dir: &Path) -> Self {
        let base_dir = base_dir.to_path_buf();
        Self { base_dir }
    }
}

impl HelperDef for TextFileContentsHelper {
    fn call<'reg: 'rc, 'rc>(
        &self,
        h: &Helper<'rc>,
        _r: &'reg Handlebars<'reg>,
        _ctx: &'rc Context,
        _rc: &mut RenderContext<'reg, 'rc>,
        out: &mut dyn Output,
    ) -> HelperResult {
        // Get our path parameter.
        let path = h
            .param(0)
            .ok_or_else(|| {
                RenderErrorReason::ParamNotFoundForIndex("text-file-contents", 0)
            })?
            .value()
            .as_str()
            .ok_or_else(|| RenderErrorReason::InvalidParamType("string"))?;

        // Construct the full path, by joining with our root directory.
        //
        // SECURITY: If you are processing untrusted data, you will want to
        // modify the code to to restrict file reads outside of a specific
        // directory.
        let full_path = self.base_dir.join(path);

        // Read the file.
        let contents = fs::read_to_string(&full_path).map_err(|err| {
            RenderErrorReason::Other(format!("error reading {full_path:?}: {err}"))
        })?;
        out.write(&contents)?;
        Ok(())
    }
}

/// Convert a value into a string, using Rust's [`std::fmt::Display`] trait.
fn to_string_helper(
    h: &Helper,
    _: &Handlebars,
    _: &Context,
    _: &mut RenderContext,
    out: &mut dyn Output,
) -> HelperResult {
    // Get our value parameter.
    let value = h
        .param(0)
        .ok_or_else(|| RenderErrorReason::ParamNotFoundForIndex("to-string", 0))?
        .value();

    // Convert the value to a string.
    Ok(out.write(&value.to_string())?)
}

/// Render a [`Template`] version of a type, and return the [`Rendered`] version
pub trait RenderTemplate {
    type Output;

    /// Render the template.
    fn render_template(
        &self,
        handlebars: &Handlebars,
        bindings: &JsonObject,
    ) -> Result<Self::Output>;
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
                let rendered = s.render_template(handlebars, bindings)?;
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
        let mut output = serde_json::Map::new();
        for (key, value) in self {
            let rendered_key = handlebars.render_template(key, bindings)?;
            let rendered_value = value.render_template(handlebars, bindings)?;
            output.insert(rendered_key, rendered_value);
        }
        Ok(Value::Object(output))
    }
}

impl RenderTemplate for &str {
    type Output = String;

    /// Render a template, returning a helpful error message if it fails.
    fn render_template(
        &self,
        handlebars: &Handlebars,
        bindings: &JsonObject,
    ) -> Result<String> {
        handlebars.render_template(self, bindings).with_context(|| {
            let binding_keys = bindings
                .keys()
                .map(|k| &k[..])
                .collect::<Vec<_>>()
                .join(", ");
            format!("Error rendering template {self:?} with bindings: [{binding_keys}]",)
        })
    }
}

impl RenderTemplate for &String {
    type Output = String;

    /// Render a template, returning a helpful error message if it fails.
    fn render_template(
        &self,
        handlebars: &Handlebars,
        bindings: &JsonObject,
    ) -> Result<String> {
        (&self[..]).render_template(handlebars, bindings)
    }
}

impl<T> RenderTemplate for Option<T>
where
    T: RenderTemplate,
{
    type Output = Option<T::Output>;

    fn render_template(
        &self,
        handlebars: &Handlebars,
        bindings: &JsonObject,
    ) -> Result<Self::Output> {
        self.as_ref()
            .map(|val| val.render_template(handlebars, bindings))
            .transpose()
    }
}
