//! CLI test cases.
//!
//! We run all tests against either LiteLLM (for models hosted elsewhere) or
//! Ollama's server (for local models). Theoretically LiteLLM supports proxying
//! to Ollama, but:
//!
//! 1. LiteLLM's Ollama support seems to avoid using /chat/completions and
//!    instead uses some older endpoint, losing support for critical features.
//! 2. It's convenient to be able to run LiteLLM tests using real credentials
//!    on CI runners and other machines that can't reasonably host Ollama.

use std::process::Command;

use assert_cmd::prelude::*;

/// Fake API key for local LiteLLM instance.
static LITELLM_API_KEY: &str = "sk-1234";
/// API base URL for local LiteLLM instance.
static LITELLM_API_BASE: &str = "http://localhost:4000/v1";

/// Standard cheap models from multiple providers, available through
/// our `litellm_config.yml` setup.
static LITELLM_CHEAP_MODELS: &[&str] = &[
    "gpt-4o-mini",
    "claude-3-5-haiku-20241022",
    "gemini-2.0-flash",
];

/// Fake API key for local Ollama instance.
static OLLAMA_API_KEY: &str = "sk-1234";
/// API base URL for local Ollama instance.
static OLLAMA_API_BASE: &str = "http://localhost:11434/v1";

/// Fast Ollama models to test against.
static OLLAMA_FAST_MODELS: &[&str] = &["gemma3:4b"];

/// Create a new `Command` with our binary.
fn cmd() -> Command {
    Command::cargo_bin("prompt-scaler").unwrap()
}

#[test]
fn test_help() {
    cmd().arg("--help").assert().success();
}

#[test]
fn test_version() {
    cmd().arg("--version").assert().success();
}

#[test]
#[ignore = "Needs LiteLLM running"]
fn test_chat_text_jsonl_input_litellm() {
    cmd()
        .env("OPENAI_API_KEY", LITELLM_API_KEY)
        .env("OPENAI_API_BASE", LITELLM_API_BASE)
        .arg("chat")
        .arg("tests/fixtures/input.jsonl")
        .arg("--prompt")
        .arg("tests/fixtures/prompt.toml")
        .arg("--schema")
        .arg("tests/fixtures/schema.json")
        .assert()
        .success();
}

#[test]
#[ignore = "Needs LiteLLM running"]
fn test_chat_text_csv_input_litellm() {
    for &model in LITELLM_CHEAP_MODELS {
        println!("Testing model: {}", model);
        cmd()
            .env("OPENAI_API_KEY", LITELLM_API_KEY)
            .env("OPENAI_API_BASE", LITELLM_API_BASE)
            .arg("chat")
            .arg("tests/fixtures/input.csv")
            .arg("--model")
            .arg(model)
            .arg("--prompt")
            .arg("tests/fixtures/prompt.toml")
            .arg("--schema")
            .arg("tests/fixtures/schema.json")
            .assert()
            .success();
    }
}

#[test]
#[ignore = "Needs Ollama running"]
fn test_chat_text_csv_input_ollama() {
    for &model in OLLAMA_FAST_MODELS {
        println!("Testing model: {}", model);
        cmd()
            .env("OPENAI_API_KEY", OLLAMA_API_KEY)
            .env("OPENAI_API_BASE", OLLAMA_API_BASE)
            .arg("chat")
            .arg("tests/fixtures/input.csv")
            .args(["--jobs", "1"])
            .arg("--model")
            .arg(model)
            .arg("--prompt")
            .arg("tests/fixtures/prompt.toml")
            .arg("--schema")
            .arg("tests/fixtures/schema.json")
            .assert()
            .success();
    }
}

#[test]
#[ignore = "Slightly expensive & needs LiteLLM running"]
fn test_chat_image_csv_input_litellm() {
    for &model in LITELLM_CHEAP_MODELS {
        println!("Testing model: {}", model);
        cmd()
            .env("OPENAI_API_KEY", LITELLM_API_KEY)
            .env("OPENAI_API_BASE", LITELLM_API_BASE)
            .arg("chat")
            .arg("tests/fixtures/images/input.csv")
            .arg("--model")
            .arg(model)
            .arg("--prompt")
            .arg("tests/fixtures/images/prompt.toml")
            .arg("--schema")
            .arg("tests/fixtures/images/schema.json")
            .assert()
            .success();
    }
}

#[test]
#[ignore = "Slow & needs Ollama running"]
fn test_chat_image_csv_input_ollama() {
    for &model in OLLAMA_FAST_MODELS {
        println!("Testing model: {}", model);
        cmd()
            .env("OPENAI_API_KEY", OLLAMA_API_KEY)
            .env("OPENAI_API_BASE", OLLAMA_API_BASE)
            .arg("chat")
            .arg("tests/fixtures/images/input.csv")
            .args(["--jobs", "1"])
            .arg("--model")
            .arg(model)
            .arg("--prompt")
            .arg("tests/fixtures/images/prompt.toml")
            .arg("--schema")
            .arg("tests/fixtures/images/schema.json")
            .assert()
            .success();
    }
}
