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

use std::process::{Command, Stdio};

use assert_cmd::prelude::*;
use csv::{ReaderBuilder, WriterBuilder};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

/// Endpoint for local `llama-server` instance. This is OpenAI-compatible, it
/// acts as a strong replacement for tools like Ollama. We use this to test the
/// things that we can reasonably test locally.
static LLAMA_SERVER_ENDPOINT: &str = "http://localhost:8080/v1";
/// Our llama-server API key. By default, llama-server ignores this unless you
/// configure an API key.
static LLAMA_SERVER_API_KEY: &str = "sk-1234";
/// Default model to use for `llama-server` in tests. Should be a small, fast
/// model with at least some very basic OCR abilities.
static LLAMA_SERVER_MODEL: &str = "unsloth/gemma-4-E2B-it-GGUF:Q4_K_M";

/// Cheap models that route to their provider's native adapter (the default
/// driver). Each requires the matching API key in `.env`.
static NATIVE_CHEAP_MODELS: &[&str] = &[
    // Gemini via AI Studio.
    "gemini-2.5-flash",
    // Haiku is cheap enough for testing and finally handles JSON.
    "claude-haiku-4-5-20251001",
    // GPT-5.4 Nano is a cheap, current GPT model.
    "gpt-5.4-nano",
];

/// Some cheap models for use with `--driver=vertex`.
static VERTEX_CHEAP_MODELS: &[&str] = &["gemini-2.5-flash"];

/// AWS Bedrock models that are likely to work.
///
/// See the chart at https://aws.amazon.com/en/blogs/machine-learning/structured-data-response-with-amazon-bedrock-prompt-engineering-and-tool-use/.
///
/// For now, since we use a manual JSON schema passed in the system prompt, and not
/// tool calling, we need to avoid Haiku 3.0. Haiku 3.5 works about 98% of the time,
/// so it might be reasonable for production use with appropriate retries.
static BEDROCK_MODELS: &[&str] = &["us.anthropic.claude-haiku-4-5-20251001-v1:0"];

/// Create a new `Command` with our binary.
fn cmd() -> Command {
    let mut cmd = Command::cargo_bin("prompt-scaler").unwrap();
    // Disable color so any RUST_LOG output is readable.
    cmd.env("NO_COLOR", "1");
    cmd
}

/// Add `llama-server` environment variables to a command, for testing against a local
/// `llama-server` instance.
trait LlamaServerCommandExt {
    fn with_llama_server_env(&mut self) -> &mut Self;
}

impl LlamaServerCommandExt for Command {
    fn with_llama_server_env(&mut self) -> &mut Self {
        self.env("OPENAI_API_BASE", LLAMA_SERVER_ENDPOINT)
            .env("OPENAI_API_KEY", LLAMA_SERVER_API_KEY)
    }
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
#[ignore = "Needs llama-server running"]
fn test_chat_text_jsonl_input_llama() {
    cmd()
        .with_llama_server_env()
        .arg("chat")
        .arg("tests/fixtures/texts/input.jsonl")
        .args(["--model", LLAMA_SERVER_MODEL])
        .arg("--prompt")
        .arg("tests/fixtures/texts/prompt.toml")
        .assert()
        .success();
}

#[test]
#[ignore = "Needs llama-server running"]
fn test_chat_text_csv_input_llama() {
    cmd()
        .with_llama_server_env()
        .arg("chat")
        .arg("tests/fixtures/texts/input.csv")
        .args(["--model", LLAMA_SERVER_MODEL])
        .arg("--allow-reordering")
        .arg("--prompt")
        .arg("tests/fixtures/texts/prompt.toml")
        .assert()
        .success();
}

#[test]
#[ignore = "Needs llama-server running"]
fn test_chat_external_schema_csv_input_llama() {
    // Prompts using JSON Schemas generated from various languages. See our
    // `Justfile` for how the schemas referred to by these files are generated.
    let prompts = ["prompt_py.toml", "prompt_ts.toml"];
    for prompt in prompts {
        println!("Testing schema prompt: {prompt}");
        cmd()
            .with_llama_server_env()
            .arg("chat")
            .arg("tests/fixtures/external_schemas/input.csv")
            .args(["--model", LLAMA_SERVER_MODEL])
            .arg("--prompt")
            .arg(format!("tests/fixtures/external_schemas/{prompt}"))
            .assert()
            .success();
    }
}

#[test]
#[ignore = "Needs llama-server running"]
fn test_chat_image_csv_input_llama() {
    cmd()
        .with_llama_server_env()
        .arg("chat")
        .arg("tests/fixtures/images/input.csv")
        .arg("--model")
        .arg(LLAMA_SERVER_MODEL)
        .arg("--prompt")
        .arg("tests/fixtures/images/prompt.toml")
        .assert()
        .success();
}

#[test]
#[ignore = "Needs native API keys in .env and is slightly expensive"]
fn test_chat_image_csv_input_native() {
    for &model in NATIVE_CHEAP_MODELS {
        println!("Testing model: {model}");
        cmd()
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
#[ignore = "Needs Vertex credentials in .env and is slightly expensive"]
fn test_chat_image_csv_input_vertex() {
    for &model in VERTEX_CHEAP_MODELS {
        println!("Testing model: {model}");
        cmd()
            .arg("chat")
            .arg("tests/fixtures/images/input.csv")
            .args(["--driver", "vertex"])
            .arg("--model")
            .arg(model)
            .arg("--prompt")
            .arg("tests/fixtures/images/prompt.toml")
            .stderr(Stdio::inherit())
            .assert()
            .success();
    }
}

#[test]
#[ignore = "Needs AWS Bedrock credentials in .env and is slightly expensive"]
fn test_chat_text_csv_input_bedrock() {
    for &model in BEDROCK_MODELS {
        println!("Testing model: {model}");
        cmd()
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
#[ignore = "Needs AWS Bedrock credentials in .env and is slightly expensive"]
fn test_chat_image_csv_input_bedrock() {
    for &model in BEDROCK_MODELS {
        println!("Testing model: {model}");
        cmd()
            .arg("chat")
            .arg("tests/fixtures/images/input.csv")
            .args(["--driver", "bedrock"])
            .args(["--jobs", "1", "--limit", "1"])
            .arg("--model")
            .arg(model)
            .arg("--prompt")
            .arg("tests/fixtures/images/prompt.toml")
            .assert()
            .success();
    }
}

#[test]
#[ignore = "Needs `llama-server` running"]
fn test_ocr_llama() {
    cmd()
        .with_llama_server_env()
        .arg("ocr")
        .arg("tests/fixtures/ocr/input.csv")
        .args(["--base-dir", "tests/fixtures/"])
        .args(["--jobs", "3"])
        .args(["--model", LLAMA_SERVER_MODEL])
        // Rasterization is needed for PDFs with llama-server.
        .arg("--rasterize")
        .assert()
        .success();
}

#[test]
#[ignore = "Needs Vertex credentials in .env and is slightly expensive"]
fn test_ocr_pdf_vertex() {
    for &model in VERTEX_CHEAP_MODELS {
        println!("Testing model: {model}");
        cmd()
            .arg("ocr")
            .arg("tests/fixtures/ocr/input.csv")
            .args(["--base-dir", "tests/fixtures/"])
            .args(["--jobs", "3"])
            .args(["--model", model])
            .args(["--driver", "vertex"])
            .assert()
            .success();
    }
}

#[test]
#[ignore = "Needs Vertex credentials in .env and is slightly expensive"]
fn test_ocr_pdf_with_options_vertex() {
    for &model in VERTEX_CHEAP_MODELS {
        println!("Testing model: {model}");
        cmd()
            .arg("ocr")
            .arg("tests/fixtures/ocr/input.csv")
            .args(["--base-dir", "tests/fixtures/"])
            .args(["--jobs", "3"])
            .args(["--model", model])
            .args(["--driver", "vertex"])
            .args(["--offset", "0"])
            .args(["--limit", "1"])
            .args(["--allow-reordering"])
            .args(["--max-pages", "1"])
            .args(["--max-completion-tokens", "1000"])
            .args(["--temperature", "0.5"])
            .args(["--top-p", "0.1"])
            .args(["--llm-timeout", "60"])
            .args(["--rate-limit", "10/s"])
            .assert()
            .success();
    }
}

#[test]
#[ignore = "Needs Vertex credentials in .env and is slightly expensive"]
fn test_ocr_rasterized_vertex() {
    for &model in VERTEX_CHEAP_MODELS {
        println!("Testing model: {model}");
        cmd()
            .arg("ocr")
            .arg("tests/fixtures/ocr/input.csv")
            .args(["--base-dir", "tests/fixtures/"])
            .args(["--jobs", "3"])
            .args(["--model", model])
            .args(["--driver", "vertex"])
            .arg("--rasterize")
            .assert()
            .success();
    }
}

#[test]
#[ignore = "Needs Vertex credentials in .env and is slightly expensive"]
fn test_ocr_custom_prompt_vertex() {
    for &model in VERTEX_CHEAP_MODELS {
        println!("Testing model: {model}");
        cmd()
            .arg("ocr")
            .arg("tests/fixtures/ocr/input.csv")
            .arg("--base-dir")
            .arg("tests/fixtures/")
            .args(["--jobs", "3"])
            .arg("--prompt")
            // Same prompt as usual, but pass it explicitly.
            .arg("src/queues/ocr/engines/llm/default_ocr_prompt.toml")
            .args(["--model", model])
            .args(["--driver", "vertex"])
            .assert()
            .success();
    }
}

#[test]
fn test_ocr_pdftotext() {
    cmd()
        .arg("ocr")
        .arg("tests/fixtures/ocr/input.csv")
        .args(["--base-dir", "tests/fixtures/"])
        .arg("--jobs")
        .arg("3")
        // Our image test case won't work, so allow more failures than usual.
        .arg("--allowed-failure-rate")
        .arg("0.5")
        .arg("--model")
        .arg("pdftotext")
        .arg("--page-memory-limit")
        .arg("1GiB")
        .assert()
        .success();
}

#[test]
fn test_ocr_pdftotext_password() {
    cmd()
        .arg("ocr")
        .arg("tests/fixtures/ocr/passwd_input_good.csv")
        .args(["--base-dir", "tests/fixtures/ocr/"])
        .arg("--jobs")
        .arg("3")
        .arg("--model")
        .arg("pdftotext")
        .arg("--page-memory-limit")
        .arg("1GiB")
        .assert()
        .success();
}

#[test]
fn test_ocr_pdftotext_password_bad() {
    cmd()
        .arg("ocr")
        .arg("tests/fixtures/ocr/passwd_input_bad.csv")
        .args(["--base-dir", "tests/fixtures/ocr/"])
        .arg("--jobs")
        .arg("3")
        .arg("--model")
        .arg("pdftotext")
        .arg("--page-memory-limit")
        .arg("1GiB")
        .assert()
        .stdout(predicates::str::contains("two_pages_passwd_blank"))
        .stdout(predicates::str::contains("two_pages_passwd_wrong"))
        .stdout(predicates::str::contains("Incorrect password").count(2))
        .failure();
}

#[test]
fn test_ocr_tesseract() {
    cmd()
        .arg("ocr")
        .arg("tests/fixtures/ocr/input.csv")
        .args(["--base-dir", "tests/fixtures/"])
        .arg("--jobs")
        .arg("3")
        .arg("--model")
        .arg("tesseract")
        .arg("--rasterize")
        .assert()
        .success();
}

#[test]
fn test_ocr_tesseract_password() {
    cmd()
        .arg("ocr")
        .arg("tests/fixtures/ocr/passwd_input_good.csv")
        .args(["--base-dir", "tests/fixtures/ocr/"])
        .arg("--jobs")
        .arg("3")
        .arg("--model")
        .arg("tesseract")
        .arg("--rasterize")
        .assert()
        .success();
}

#[test]
fn test_ocr_tesseract_password_bad() {
    cmd()
        .arg("ocr")
        .arg("tests/fixtures/ocr/passwd_input_bad.csv")
        .args(["--base-dir", "tests/fixtures/ocr/"])
        .arg("--jobs")
        .arg("3")
        .arg("--model")
        .arg("tesseract")
        .arg("--rasterize")
        .assert()
        .stdout(predicates::str::contains("two_pages_passwd_blank"))
        .stdout(predicates::str::contains("two_pages_passwd_wrong"))
        .stdout(predicates::str::contains("Incorrect password").count(2))
        .failure();
}

#[test]
fn test_ocr_tiff_tesseract() {
    cmd()
        .arg("ocr")
        .arg("tests/fixtures/ocr/tiff_input.csv")
        .arg("--jobs")
        .arg("3")
        .arg("--model")
        .arg("tesseract")
        .arg("--rasterize") // Required by tesseract engine
        .assert()
        .success();
}

#[test]
#[ignore = "Slightly expensive and needs AWS credentials"]
fn test_ocr_textract() {
    cmd()
        .arg("ocr")
        .arg("tests/fixtures/ocr/input.csv")
        .args(["--base-dir", "tests/fixtures/"])
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
        row.path = format!("{}tests/fixtures/{}", s3_location, row.path);
        eprintln!("Writing record with path: {}", row.path);
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

#[test]
#[ignore = "Slightly expensive and needs AWS credentials + password-protected PDF in bucket"]
fn test_ocr_textract_async_password_protected() {
    dotenvy::dotenv().ok();

    // textract-async does not accept passwords, so OCRing an encrypted PDF
    // should produce an error from AWS Textract. We follow the same pattern
    // as test_ocr_textract_async, reading from passwd_input_good.csv and
    // constructing S3 URIs.
    let mut reader = ReaderBuilder::new()
        .from_path("tests/fixtures/ocr/passwd_input_good.csv")
        .expect("Failed to read passwd_input_good.csv");
    let temp_file = NamedTempFile::new().expect("Failed to create temp file");
    let mut writer = WriterBuilder::new()
        .from_path(temp_file.path())
        .expect("Failed to create temp file writer");

    // Row format (passwd_input_good.csv uses the same schema).
    #[derive(Deserialize, Serialize)]
    struct Row {
        id: String,
        path: String,
        password: String,
    }

    // Prepend paths with our S3 locations. passwd_input_good.csv uses bare
    // filenames (resolved via --base-dir locally), so we append the /ocr/
    // suffix just as the other pwd-input tests would via --base-dir.
    let mut s3_location = std::env::var("S3_TEST_FIXTURE_LOCATION")
        .expect("S3_TEST_FIXTURE_LOCATION environment variable not set");
    if !s3_location.ends_with('/') {
        s3_location.push('/');
    }
    for row in reader.deserialize::<Row>() {
        let mut row = row.expect("Failed to read record");
        row.path = format!("{}tests/fixtures/ocr/{}", s3_location, row.path);
        eprintln!("Writing record with path: {}", row.path);
        writer.serialize(&row).expect("Failed to write record");
    }

    // Flush our writer.
    writer.flush().expect("Failed to flush writer");
    drop(writer);

    cmd()
        .arg("ocr")
        .arg(temp_file.path())
        .arg("--model")
        .arg("textract-async")
        .assert()
        // Verify the error output includes the document ID and a failed status.
        // Textract returns a generic "Textract job ... failed" for encrypted
        // PDFs, so we check for the standard JSONL error markers.
        .stdout(predicates::str::contains("two_pages_passwd"))
        .stdout(predicates::str::contains("\"status\":\"failed\""))
        .failure();
}

#[test]
#[ignore = "Needs llama-server running"]
fn test_chat_skip_processing_and_passthrough_llama() {
    use serde_json::Value;

    let output = cmd()
        .with_llama_server_env()
        .arg("chat")
        .arg("tests/fixtures/skip_and_passthrough/input.jsonl")
        .arg("--prompt")
        .arg("tests/fixtures/skip_and_passthrough/prompt.toml")
        .args(["--model", LLAMA_SERVER_MODEL])
        .stdout(Stdio::piped())
        .output()
        .expect("Failed to execute command");
    if !output.status.success() {
        eprintln!("STDERR:\n{}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).expect("Invalid UTF-8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "Should have 3 output records");

    // Parse each line as JSON
    let records: Vec<Value> = lines
        .iter()
        .map(|line| serde_json::from_str(line).expect("Failed to parse JSON"))
        .collect();

    // Check first record (skip1)
    assert_eq!(records[0]["id"], "skip1");
    assert_eq!(records[0]["status"], "skipped");
    assert!(records[0]["response"].is_null());
    assert_eq!(records[0]["passthrough_data"]["custom"], "data");
    assert_eq!(records[0]["passthrough_data"]["count"], 42);

    // Check second record (normal)
    assert_eq!(records[1]["id"], "normal");
    assert_eq!(records[1]["status"], "ok");
    assert!(records[1]["response"].is_object());
    assert!(records[1]["response"]["punchline"].is_string());
    assert_eq!(records[1]["passthrough_data"]["tag"], "test");

    // Check third record (skip2)
    assert_eq!(records[2]["id"], "skip2");
    assert_eq!(records[2]["status"], "skipped");
    assert!(records[2]["response"].is_null());
    assert_eq!(records[2]["passthrough_data"]["another"], "value");
}

#[test]
fn test_chat_echo_driver() {
    use serde_json::Value;

    let output = cmd()
        .arg("chat")
        .arg("tests/fixtures/echo/input.csv")
        .arg("--prompt")
        .arg("tests/fixtures/echo/prompt.toml")
        .arg("--driver")
        .arg("echo")
        .arg("--model")
        .arg("test-model")
        .stdout(Stdio::piped())
        .output()
        .expect("Failed to execute command");
    if !output.status.success() {
        eprintln!("STDERR:\n{}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).expect("Invalid UTF-8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "Should have 1 output record");

    // Parse the line as JSON
    let record: Value = serde_json::from_str(lines[0]).expect("Failed to parse JSON");

    // Check the record
    assert_eq!(record["id"], "1");
    assert_eq!(record["status"], "ok");
    assert_eq!(record["response"]["echo"], "Hello world");
}
