//! LLM drivers.
//!
//! Mostly we prefer to leave LLM compatibility to LiteLLM and similar
//! gateways, but when we're aiming for extremely high throughput,
//! sometimes it's better to keep everything in native Rust.

use std::{error, fmt, ops::AddAssign, pin::Pin, time::Duration};

use async_trait::async_trait;
use clap::{Args, ValueEnum};
use futures::{FutureExt as _, TryFutureExt as _};
use keen_retry::RetryResult;
use schemars::JsonSchema;
use serde::Serialize;
use tokio::time;

use crate::{
    litellm::LiteLlmModel,
    prelude::*,
    prompt::{ChatPrompt, Rendered},
    rate_limit::RateLimit,
    retry::IsKnownTransient,
};

pub mod bedrock;
pub mod native;
pub mod openai;
pub mod vertex;

/// Our different driver types.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum DriverType {
    /// OpenAI driver (also for LiteLLM, Ollama, etc).
    #[default]
    #[clap(name = "openai")]
    OpenAI,

    /// AWS Bedrock driver.
    Bedrock,

    /// Attempt to use a native driver for each specific AI.
    Native,

    /// Vertex driver.
    Vertex,
}

impl DriverType {
    /// Instantiate an appropriate driver.
    pub async fn create_driver(&self) -> Result<Box<dyn Driver>> {
        match self {
            DriverType::OpenAI => Ok(Box::new(openai::OpenAiDriver::new().await?)),
            DriverType::Bedrock => Ok(Box::new(bedrock::BedrockDriver::new().await?)),
            DriverType::Native => Ok(Box::new(native::NativeDriver::new().await?)),
            DriverType::Vertex => Ok(Box::new(vertex::VertexDriver::new().await?)),
        }
    }
}

/// Our chat-related options.
#[derive(Args, Clone, Debug)]
pub struct LlmOpts {
    /// EXPERIMENTAL: The LLM driver to use. This defaults to `openai`, which
    /// works with OpenAI, LiteLLM and Ollama-based models.
    #[clap(long, value_enum, default_value_t = DriverType::default())]
    pub driver: DriverType,

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

    /// A timeout, in seconds, for the LLM to return a complete response.
    /// Note that even if a request times out, you'll probably still be charged.
    /// Useful dealing with runaway responses and overloaded servers.
    #[clap(long)]
    pub timeout: Option<u64>,

    /// A rate limit for LLM API requests, of the form "10/s" or "2000/m". This is
    /// applied separately from `--jobs`.
    #[clap(long)]
    pub rate_limit: Option<RateLimit>,
}

impl LlmOpts {
    /// Apply a timeout to a future.
    ///
    /// Yes, this type signature is a bit complicated. It's possible that
    /// `future` holds references to data that it doesn't own. So we declare
    /// `'fut` to represent the lifetime of any data held by `future`, and
    /// carefully preserve it.
    ///
    /// The `Pin<Box<dyn Future<...>>>` is just our friend [`BoxedFuture`],
    /// written out the long way so we can include `'fut`. We're making a pretty
    /// elaborate promise to the Rust compiler here about data ownership and
    /// lifetimes.
    ///
    /// We box our output future because it may have different implementations,
    /// depending on which branch we took, and so Rust needs to allocate the future
    /// on the heap and only provide an abstract [`Future`] interface.
    ///
    /// Honestly we try to minimize this stuff, but timeouts are _hard_.
    pub fn apply_timeout<'fut, T, E>(
        &self,
        future: impl Future<Output = Result<T, E>> + Send + 'fut,
    ) -> Pin<Box<dyn Future<Output = Result<T, LlmError<E>>> + Send + 'fut>>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let future = future.map_err(LlmError::Native);
        if let Some(timeout) = self.timeout {
            time::timeout(Duration::from_secs(timeout), future)
                // We have a `Result<Result<T, LlmError<E>>, Elapsed>` here, and
                // we want to convert it to a `Result<T, LlmError<E>>`.
                .map(|result| match result {
                    Ok(inner) => inner,
                    Err(_) => Err(LlmError::Timeout),
                })
                .boxed()
        } else {
            future.boxed()
        }
    }
}

/// A [`RetryResult`] for LLM requests. This allows [`Driver`] instances to
/// distinguish between errors that may be transient, and errors that are
/// definitely fatal.
pub type LlmRetryResult<T> = RetryResult<(), (), T, anyhow::Error>;

/// Interface trait for LLM drivers.
#[async_trait]
pub trait Driver: fmt::Debug + Send + Sync + 'static {
    /// Run a "chat completion" request.
    ///
    /// This takes a [`LiteLlmModel`] even for non-OpenAI drivers, because it's
    /// potentially useful to use LiteLLM for model billing info while talking
    /// directly to the model itself.
    async fn chat_completion(
        &self,
        model: &str,
        model_info: Option<&LiteLlmModel>,
        prompt: &ChatPrompt<Rendered>,
        schema: Value,
        llm_opts: &LlmOpts,
    ) -> LlmRetryResult<ChatCompletionResponse>;
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

/// Token usage.
#[derive(Clone, Debug, Default, JsonSchema, Serialize)]
pub struct TokenUsage {
    /// How many tokens were used in the prompt?
    pub prompt_tokens: u64,

    /// How many tokens were used in the response?
    pub completion_tokens: u64,
}

impl TokenUsage {
    /// Was our token usage zero?
    pub fn is_zero(&self) -> bool {
        self.prompt_tokens == 0 && self.completion_tokens == 0
    }

    /// Estimate the cost of this token usage.
    pub fn estimate_cost(&self, model: Option<&LiteLlmModel>) -> Option<f64> {
        if let Some(model) = model {
            let input_cost =
                self.prompt_tokens as f64 * model.model_info.input_cost_per_token;
            let output_cost =
                self.completion_tokens as f64 * model.model_info.output_cost_per_token;
            Some(input_cost + output_cost)
        } else {
            None
        }
    }
}

impl AddAssign for TokenUsage {
    fn add_assign(&mut self, other: Self) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
    }
}

/// An error which occurred while calling an LLM.
///
/// Used internally by drivers to handle timeouts.
#[derive(Debug)]
pub enum LlmError<E> {
    /// A native error.
    Native(E),

    /// A timeout error.
    Timeout,
}

impl<E> IsKnownTransient for LlmError<E>
where
    E: IsKnownTransient,
{
    /// Is this a known transient error?
    fn is_known_transient(&self) -> bool {
        match self {
            LlmError::Native(err) => err.is_known_transient(),
            // Runaway LLM responses and some kinds of network timeouts can be retried
            // with hope of a better result.
            LlmError::Timeout => true,
        }
    }
}

impl<E> fmt::Display for LlmError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LlmError::Native(err) => write!(f, "LLM error: {err}"),
            LlmError::Timeout => write!(f, "LLM request timed out"),
        }
    }
}

impl<E> error::Error for LlmError<E>
where
    E: error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            LlmError::Native(err) => Some(err),
            LlmError::Timeout => None,
        }
    }
}
