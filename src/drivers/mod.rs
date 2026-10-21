//! Drivers for various backend implementations.

use std::{env::current_dir, fmt, ops::AddAssign, str::FromStr};

use clap::{Args, ValueEnum as _};
use keen_retry::RetryResult;
use leaky_bucket::RateLimiter;
use reqwest::StatusCode;
use schemars::JsonSchema;

use crate::{
    costs::{ModelCost, ModelCostDatabase},
    drivers::{decide::DecideDriverType, llm::LlmDriverType},
    mem_limit::MemLimit,
    prelude::*,
    rate_limit::RateLimit,
    retry::IsKnownTransient,
};

pub mod decide;
pub mod llm;
pub mod ocr;

/// Types of drivers.
///
/// This enum is of limited use, because it includes incompatible driver types.
/// Right now, we use it for [`crate::costs::ModelCostDatabase`] lookups, but
/// that file format may change to include driver types in the future.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq, Hash, Ord)]
pub enum DriverType {
    Llm(LlmDriverType),
    Decide(DecideDriverType),
}

impl FromStr for DriverType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // LLM drivers are tried first, so they keep their names in the cost
        // CSV if a decide driver name is ever added that collides.
        if let Ok(llm) = LlmDriverType::from_str(s, true) {
            Ok(Self::Llm(llm))
        } else if let Ok(decide) = DecideDriverType::from_str(s, true) {
            Ok(Self::Decide(decide))
        } else {
            Err(anyhow::anyhow!("invalid driver type: {}", s))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_type_parses_llm_and_decide_names() {
        assert_eq!(
            DriverType::from_str("native").unwrap(),
            DriverType::Llm(LlmDriverType::Native)
        );
        assert_eq!(
            DriverType::from_str("system_one").unwrap(),
            DriverType::Decide(DecideDriverType::SystemOne)
        );
        assert!(DriverType::from_str("not-a-driver").is_err());
    }

    #[test]
    fn driver_errors_we_create_are_never_transient() {
        // `invalid_input` and `invalid_output` describe deterministic failures:
        // a bad prompt, or a server that answered the wrong question. Retrying
        // resends the same request to the same broken endpoint, so these must
        // stay fatal however the message reads.
        for error in [
            DriverError::invalid_input(anyhow!("prompt has no questions")),
            DriverError::invalid_output(anyhow!("answer was not offered")),
        ] {
            assert!(
                !error.is_known_transient(),
                "{error:?} should not be retried"
            );
        }
    }

    #[test]
    fn api_status_errors_classify_from_the_status() {
        // The transience table itself lives in `retry`; what matters here is
        // that `api_status` consults it, so a driver reporting a server status
        // gets the retry policy that status implies.
        assert!(
            DriverError::api_status(StatusCode::TOO_MANY_REQUESTS, anyhow!("slow down"))
                .is_known_transient()
        );
        assert!(
            !DriverError::api_status(
                StatusCode::BAD_REQUEST,
                anyhow!("not a decision model"),
            )
            .is_known_transient()
        );
    }
}

/// Driver-related options.
#[derive(Args, Clone, Debug)]
pub struct DriverOpts {
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

impl DriverOpts {
    /// Get the cost for the chosen model.
    ///
    /// There's no deeply-principled reason that this is here, other than that
    /// it has [`Self::model_cost_data`]. It could potentially be moved to
    /// [`ModelCostDatabase`].
    pub fn cost_info(&self, driver_type: DriverType, model: &str) -> Option<ModelCost> {
        // Look up model cost data.
        let cost_db = ModelCostDatabase::new(self.model_cost_data.as_deref());
        let cost = cost_db.lookup(driver_type, model).cloned();
        if let Some(ref cost) = cost {
            debug!(driver = ?driver_type, %model, ?cost, "Model cost info");
        } else {
            debug!(driver = ?driver_type, %model, "Model cost info not available");
        }
        cost
    }

    /// Build a rate limiter.
    pub fn rate_limiter(&self) -> Option<RateLimiter> {
        self.rate_limit.as_ref().map(|rl| rl.to_rate_limiter())
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
/// definitely fatal. We use [`anyhow::Error`] here instead of a more precise
/// type because we don't care about crate-specific error details besides
/// retryability.
pub type DriverRetryResult<T> = RetryResult<(), (), T, anyhow::Error>;

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

    /// API call returned an error HTTP status along with the server's own error
    /// message. [`is_transient`] set from the status code.
    pub fn api_status(status: StatusCode, source: impl Into<anyhow::Error>) -> Self {
        let is_transient = status.is_known_transient();
        Self {
            kind: DriverErrorKind::Api,
            source: Some(source.into()),
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
