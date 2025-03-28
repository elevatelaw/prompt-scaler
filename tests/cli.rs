//! CLI test cases.

use std::process::Command;

use assert_cmd::prelude::*;

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
fn test_chat_jsonl() {
    cmd()
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
fn test_chat_csv() {
    cmd()
        .arg("chat")
        .arg("tests/fixtures/input.csv")
        .arg("--prompt")
        .arg("tests/fixtures/prompt.toml")
        .arg("--schema")
        .arg("tests/fixtures/schema.json")
        .assert()
        .success();
}
