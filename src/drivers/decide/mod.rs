//! Drivers for decision models.

// We have not yet finished hooking this up.
#![allow(dead_code)]

use std::{collections::BTreeMap, fmt};

use anyhow::ensure;
use async_trait::async_trait;
use clap::ValueEnum;
use schemars::JsonSchema;

use super::{DriverError, TokenUsage};
use crate::prelude::*;

pub mod system_one;

/// A request to a decision model.
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct DecideRequest {
    /// Model to use. Required by most APIs.
    model: String,

    /// The state to send.
    state: Value,

    /// Images to send, as data URLs. Only supported by limited number
    /// of models, many served with llama.cpp.
    #[serde(skip_serializing_if = "Option::is_none")]
    images: Option<Vec<String>>,

    /// Questions to ask.
    questions: BTreeMap<String, Question>,
}

/// A question. This will not undergo template instantiation.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
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
    pub fn validate(&self) -> Result<()> {
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
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct NoulCriteria {
    /// The criteria for a true answer.
    pub r#true: Option<String>,
    /// The criteria for a false answer.
    pub r#false: Option<String>,
}

/// A response from a decision model.
///
/// This is our standard response type, which
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DecideResponse {
    /// The name of the model used. May be different from the requested model name, thanks
    /// to model name resolution.
    model: String,

    /// Answers to the questions.
    answers: BTreeMap<String, Answer>,

    /// Token usage. `None` if the server reported no usage at all. In some
    /// cases, servers may return 0 output tokens used. We typically
    /// normalize this 1 token per answer.
    token_usage: Option<TokenUsage>,
}

/// An answer to a [`Question`].
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
#[non_exhaustive]
pub enum Answer {
    /// Answer to a true/false question.
    Noul {
        /// Probability that the generated token would have represented `true`.
        noul: f64,
    },
    /// Answer to a multiple-choice question.
    Choice {
        /// The most likely choice.
        choice: String,
        /// The probability that each of the choices would have been generated.
        probabilities: BTreeMap<String, f64>,
        /// Confidence in the choice we chose. The meaning of this field
        /// unfortunately differs between models.
        confidence: f64,
    },
    /// Answer to a scoring question.
    Score {
        /// Expected value of the score, counting from 0. Will typically fall
        /// in between two labels.
        score: f64,
        /// A mapping from score values (represented as integer strings) to the
        /// descriptions of those labels.
        legend: BTreeMap<String, String>,
        /// The probability that each of the score values would have been generated.
        probabilities: BTreeMap<String, f64>,
        /// Confidence in the score we chose. The meaning of this field may
        /// differs between models.
        confidence: f64,
    },
}

/// Our different decide driver types.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, ValueEnum,
)]
#[clap(rename_all = "snake_case")]
pub enum DecideDriverType {
    /// llama.cpp, Jev, and other System One-compatible servers. Set
    /// `SYSTEM_ONE_API_URL` to point at a server.
    #[default]
    SystemOne,
}

impl DecideDriverType {
    /// Instantiate an appropriate driver.
    pub fn create_driver(&self) -> Result<Box<dyn DecideDriver>> {
        match self {
            Self::SystemOne => Ok(Box::new(system_one::SystemOneDriver::new())),
        }
    }
}

impl fmt::Display for DecideDriverType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SystemOne => write!(f, "system_one"),
        }
    }
}

/// Check that `answers` correspond to the questions we sent.
///
/// Decision-model servers are deterministic, so a mismatch here indicates a
/// bug in the endpoint, not flaky generation: failures are always fatal
/// ([`DriverError::invalid_output`]), because retrying would send the same
/// request to the same broken server.
///
/// This is intentionally minimal. We already parsed the answers' structure;
/// what parsing can't check is the *relation* between answers and
/// questions, so that's all we check: matching names, matching kinds, and
/// choices we actually offered. We deliberately do not check arithmetic
/// properties (probabilities summing to 1, choice equalling argmax): those
/// are server-quality questions that shouldn't fail our records.
pub fn verify_answers(
    questions: &BTreeMap<String, Question>,
    answers: &BTreeMap<String, Answer>,
) -> Result<(), DriverError> {
    // We must have answered exactly the questions we asked.
    let missing: Vec<&String> = questions
        .keys()
        .filter(|name| !answers.contains_key(*name))
        .collect();
    let extra: Vec<&String> = answers
        .keys()
        .filter(|name| !questions.contains_key(*name))
        .collect();
    if !missing.is_empty() || !extra.is_empty() {
        return Err(DriverError::invalid_output(anyhow!(
            "Answers don't match the questions sent: missing {missing:?}, unexpected {extra:?}"
        )));
    }

    for (name, question) in questions {
        let answer = &answers[name]; // presence guaranteed above
        match (question, answer) {
            (Question::Noul { .. }, Answer::Noul { .. }) => {}
            (Question::Choice { criteria, .. }, Answer::Choice { choice, .. }) => {
                if !criteria.contains_key(choice) {
                    return Err(DriverError::invalid_output(anyhow!(
                        "Answer to {name:?} chose {choice:?}, which was not offered"
                    )));
                }
            }
            (Question::Score { .. }, Answer::Score { .. }) => {}
            _ => {
                return Err(DriverError::invalid_output(anyhow!(
                    "Answer to {name:?} has the wrong type: {answer:?}"
                )));
            }
        }
    }

    Ok(())
}

/// A driver for decision ("system one") models.
#[async_trait]
pub trait DecideDriver: fmt::Debug + Send + Sync + 'static {
    /// Answer the questions in `request`.
    ///
    /// We include `model` inside `request`. Callers are responsible for
    /// handling timeouts and retry behavior.
    async fn decide(
        &self,
        request: &DecideRequest,
    ) -> Result<DecideResponse, DriverError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn questions() -> BTreeMap<String, Question> {
        BTreeMap::from([
            (
                "category".to_string(),
                Question::Choice {
                    instructions: "What kind of document is this?".to_string(),
                    criteria: BTreeMap::from([
                        ("invoice".to_string(), None),
                        ("memo".to_string(), None),
                    ]),
                },
            ),
            (
                "urgent".to_string(),
                Question::Noul {
                    instructions: "Is it urgent?".to_string(),
                    criteria: None,
                },
            ),
            (
                "quality".to_string(),
                Question::Score {
                    instructions: "How well written is it?".to_string(),
                    criteria: vec!["bad".to_string(), "good".to_string()],
                },
            ),
        ])
    }

    fn answers() -> BTreeMap<String, Answer> {
        BTreeMap::from([
            (
                "category".to_string(),
                Answer::Choice {
                    choice: "invoice".to_string(),
                    probabilities: BTreeMap::from([
                        ("invoice".to_string(), 0.9),
                        ("memo".to_string(), 0.1),
                    ]),
                    confidence: 0.8,
                },
            ),
            ("urgent".to_string(), Answer::Noul { noul: 0.7 }),
            (
                "quality".to_string(),
                Answer::Score {
                    score: 1.0,
                    legend: BTreeMap::from([
                        ("0".to_string(), "bad".to_string()),
                        ("1".to_string(), "good".to_string()),
                    ]),
                    probabilities: BTreeMap::from([
                        ("0".to_string(), 0.5),
                        ("1".to_string(), 0.5),
                    ]),
                    confidence: 0.0,
                },
            ),
        ])
    }

    #[test]
    fn verify_accepts_matching_answers() {
        verify_answers(&questions(), &answers()).unwrap();
    }

    #[test]
    fn verify_rejects_missing_answer() {
        let mut answers = answers();
        answers.remove("urgent");
        let err = verify_answers(&questions(), &answers).unwrap_err();
        assert!(err.to_string().contains("missing [\"urgent\"]"), "{err}");
    }

    #[test]
    fn verify_rejects_extra_answer() {
        let mut answers = answers();
        answers.insert("bogus".to_string(), Answer::Noul { noul: 0.5 });
        let err = verify_answers(&questions(), &answers).unwrap_err();
        assert!(err.to_string().contains("unexpected [\"bogus\"]"), "{err}");
    }

    #[test]
    fn verify_rejects_wrong_kind() {
        let mut answers = answers();
        answers.insert(
            "urgent".to_string(),
            Answer::Score {
                score: 0.5,
                legend: BTreeMap::from([
                    ("0".to_string(), "a".to_string()),
                    ("1".to_string(), "b".to_string()),
                ]),
                probabilities: BTreeMap::from([
                    ("0".to_string(), 0.5),
                    ("1".to_string(), 0.5),
                ]),
                confidence: 0.0,
            },
        );
        let err = verify_answers(&questions(), &answers).unwrap_err();
        assert!(err.to_string().contains("wrong type"), "{err}");
    }

    #[test]
    fn verify_rejects_unoffered_choice() {
        let mut answers = answers();
        answers.insert(
            "category".to_string(),
            Answer::Choice {
                choice: "receipt".to_string(),
                probabilities: BTreeMap::from([
                    ("invoice".to_string(), 0.9),
                    ("memo".to_string(), 0.1),
                ]),
                confidence: 0.8,
            },
        );
        let err = verify_answers(&questions(), &answers).unwrap_err();
        assert!(err.to_string().contains("was not offered"), "{err}");
    }
}
