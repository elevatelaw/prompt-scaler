//! Support for decision models.

// We're still roughing in the decision model code.
#![allow(dead_code)]

use std::collections::BTreeMap;

use schemars::JsonSchema;

use crate::{async_utils::io::JsonObject, drivers::decide::Answer, prelude::*};

/// A decision input record.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct DecideInput {
    /// A `state` field containing input to pass directly to the model. If
    /// present, the prompt must not contain a [`DecisionPrompt::state`] field,
    /// and all custom fields must be absent.
    #[serde(default)]
    state: Option<Value>,

    /// Other fields. We keep these "flattened" in the record because they're
    /// under the control of the caller, and because our input format may be a
    /// CSV file, which is inherently "flat".
    ///
    /// If any of these are present, `state` must be absent.
    #[serde(flatten)]
    template_bindings: JsonObject,
}

/// A decision output record.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct DecideOutput {
    /// Answers to questions posed in the prompt. Will not be present if the
    /// request returned an error.
    #[serde(skip_serializing_if = "Option::is_none")]
    answers: Option<BTreeMap<String, Answer>>,
}
