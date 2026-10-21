//! The `decide` prompt data type.

use std::{collections::BTreeMap, marker::PhantomData};

use handlebars::Handlebars;
use schemars::JsonSchema;

use super::{PromptState, Rendered, Template};
use crate::{
    async_utils::io::JsonObject,
    drivers::decide::Question,
    prelude::*,
    templates::{RenderTemplate, TemplateEngine},
};

/// A decision-model prompt.
///
/// Prompts are JSON  for now, so we omit an `impl` for [`toml_span::Deserialize`].
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct DecidePrompt<State: PromptState = Template> {
    /// Optional, renderable `state`, which undergoes template rendering. This may be a
    ///  simple string, or structured JSON data.
    ///
    /// Exactly one of this field and `state` on the input record must be
    /// present. If this is present, the input record must not carry its own
    /// `state`; if it is absent, state comes from the input record verbatim.
    pub state: Option<Value>,

    /// Optional: Images to send to the remote API. Each entry is a template
    /// rendered with the record's bindings, usually via the `image-data-url`
    /// helper. Not necessarily widely supported. This is normally expected to
    /// be a local `file://` URL, which specific drivers will need to load and
    /// transmit in some format.
    pub images: Option<Vec<String>>,

    /// Questions to answer. This will not undergo template rendering.
    pub questions: BTreeMap<String, Question>,

    /// Zero-size placeholder to keep Rust happy by using [`State`] _somewhere_
    /// in this type.
    #[serde(default, skip)]
    _phantom: PhantomData<State>,
}

impl DecidePrompt<Template> {
    /// Make sure our template is valid.
    fn validate(&self) -> Result<()> {
        for question in self.questions.values() {
            question.validate()?;
        }
        Ok(())
    }

    /// Render the prompt as a JSON object. This causes a state transition from
    /// [`Template`] to [`Rendered`].
    pub fn render(
        &self,
        bindings: &JsonObject,
        base_dir: &Path,
    ) -> Result<DecidePrompt<Rendered>> {
        self.validate()?;
        let engine = TemplateEngine::new(base_dir)?;
        engine
            .render(self, bindings)
            .context("Could not render prompt")
    }
}

impl RenderTemplate for DecidePrompt<Template> {
    type Output = DecidePrompt<Rendered>;

    fn render_template(
        &self,
        handlebars: &Handlebars,
        bindings: &JsonObject,
    ) -> Result<Self::Output> {
        Ok(DecidePrompt {
            state: self
                .state
                .as_ref()
                .map(|state| state.render_template(handlebars, bindings))
                .transpose()?,
            images: self
                .images
                .as_ref()
                .map(|images| {
                    images
                        .iter()
                        .map(|image| image.render_template(handlebars, bindings))
                        .collect::<Result<Vec<_>>>()
                })
                .transpose()?,
            questions: self.questions.clone(),
            _phantom: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Template bindings.
    fn bindings(pairs: &[(&str, &str)]) -> JsonObject {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), json!(value)))
            .collect()
    }

    /// A prompt whose single valid question carries `instructions`, so that
    /// [`DecidePrompt::render`]'s validation step passes.
    fn prompt(
        state: Value,
        instructions: &str,
        images: Option<Vec<String>>,
    ) -> DecidePrompt {
        DecidePrompt {
            state: Some(state),
            images,
            questions: BTreeMap::from([(
                "urgent".to_string(),
                Question::Noul {
                    instructions: instructions.to_string(),
                    criteria: None,
                },
            )]),
            _phantom: PhantomData,
        }
    }

    fn base_dir() -> PathBuf {
        std::env::current_dir().expect("should be able to find the current directory")
    }

    #[test]
    fn state_templates_render_at_any_depth() {
        let prompt = prompt(
            json!({
                "doc": "{{text}}",
                "meta": { "quoted": "[{{text}}]", "number": 42, "flag": true },
            }),
            "Does this need action within 24 hours?",
            None,
        );

        let rendered = prompt
            .render(&bindings(&[("text", "hello")]), &base_dir())
            .expect("prompt should render");

        assert_eq!(
            rendered.state,
            Some(json!({
                "doc": "hello",
                "meta": { "quoted": "[hello]", "number": 42, "flag": true },
            }))
        );
    }

    #[test]
    fn questions_are_never_templated() {
        // Questions are wire-shaped, user-authored text, deliberately kept out
        // of Handlebars: option labels and criteria go to the server as written
        // and are compared against there, so a stray (or missing) mustache in
        // them must neither error nor silently disappear.
        let prompt = prompt(
            json!("{{text}}"),
            "Does {{text}} stay? So does {{no_such_binding}}.",
            None,
        );

        let rendered = prompt
            .render(&bindings(&[("text", "hello")]), &base_dir())
            .expect("prompt should render");

        let Question::Noul {
            instructions,
            criteria: _,
        } = &rendered.questions["urgent"]
        else {
            panic!("question kind should survive rendering");
        };
        assert_eq!(
            instructions,
            "Does {{text}} stay? So does {{no_such_binding}}."
        );
    }

    #[test]
    fn images_render_with_the_record_bindings() {
        let prompt = prompt(
            json!("some state"),
            "Does this need action within 24 hours?",
            Some(vec!["{{path}}".to_string(), "literal.png".to_string()]),
        );

        let rendered = prompt
            .render(&bindings(&[("path", "images/a.png")]), &base_dir())
            .expect("prompt should render");

        assert_eq!(
            rendered.images,
            Some(vec!["images/a.png".to_string(), "literal.png".to_string()])
        );
    }
}
