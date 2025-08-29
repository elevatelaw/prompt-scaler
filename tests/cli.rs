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
use csv::{ReaderBuilder, WriterBuilder};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

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

/// Some cheap models for use with `--driver=native`.
static NATIVE_CHEAP_MODELS: &[&str] = &[
    // This should work, but we probably want to overhaul how we handle
    // API keys for to distinguish between LiteLLM and
    //"gpt-4o-mini",

    // Does not return JSON.
    //"claude-3-5-haiku-20241022",

    // Works fine in native mode!
    "gemini-2.0-flash",
];

/// Fake API key for local Ollama instance.
static OLLAMA_API_KEY: &str = "sk-1234";
/// API base URL for local Ollama instance.
static OLLAMA_API_BASE: &str = "http://localhost:11434/v1";

/// Fast Ollama models to test against.
static OLLAMA_FAST_MODELS: &[&str] = &["gemma3:4b-it-qat"];

/// AWS Bedrock models that are likely to work.
///
/// See the chart at https://aws.amazon.com/en/blogs/machine-learning/structured-data-response-with-amazon-bedrock-prompt-engineering-and-tool-use/.
///
/// For now, since we use a manual JSON schema passed in the system prompt, and not
/// tool calling, we need to avoid Haiku 3.0. Haiku 3.5 works about 98% of the time,
/// so it might be reasonable for production use with appropriate retries.
static BEDROCK_MODELS: &[&str] = &["us.anthropic.claude-sonnet-4-20250514-v1:0"];

/// Create a new `Command` with our binary.
fn cmd() -> Command {
    let mut cmd = Command::cargo_bin("prompt-scaler").unwrap();
    // Disable color so any RUST_LOG output is readable.
    cmd.env("NO_COLOR", "1");
    cmd
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
        .arg("tests/fixtures/texts/input.jsonl")
        .arg("--prompt")
        .arg("tests/fixtures/texts/prompt.toml")
        .assert()
        .success();
}

#[test]
#[ignore = "Needs LiteLLM running"]
fn test_chat_text_csv_input_litellm() {
    for &model in LITELLM_CHEAP_MODELS {
        println!("Testing model: {model}");
        cmd()
            .env("OPENAI_API_KEY", LITELLM_API_KEY)
            .env("OPENAI_API_BASE", LITELLM_API_BASE)
            .arg("chat")
            .arg("tests/fixtures/texts/input.csv")
            .arg("--allow-reordering")
            .arg("--model")
            .arg(model)
            .arg("--prompt")
            .arg("tests/fixtures/texts/prompt.toml")
            .assert()
            .success();
    }
}

#[test]
#[ignore = "Needs Ollama running"]
fn test_chat_text_csv_input_ollama() {
    for &model in OLLAMA_FAST_MODELS {
        println!("Testing model: {model}");
        cmd()
            .env("OPENAI_API_KEY", OLLAMA_API_KEY)
            .env("OPENAI_API_BASE", OLLAMA_API_BASE)
            .arg("chat")
            .arg("tests/fixtures/texts/input.csv")
            .args(["--jobs", "1", "--limit", "1"])
            .arg("--model")
            .arg(model)
            .arg("--prompt")
            .arg("tests/fixtures/texts/prompt.toml")
            .assert()
            .success();
    }
}

#[test]
#[ignore = "Needs LiteLLM running"]
fn test_chat_external_schema_csv_input_litellm() {
    // Prompts using JSON Schemas generated from various languages. See our
    // `Justfile` for how the schemas referred to by these files are generated.
    let prompts = ["prompt_py.toml", "prompt_ts.toml"];
    for prompt in prompts {
        println!("Testing schema prompt: {prompt}");
        cmd()
            .env("OPENAI_API_KEY", LITELLM_API_KEY)
            .env("OPENAI_API_BASE", LITELLM_API_BASE)
            .arg("chat")
            .arg("tests/fixtures/external_schemas/input.csv")
            .arg("--prompt")
            .arg(format!("tests/fixtures/external_schemas/{prompt}"))
            .arg("--model")
            .arg(LITELLM_CHEAP_MODELS[0])
            .assert()
            .success();
    }
}

#[test]
#[ignore = "Slightly expensive & needs LiteLLM running"]
fn test_chat_image_csv_input_litellm() {
    for &model in LITELLM_CHEAP_MODELS {
        println!("Testing model: {model}");
        cmd()
            .env("OPENAI_API_KEY", LITELLM_API_KEY)
            .env("OPENAI_API_BASE", LITELLM_API_BASE)
            .arg("chat")
            .arg("tests/fixtures/images/input.csv")
            .arg("--model")
            .arg(model)
            .arg("--prompt")
            .arg("tests/fixtures/images/prompt.toml")
            .assert()
            .success();
    }
}

#[test]
#[ignore = "Slightly expensive & needs various API keys in .env"]
fn test_chat_image_csv_input_native() {
    for &model in NATIVE_CHEAP_MODELS {
        println!("Testing model: {model}");
        cmd()
            .arg("chat")
            .arg("tests/fixtures/images/input.csv")
            .args(["--driver", "native"])
            .arg("--model")
            .arg(model)
            .arg("--prompt")
            .arg("tests/fixtures/images/prompt.toml")
            .assert()
            .success();
    }
}

#[test]
#[ignore = "Needs Ollama running"]
fn test_chat_text_csv_input_bedrock() {
    for &model in BEDROCK_MODELS {
        println!("Testing model: {model}");
        cmd()
            .env("OPENAI_API_KEY", OLLAMA_API_KEY)
            .env("OPENAI_API_BASE", OLLAMA_API_BASE)
            .arg("chat")
            .arg("tests/fixtures/texts/input.csv")
            .args(["--driver", "bedrock"])
            .args(["--jobs", "1", "--limit", "1"])
            .arg("--model")
            .arg(model)
            .arg("--prompt")
            .arg("tests/fixtures/texts/prompt.toml")
            .assert()
            .success();
    }
}

#[test]
#[ignore = "Slow & needs Ollama running"]
fn test_chat_image_csv_input_ollama() {
    for &model in OLLAMA_FAST_MODELS {
        println!("Testing model: {model}");
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
            .assert()
            .success();
    }
}

#[test]
#[ignore = "Needs LiteLLM running"]
fn test_ocr_pdf() {
    cmd()
        .env("OPENAI_API_KEY", LITELLM_API_KEY)
        .env("OPENAI_API_BASE", LITELLM_API_BASE)
        .arg("ocr")
        .arg("tests/fixtures/ocr/input.csv")
        .arg("--jobs")
        .arg("3")
        .arg("--model")
        .arg("gemini-2.0-flash")
        .assert()
        .success();
}

#[test]
#[ignore = "Needs LiteLLM running"]
fn test_ocr_pdf_with_options() {
    cmd()
        .env("OPENAI_API_KEY", LITELLM_API_KEY)
        .env("OPENAI_API_BASE", LITELLM_API_BASE)
        .arg("ocr")
        .arg("tests/fixtures/ocr/input.csv")
        .arg("--jobs")
        .arg("3")
        .arg("--model")
        .arg("gemini-2.0-flash")
        .args(["--offset", "0"])
        .args(["--limit", "1"])
        .args(["--allow-reordering"])
        .args(["--max-pages", "1"])
        .args(["--max-completion-tokens", "1000"])
        .args(["--temperature", "0.5"])
        .args(["--top-p", "0.1"])
        .args(["--timeout", "60"])
        .args(["--rate-limit", "10/s"])
        .assert()
        .success();
}

#[test]
#[ignore = "Needs LiteLLM running"]
fn test_ocr_rasterized() {
    cmd()
        .env("OPENAI_API_KEY", LITELLM_API_KEY)
        .env("OPENAI_API_BASE", LITELLM_API_BASE)
        .arg("ocr")
        .arg("tests/fixtures/ocr/input.csv")
        .arg("--jobs")
        .arg("3")
        .arg("--model")
        .arg("gemini-2.0-flash")
        .arg("--rasterize")
        .assert()
        .success();
}

#[test]
#[ignore = "Needs LiteLLM running"]
fn test_ocr_custom_prompt() {
    cmd()
        .env("OPENAI_API_KEY", LITELLM_API_KEY)
        .env("OPENAI_API_BASE", LITELLM_API_BASE)
        .arg("ocr")
        .arg("tests/fixtures/ocr/input.csv")
        .arg("--jobs")
        .arg("3")
        .arg("--prompt")
        // Same prompt as usual, but pass it explicitly.
        .arg("src/queues/ocr/engines/llm/default_ocr_prompt.toml")
        .arg("--model")
        .arg("gemini-2.0-flash")
        .assert()
        .success();
}

#[test]
fn test_ocr_pdftotext() {
    cmd()
        .arg("ocr")
        .arg("tests/fixtures/ocr/input.csv")
        .arg("--jobs")
        .arg("3")
        // Our image test case won't work, so allow more failures than usual.
        .arg("--allowed-failure-rate")
        .arg("0.5")
        .arg("--model")
        .arg("pdftotext")
        .assert()
        .success();
}

#[test]
fn test_ocr_tesseract() {
    cmd()
        .arg("ocr")
        .arg("tests/fixtures/ocr/input.csv")
        .arg("--jobs")
        .arg("3")
        .arg("--model")
        .arg("tesseract")
        .arg("--rasterize")
        .assert()
        .success();
}

#[test]
#[ignore = "Slightly expensive and needs AWS credentials"]
fn test_ocr_textract() {
    cmd()
        .arg("ocr")
        .arg("tests/fixtures/ocr/input.csv")
        .arg("--jobs")
        .arg("3")
        .arg("--model")
        .arg("textract")
        .assert()
        .success();
}

#[test]
#[ignore = "Slightly expensive and needs AWS credentials + files in bucket"]
fn test_ocr_textract_async() {
    dotenvy::dotenv().ok();

    // Prepare to make a temp copy of input.csv. NamedTempFile can theoretically
    // be insecure on Linux and Unix-like systems if a temporary file cleaner is
    // active and deletes the file before we're done with it. Since this a short
    // test, written in a fairly secure programming language, that OCRs things
    // in S3 buckets, the risks are extremely minimal.
    let mut reader = ReaderBuilder::new()
        .from_path("tests/fixtures/ocr/input.csv")
        .expect("Failed to read input.csv");
    let temp_file = NamedTempFile::new().expect("Failed to create temp file");
    let mut writer = WriterBuilder::new()
        .from_path(temp_file.path())
        .expect("Failed to create temp file writer");

    // Row format.
    #[derive(Deserialize, Serialize)]
    struct Row {
        id: String,
        path: String,
    }

    // Prepend paths with our S3 locations.
    let mut s3_location = std::env::var("S3_TEST_FIXTURE_LOCATION")
        .expect("S3_TEST_FIXTURE_LOCATION environment variable not set");
    if !s3_location.ends_with('/') {
        s3_location.push('/');
    }
    for row in reader.deserialize::<Row>() {
        let mut row = row.expect("Failed to read record");
        row.path = format!("{}{}", s3_location, row.path);
        writer.serialize(&row).expect("Failed to write record");
    }

    // Flush our writer.
    writer.flush().expect("Failed to flush writer");
    drop(writer);

    cmd()
        .arg("ocr")
        .arg(temp_file.path())
        .arg("--jobs")
        .arg("2")
        .arg("--model")
        .arg("textract-async")
        .arg("--include-page-breaks")
        .assert()
        // The actual line-feed will be escaped in the JSON output using JSON
        // escape sequences, not Rust ones.
        .stdout(predicates::str::contains("\\fOCR TEST DOCUMENT"))
        .success();
}
