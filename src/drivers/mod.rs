//! LLM drivers.
//!
//! We support multiple LLM providers via native Rust drivers.

use std::{env::current_dir, fmt, ops::AddAssign, path::PathBuf, time::Duration};

use async_trait::async_trait;
use clap::{Args, ValueEnum};
use keen_retry::RetryResult;
use schemars::JsonSchema;
use serde::Serialize;

use crate::{
    costs::ModelCost,
    mem_limit::{MemLimit, MemLimiter},
    prelude::*,
    prompt::{ChatPrompt, Rendered},
    rate_limit::RateLimit,
    retry::IsKnownTransient,
};

pub mod bedrock;
pub mod echo;
pub mod native;
pub mod vertex;

/// Our different driver types.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, ValueEnum,
)]
#[clap(rename_all = "snake_case")]
pub enum DriverType {
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

impl DriverType {
    /// Instantiate an appropriate driver.
    pub async fn create_driver(&self) -> Result<Box<dyn Driver>> {
        match self {
            DriverType::Bedrock => Ok(Box::new(bedrock::BedrockDriver::new().await?)),
            DriverType::Echo => Ok(Box::new(echo::EchoDriver::new())),
            DriverType::Native => Ok(Box::new(native::NativeDriver::new().await?)),
            DriverType::Vertex => Ok(Box::new(vertex::VertexDriver::new().await?)),
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
    #[clap(long, alias = "timeout")]
    pub llm_timeout: Option<u64>,

    /// A rate limit for LLM API requests, of the form "10/s" or "2000/m". This is
    /// applied separately from `--jobs`.
    #[clap(long)]
    pub rate_limit: Option<RateLimit>,

    /// Limit the total memory used by in-flight page images, e.g. "2GiB",
    /// "500MB". When this limit is reached, image loading will block until
    /// other images are released. Useful for high-concurrency OCR workloads
    /// where hundreds of page images might otherwise overwhelm RAM. Note that
    /// this is not exact: Some drivers may make multiple copies of a page image
    /// internally that we don't track. Consider this approximate and advisory
    /// rather than a hard limit.
    #[clap(long)]
    pub page_memory_limit: Option<MemLimit>,

    /// Path to a CSV file with model cost information. Overrides the built-in
    /// defaults. The CSV should have columns: model, input_cost_per_token,
    /// output_cost_per_token, pricing_source_url. See
    /// src/default_model_costs.csv for the expected format.
    #[clap(long)]
    pub model_cost_data: Option<PathBuf>,

    /// Base directory for resolving file paths. By default, this is the
    /// directory containing the input CSV/JSONL, or the current working
    /// directory if the input is from standard input.
    #[clap(long)]
    pub base_dir: Option<PathBuf>,

    /// Our canonical base directory.
    #[clap(skip)]
    canonical_base_dir: Option<PathBuf>,
}

impl LlmOpts {
    /// Get the timeout as a [`Duration`], if set.
    pub fn llm_timeout_duration(&self) -> Option<Duration> {
        self.llm_timeout.map(Duration::from_secs)
    }

    /// Compute and store our canonical base directory for later use.
    ///
    /// The order of precedence is:
    ///
    /// 1. `--base-dir` if set.
    /// 2. Directory containing the input CSV/JSONL if input is from a file.
    /// 3. Current working directory otherwise.
    pub fn compute_canonical_base_dir(
        &mut self,
        input_path: Option<&Path>,
    ) -> Result<()> {
        // Figure out what base directory to use.
        let actual_base_dir = match (self.base_dir.as_deref(), input_path) {
            (Some(p), _) => p,
            (None, Some(input_path)) => {
                // Give a nice error if the input path is bad, before we try to
                // canonicalize the parent directory, which gives a worse error.
                if !input_path.exists() {
                    return Err(anyhow!(
                        "Input path does not exist: {}",
                        input_path.display()
                    ));
                } else if !input_path.is_file() {
                    return Err(anyhow!(
                        "Input path is not a file: {}",
                        input_path.display()
                    ));
                }
                input_path.parent().unwrap_or(Path::new("."))
            }
            (None, None) => Path::new("."),
        };

        // Make it an absolute path.
        let absolute_base_dir = current_dir()?.join(actual_base_dir);

        // Canonicalize it.
        let canonical_base_dir = absolute_base_dir.canonicalize().with_context(|| {
            format!(
                "Failed to canonicalize base directory: {}",
                absolute_base_dir.display()
            )
        })?;

        // Store it for later.
        self.canonical_base_dir = Some(canonical_base_dir);
        Ok(())
    }

    /// Get the base directory for resolving file paths.
    pub fn canonical_base_dir(&self) -> &Path {
        self.canonical_base_dir.as_ref().expect(
            "canonical_base_dir not set; compute_canonical_base_dir must be called first",
        )
    }
}

/// A [`RetryResult`] for LLM requests. This allows [`Driver`] instances to
/// distinguish between errors that may be transient, and errors that are
/// definitely fatal.
pub type LlmRetryResult<T> = RetryResult<(), (), T, anyhow::Error>;

/// Semantic classification of driver failures.
#[derive(Debug, Clone, Copy)]
pub enum DriverErrorKind {
    /// Input preparation failed (prompt conversion, schema issues,
    /// request building). Never retryable.
    InvalidInput,
    /// The API call itself failed (network, HTTP, SDK errors).
    /// Retryability depends on the specific error.
    Api,
    /// The output was invalid — either the API response didn't match its
    /// expected structure (not retryable), or the model generated bad
    /// text like non-JSON (retryable, models are nondeterministic).
    InvalidOutput,
}

/// An error from a [`Driver`] implementation.
#[derive(Debug)]
pub struct DriverError {
    /// What kind of failure occurred.
    pub kind: DriverErrorKind,
    /// The underlying error, if any.
    pub source: Option<anyhow::Error>,
    /// Whether this error is likely transient and worth retrying.
    pub is_transient: bool,
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind_str = match self.kind {
            DriverErrorKind::InvalidInput => "invalid input",
            DriverErrorKind::Api => "API error",
            DriverErrorKind::InvalidOutput => "invalid output",
        };
        if let Some(source) = &self.source {
            write!(f, "{kind_str}: {source}")
        } else {
            write!(f, "{kind_str}")
        }
    }
}

impl std::error::Error for DriverError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|e| e.as_ref())
    }
}

impl IsKnownTransient for DriverError {
    fn is_known_transient(&self) -> bool {
        self.is_transient
    }
}

impl DriverError {
    /// Input preparation failed. Always fatal.
    pub fn invalid_input(source: impl Into<anyhow::Error>) -> Self {
        Self {
            kind: DriverErrorKind::InvalidInput,
            source: Some(source.into()),
            is_transient: false,
        }
    }

    /// API call failed. Transience classified by [`IsKnownTransient`] on the
    /// concrete error type.
    pub fn api(error: impl IsKnownTransient + Into<anyhow::Error>) -> Self {
        let is_transient = error.is_known_transient();
        Self {
            kind: DriverErrorKind::Api,
            source: Some(error.into()),
            is_transient,
        }
    }

    /// API response didn't match expected structure. Fatal — retrying sends
    /// the same request to the same broken endpoint.
    pub fn invalid_output(source: impl Into<anyhow::Error>) -> Self {
        Self {
            kind: DriverErrorKind::InvalidOutput,
            source: Some(source.into()),
            is_transient: false,
        }
    }

    /// Model generated bad output (non-JSON, wrong tool, etc.).
    /// Transient — models are nondeterministic.
    pub fn invalid_output_transient(source: impl Into<anyhow::Error>) -> Self {
        Self {
            kind: DriverErrorKind::InvalidOutput,
            source: Some(source.into()),
            is_transient: true,
        }
    }
}

/// Interface trait for LLM drivers.
#[async_trait]
pub trait Driver: fmt::Debug + Send + Sync + 'static {
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
    pub fn estimate_cost(&self, model_cost: Option<&ModelCost>) -> Option<f64> {
        if let Some(cost) = model_cost {
            let input_cost = self.prompt_tokens as f64 * cost.input_cost_per_token;
            let output_cost = self.completion_tokens as f64 * cost.output_cost_per_token;
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
