//! Shared driver for "system one"-compatible APIs, like llama.cpp and Jev.
//!
//! Talks to `POST {base}/v1/systemone`. Configure with `SYSTEM_ONE_API_URL`
//! (default `http://127.0.0.1:8080`) and the optional `SYSTEM_ONE_API_KEY`
//! (sent as a Bearer token).

use std::{collections::BTreeMap, env, fmt};

use super::{Answer, DecideDriver, DecideRequest, DecideResponse};
use crate::{
    drivers::{DriverError, TokenUsage},
    prelude::*,
};

/// Default server location.
const DEFAULT_BASE_URL: &str = "http://localhost:8080";

/// Wire-format usage information.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Usage {
    /// The number of input tokens used.
    input_tokens: u64,

    /// The number of output tokens used. Depending on the API used, this will
    /// typically be either 1 token per question asked, or 0. We normalize this
    /// to 1 per answer when converting to
    /// [`TokenUsage`](crate::drivers::TokenUsage).
    #[serde(default)]
    output_tokens: u64,
}

/// Wire-format response from a System One endpoint.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SystemOneResponse {
    /// The model that answered. Servers return a resolved or aliased name.
    model: Option<String>,

    /// Answers, keyed by question name.
    answers: BTreeMap<String, Answer>,

    /// Usage information. Optional because servers are inconsistent.
    usage: Option<Usage>,
}

/// Driver for llama.cpp, Jev, and other System One-compatible servers.
pub struct SystemOneDriver {
    /// Our HTTP client. No timeout is set: callers enforce timeouts.
    client: reqwest::Client,

    /// The full `.../v1/systemone` endpoint URL.
    endpoint: String,

    /// Optional Bearer token.
    api_key: Option<String>,
}

impl SystemOneDriver {
    /// Create a driver from `SYSTEM_ONE_API_URL` and `SYSTEM_ONE_API_KEY`.
    pub fn new() -> Self {
        let base_url = env::var("SYSTEM_ONE_API_URL")
            .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Self {
            client: reqwest::Client::new(),
            endpoint: format!("{}/v1/systemone", base_url.trim_end_matches('/')),
            api_key: env::var("SYSTEM_ONE_API_KEY")
                .ok()
                .filter(|key| !key.is_empty()),
        }
    }
}

impl fmt::Debug for SystemOneDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Keep the API key out of logs and panic messages.
        f.debug_struct("SystemOneDriver")
            .field("endpoint", &self.endpoint)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

#[async_trait]
impl DecideDriver for SystemOneDriver {
    #[instrument(level = "debug", skip_all)]
    async fn decide(
        &self,
        request: &DecideRequest,
    ) -> Result<DecideResponse, DriverError> {
        debug!(model = %request.model, endpoint = %self.endpoint, "System one request");

        let mut req = self.client.post(&self.endpoint).json(request);
        if let Some(api_key) = &self.api_key {
            req = req.bearer_auth(api_key);
        }

        let response = req.send().await.map_err(DriverError::api)?;
        let status = response.status();
        if !status.is_success() {
            // Servers return helpful messages (llama.cpp explains 400s and
            // 501 "not a decision model"); pass them through.
            let body = response.text().await.map_err(DriverError::api)?;
            return Err(DriverError::api_status(status, anyhow!("{body}")));
        }

        // Read the body as text and parse ourselves, so that a transport
        // failure classifies as transient API trouble, while a body that
        // doesn't match our types is fatal: the server is deterministic, and
        // retrying sends the same request to the same broken endpoint.
        let body = response.text().await.map_err(DriverError::api)?;
        let wire: SystemOneResponse = serde_json::from_str(&body)
            .with_context(|| format!("Invalid system one response body: {body}"))
            .map_err(DriverError::invalid_output)?;

        let token_usage = wire.usage.map(|usage| {
            // These models generate exactly one token per answer, even when
            // the server reports otherwise (llama.cpp always says 0).
            let completion_tokens = u64::try_from(wire.answers.len())
                .expect("more answers generated than u64 can hold");
            if usage.output_tokens != completion_tokens {
                debug!(
                    server_output_tokens = usage.output_tokens,
                    answers_generated = completion_tokens,
                    "Server output_tokens differs from answers generated; using answer count"
                );
            }
            TokenUsage {
                prompt_tokens: usage.input_tokens,
                completion_tokens,
            }
        });

        Ok(DecideResponse {
            model: wire.model.unwrap_or_else(|| request.model.clone()),
            answers: wire.answers,
            token_usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::decide::{DecideDriver, NoulCriteria, Question};

    const KEV_MODEL: &str = "ggml-org/Kev-4B-GGUF:Q4_K_M";

    /// A request exercising all three question kinds.
    fn test_request() -> DecideRequest {
        let mut questions = BTreeMap::new();
        questions.insert(
            "category".to_string(),
            Question::Choice {
                instructions: "What kind of document is this?".to_string(),
                criteria: [
                    (
                        "invoice".to_string(),
                        Some("A bill for goods or services.".to_string()),
                    ),
                    (
                        "memo".to_string(),
                        Some("An internal memorandum.".to_string()),
                    ),
                ]
                .into_iter()
                .collect(),
            },
        );
        questions.insert(
            "urgent".to_string(),
            Question::Noul {
                instructions: "Does this need action within 24 hours?".to_string(),
                criteria: Some(NoulCriteria {
                    r#true: Some("Payment or reply is due within 24 hours.".to_string()),
                    r#false: Some("Nothing is due within 24 hours.".to_string()),
                }),
            },
        );
        questions.insert(
            "quality".to_string(),
            Question::Score {
                instructions: "How clearly is this document written?".to_string(),
                criteria: vec![
                    "Confusing or incomplete.".to_string(),
                    "Understandable but sloppy.".to_string(),
                    "Clear and precise.".to_string(),
                ],
            },
        );
        DecideRequest {
            model: KEV_MODEL.to_string(),
            state: json!(
                "INVOICE #2026-041\nAcme Widgets Ltd.\n\
                 500x widget brackets, delivered 2026-10-01.\n\
                 Total due: $4,250.00. Please pay within 7 days."
            ),
            images: None,
            questions,
        }
    }

    #[test]
    fn test_request_wire_format() {
        let json = serde_json::to_value(test_request()).unwrap();
        assert_eq!(json["model"], KEV_MODEL);
        assert_eq!(json["questions"]["category"]["type"], "choice");
        assert_eq!(json["questions"]["urgent"]["type"], "noul");
        assert_eq!(json["questions"]["quality"]["type"], "score");
        assert!(
            json.get("images").is_none(),
            "images must be omitted when None"
        );
    }

    /// Live test against a real llama.cpp server. Not run by default.
    #[tokio::test]
    #[ignore = "Needs llama-server serving ggml-org/Kev-4B-GGUF:Q4_K_M; override endpoint/creds via SYSTEM_ONE_API_URL / SYSTEM_ONE_API_KEY"]
    async fn test_system_one_live_kev() {
        let driver = SystemOneDriver::new();
        let response = driver
            .decide(&test_request())
            .await
            .expect("live decide failed");
        println!("live decide response: {response:#?}");

        assert_eq!(response.answers.len(), 3, "one answer per question");

        match &response.answers["category"] {
            Answer::Choice {
                choice,
                probabilities,
                ..
            } => {
                assert!(
                    probabilities.contains_key(choice),
                    "choice {choice:?} missing from probabilities"
                );
                let sum: f64 = probabilities.values().sum();
                assert!(
                    (sum - 1.0).abs() < 1e-3,
                    "probabilities sum to {sum}, not ~1"
                );
            }
            other => panic!("category answer has wrong type: {other:?}"),
        }
        match &response.answers["urgent"] {
            Answer::Noul { noul } => {
                assert!(
                    (0.0..=1.0).contains(noul),
                    "noul probability {noul} out of range"
                );
            }
            other => panic!("urgent answer has wrong type: {other:?}"),
        }
        match &response.answers["quality"] {
            Answer::Score {
                score,
                probabilities,
                legend,
                ..
            } => {
                assert!((0.0..=2.0).contains(score), "score {score} outside 0..=2");
                assert_eq!(probabilities.len(), 3, "one probability per level");
                assert_eq!(legend.len(), 3, "one legend entry per level");
            }
            other => panic!("quality answer has wrong type: {other:?}"),
        }

        let usage = response.token_usage.expect("llama.cpp should report usage");
        assert!(usage.prompt_tokens > 0, "expected non-zero input tokens");
        assert_eq!(
            usage.completion_tokens, 3,
            "completion_tokens normalized to one per answer"
        );
    }
}
