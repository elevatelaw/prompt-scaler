//! Prompts for different CLI subcommands.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, de::DeserializeOwned};

pub mod chat;
pub mod decide;

/// Super-type of allowable prompt states. This is using the popular "type
/// state" pattern, where we use Rust types to represent allowable states and
/// transitions for a type. This all happens at compile time, in the type
/// system, and doesn't actually generate any code.
pub trait PromptState: fmt::Debug + DeserializeOwned + JsonSchema + 'static {}

/// The state for a prompt that is still a template, which needs to be rendered.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct Template;

impl PromptState for Template {}

/// The state for a prompt that has been rendered.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct Rendered;

impl PromptState for Rendered {}
