//! The `decide` prompt data type.

// Work in progress: Nothing calls this yet.
#![expect(dead_code)]

use std::{collections::BTreeMap, marker::PhantomData};

use anyhow::ensure;
use handlebars::Handlebars;
use schemars::JsonSchema;

use super::{PromptState, Rendered, Template};
use crate::{
    async_utils::io::JsonObject,
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

/// A question. This will not undergo template instantiation.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "type", deny_unknown_fields)]
#[non_exhaustive]
pub enum Question {
    /// A question with a Boolean answer. Outputs the model's
    /// estimated probability that the answer it true.
    Noul {
        /// The true-or-false question to ask.
        instructions: String,
        /// Descriptions of `true` and `false` responses. Highly recommended,
        /// especially for smaller models.
        criteria: Option<NoulCriteria>,
    },
    /// A multiple-choice question. Outputs the most likely choice, a
    /// confidence score, and the model's probability distribution
    /// over the choices.
    Choice {
        /// The multiple choice question to ask.
        instructions: String,
        /// The labels, and their criteria. Including criteria is highly
        /// recommended. Must have at least 2 criteria. The maximum number
        /// may be as low as 52 with some models.
        criteria: BTreeMap<String, Option<String>>,
    },
    /// A question to pick a score between 0 and (N-1), where N is the
    /// provided set of criteria. Returns the **expected value** of the
    /// label chosen. Must have 2-10 criteria.
    Score {
        /// The scoring question to ask.
        instructions: String,
        /// The criteria for each score, starting with 0 (not 1).
        criteria: Vec<String>,
    },
}

impl Question {
    /// Get the instructions for this question.
    fn instructions(&self) -> &str {
        match self {
            Question::Noul { instructions, .. } => instructions,
            Question::Choice { instructions, .. } => instructions,
            Question::Score { instructions, .. } => instructions,
        }
    }

    /// Validate this question.
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.instructions().is_empty(),
            "question instructions must not be empty"
        );
        match self {
            Question::Noul { .. } => {}
            Question::Choice { criteria, .. } => {
                ensure!(
                    criteria.len() >= 2,
                    "choice must have at least two criteria"
                );
            }
            Question::Score { criteria, .. } => {
                ensure!(
                    (2..=10).contains(&criteria.len()),
                    "score must have 2-10 criteria"
                );
            }
        }
        Ok(())
    }
}

/// The criteria for true and false responses. Recommended, especially
/// for smaller models.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "type", deny_unknown_fields)]
pub struct NoulCriteria {
    /// The criteria for a true answer.
    pub r#true: Option<String>,
    /// The criteria for a false answer.
    pub r#false: Option<String>,
}
