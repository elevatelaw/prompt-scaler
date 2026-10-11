//! The `decide` prompt data type.

// Work in progress: Nothing calls this yet.
#![expect(dead_code)]

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

/// A chat completion prompt.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct DecidePrompt<State: PromptState = Template> {
    /// Optional, renderable `state`, which undergoes template rendering. plain
    /// string, or JSON, which is rendered as a string. If `state` is missing,
    /// it must be provided by the input. If this is present, then `state` in
    /// the input is an error.
    pub state: Option<Value>,

    /// Optional: Images to send to the remote API. Undergoes template
    /// rendering. Not necessarily widely supported.
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
