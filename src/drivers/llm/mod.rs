//! LLM drivers.
//!
//! We support multiple LLM providers via native Rust drivers.

use std::{fmt, time::Duration};

use async_trait::async_trait;
use clap::{Args, ValueEnum};
use genai::chat::ReasoningEffort;

use super::DriverOpts;
use crate::{
    drivers::{DriverError, TokenUsage},
    mem_limit::MemLimiter,
    prelude::*,
    prompts::{Rendered, chat::ChatPrompt},
};

pub mod bedrock;
pub mod echo;
pub mod native;
pub mod vertex;

/// Shared numeric translation of [`ReasoningEffort`] for drivers that express
/// thinking as a token budget rather than a provider keyword. The keyword
/// variants use the same token counts the `genai` Gemini adapter does, so
/// drivers that apply this trait agree with `genai` on what "low" etc. mean.
pub trait ReasoningEffortExt {
    /// Token budget, or `None` for [`ReasoningEffort::None`] meaning "do not
    /// override the provider default". `Budget(0)` disables thinking on
    /// providers that honor a zero budget (e.g. Gemini 2.5).
    fn thinking_budget(&self) -> Option<u32>;
}

impl ReasoningEffortExt for ReasoningEffort {
    fn thinking_budget(&self) -> Option<u32> {
        match self {
            ReasoningEffort::None => None,
            ReasoningEffort::Low | ReasoningEffort::Minimal => Some(1000),
            ReasoningEffort::Medium => Some(8000),
            ReasoningEffort::High => Some(24000),
            ReasoningEffort::Budget(n) => Some(*n),
        }
    }
}

/// Our different driver types.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, ValueEnum,
)]
#[clap(rename_all = "snake_case")]
pub enum LlmDriverType {
    /// Native per-provider driver. Routes each model to its native adapter
    /// (OpenAI, Anthropic, Gemini, Ollama, ...). Set `OPENAI_API_BASE` to
    /// force everything through an OpenAI-compatible gateway (llama-server,
    /// Ollama, LiteLLM). `openai` is accepted as an alias for backward
    /// compatibility.
    #[default]
    #[clap(alias = "openai")]
    Native,

    /// AWS Bedrock driver.
    Bedrock,

    /// Echo driver (for testing).
    Echo,

    /// Vertex driver.
    Vertex,
}

impl LlmDriverType {
    /// Instantiate an appropriate driver.
    pub async fn create_driver(&self) -> Result<Box<dyn LlmDriver>> {
        match self {
            LlmDriverType::Bedrock => Ok(Box::new(bedrock::BedrockDriver::new().await?)),
            LlmDriverType::Echo => Ok(Box::new(echo::EchoDriver::new())),
            LlmDriverType::Native => Ok(Box::new(native::NativeDriver::new().await?)),
            LlmDriverType::Vertex => Ok(Box::new(vertex::VertexDriver::new().await?)),
        }
    }
}

/// Our chat-related options.
#[derive(Args, Clone, Debug)]
pub struct LlmOpts {
    /// The LLM driver to use. Defaults to `native`, which routes each model
    /// to its provider's native adapter. Set `OPENAI_API_BASE` to route all
    /// requests through an OpenAI-compatible gateway (llama-server, Ollama,
    /// LiteLLM). `openai` is accepted as an alias for `native`.
    #[clap(long, value_enum, default_value_t = LlmDriverType::default())]
    pub driver: LlmDriverType,

    /// An upper limit on the number of completion tokens to generate. This may
    /// help prevent runaway responses, but it may also cause incomplete
    /// results. For English, many models have around 4 bytes per token.
    #[clap(long)]
    pub max_completion_tokens: Option<u32>,

    /// The temperature to use for sampling, between 0.0 and 2.0. Higher values
    /// may the output more random, while lower values may make it more
    /// deterministic. Defaults to the model's default.
    #[clap(long)]
    pub temperature: Option<f32>,

    /// The top-p sampling value to use, between 0.0 and 1.0. This is an
    /// alternative to temperature sampling. See your model's API docs for an
    /// explanation. Defaults to the model's default.
    #[clap(long)]
    pub top_p: Option<f32>,

    /// Advisory hint for how hard a reasoning model should think. Accepts
    /// `none|low|medium|high|minimal`, or an integer thinking-token budget
    /// (`0` disables thinking on providers that support it, e.g. Gemini 2.5).
    /// The `bedrock` driver supports it for OpenAI models only (effort levels,
    /// not budgets) and warns that it is ignored for other model families.
    #[clap(long, value_parser = str::parse::<ReasoningEffort>)]
    pub reasoning_effort: Option<ReasoningEffort>,

    /// A timeout, in seconds, for the LLM to return a complete response.
    /// Note that even if a request times out, you'll probably still be charged.
    /// Useful dealing with runaway responses and overloaded servers.
    #[clap(long, alias = "timeout")]
    pub llm_timeout: Option<u64>,

    /// Shared driver options.
    #[clap(flatten)]
    pub driver_opts: DriverOpts,
}

impl LlmOpts {
    /// Get the timeout as a [`Duration`], if set.
    pub fn llm_timeout_duration(&self) -> Option<Duration> {
        self.llm_timeout.map(Duration::from_secs)
    }
}

/// Interface trait for LLM drivers.
#[async_trait]
pub trait LlmDriver: fmt::Debug + Send + Sync + 'static {
    /// Run a "chat completion" request.
    async fn chat_completion(
        &self,
        model: &str,
        prompt: &ChatPrompt<Rendered>,
        schema: Value,
        llm_opts: &LlmOpts,
        mem_limiter: &MemLimiter,
    ) -> Result<ChatCompletionResponse, DriverError>;
}

/// A chat completion response.
#[derive(Debug)]
pub struct ChatCompletionResponse {
    /// Structured response from the LLM. This will not have been
    /// validated yet.
    pub response: Value,

    /// Token usage.
    pub token_usage: Option<TokenUsage>,
}
