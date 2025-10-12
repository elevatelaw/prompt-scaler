# Implementation Plan: Add `skip_processing` and `passthrough_data` to Chat Input

## Overview
Add two optional fields to the `chat` command's input format:
1. **`skip_processing`** (bool): When true, skip LLM processing entirely (including prompt rendering) and return `status: "skipped"`
2. **`passthrough_data`** (Map<String, Value>): Always pass through to output if present (as a nested object, NOT flattened)

## Key Design Decisions (per user guidance)
- Use `serde_json::Map<String, Value>` (aliased as JsonObject) for passthrough_data - no parsing ambiguity
- Add `WorkStatus::is_success()` method to encapsulate which statuses count as successes
- `skip_processing` skips prompt rendering entirely (avoids validation issues, improves performance)
- `Skipped` records don't contribute to failure counts or cost estimates
- **`passthrough_data` is NOT flattened** - remains as a nested object under the `passthrough_data` key
- One combined test fixture/test for both features

## Detailed Changes

### 1. Modify `WorkStatus` enum (src/queues/work.rs:80-89)

Add `Skipped` variant after `Ok`:

```rust
#[derive(Clone, Copy, Debug, JsonSchema, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkStatus {
    // The work item was successful.
    Ok,

    // The work item was skipped.
    Skipped,

    // Partial data.
    Incomplete,

    // The work item failed.
    Failed,
}
```

Add `is_success()` method:

```rust
impl WorkStatus {
    /// Returns true if this status represents a successful outcome.
    pub fn is_success(self) -> bool {
        matches!(self, WorkStatus::Ok | WorkStatus::Skipped)
    }
}
```

### 2. Update counter logic (src/queues/work.rs:217)

Change:
```rust
if item.status != WorkStatus::Ok {
    counters.failure_count += 1;
}
```

To:
```rust
if !item.status.is_success() {
    counters.failure_count += 1;
}
```

This ensures `Skipped` records aren't counted as failures.

### 3. Modify `ChatInput` struct (src/queues/chat.rs:22-29)

Add fields (both optional, won't be flattened):

```rust
/// An input record.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct ChatInput {
    /// Skip LLM processing and return status: "skipped"
    #[serde(default)]
    pub skip_processing: Option<bool>,

    /// Arbitrary data to pass through to output
    #[serde(default)]
    pub passthrough_data: Option<JsonObject>,

    /// Other fields. We keep these "flattened" in the record because they're
    /// under the control of the caller, and because our input format may be a
    /// CSV file, which is inherently "flat".
    #[serde(flatten)]
    pub template_bindings: JsonObject,
}
```

### 4. Modify `ChatOutput` struct (src/queues/chat.rs:32-37)

Add field (NOT flattened):

```rust
/// An output record.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct ChatOutput {
    /// The response from the LLM. If this is present, the request succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,

    /// Passthrough data from input
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passthrough_data: Option<JsonObject>,
}
```

### 5. Update `run_chat()` function (src/queues/chat.rs:209-244)

Extract new fields BEFORE clearing template_bindings (insert after line 213, before line 215):

```rust
async fn run_chat(
    state: Arc<ProcessorState>,
    mut input_record: WorkInput<ChatInput>,
) -> Result<WorkOutput<ChatOutput>> {
    let id = input_record.id.clone();

    // Extract skip_processing and passthrough_data BEFORE we clear template_bindings
    let skip_processing = input_record.data.skip_processing.unwrap_or(false);
    let passthrough_data = input_record.data.passthrough_data.take();

    // Early return if skip_processing is true (before prompt rendering)
    if skip_processing {
        return Ok(WorkOutput {
            id,
            status: WorkStatus::Skipped,
            errors: vec![],
            estimated_cost: None,
            token_usage: None,
            data: ChatOutput {
                response: None,
                passthrough_data
            },
        });
    }

    // Render our prompt.
    trace!(
        template_bindings = ?input_record.data.template_bindings,
        "Template bindings"
    );
    let prompt = state.prompt.render(&input_record.data.template_bindings)?;

    // ... rest of function
}
```

At the end of the function, update the call to `from_resolved_result`:

```rust
Ok(WorkOutput::<ChatOutput>::from_resolved_result(
    id,
    state.model_info,
    result,
    passthrough_data,  // NEW: pass through the passthrough_data
))
```

### 6. Update `from_resolved_result()` (src/queues/chat.rs:48-116)

Add `passthrough_data: Option<JsonObject>` parameter and thread it through to `ChatOutput` in all match arms:

```rust
impl WorkOutput<ChatOutput> {
    /// Create a new output record from a [`ResolvedResult`].
    fn from_resolved_result(
        id: Value,
        model: Option<&LiteLlmModel>,
        result: ResolvedResult<(), (), ChatCompletionResponse, anyhow::Error>,
        passthrough_data: Option<JsonObject>,  // NEW parameter
    ) -> Self {
        let estimate_cost =
            |usage: Option<&TokenUsage>| usage.and_then(|u| u.estimate_cost(model));
        let full_err = |err: anyhow::Error| format!("{err:?}");
        match result {
            ResolvedResult::Ok {
                output:
                    ChatCompletionResponse {
                        response,
                        token_usage,
                    },
                ..
            } => WorkOutput {
                id,
                status: WorkStatus::Ok,
                errors: vec![],
                estimated_cost: estimate_cost(token_usage.as_ref()),
                token_usage,
                data: ChatOutput {
                    response: Some(response),
                    passthrough_data,  // NEW: include passthrough_data
                },
            },
            ResolvedResult::Fatal { error, .. } => WorkOutput::new_failed(
                id,
                vec![full_err(error)],
                ChatOutput::empty_for_error(passthrough_data),  // NEW: pass passthrough_data
            ),
            ResolvedResult::Recovered {
                output:
                    ChatCompletionResponse {
                        response,
                        token_usage,
                    },
                retry_errors,
                ..
            } => WorkOutput {
                id,
                status: WorkStatus::Ok,
                errors: retry_errors.into_iter().map(full_err).collect(),
                estimated_cost: estimate_cost(token_usage.as_ref()),
                token_usage,
                data: ChatOutput {
                    response: Some(response),
                    passthrough_data,  // NEW: include passthrough_data
                },
            },
            ResolvedResult::GivenUp {
                retry_errors,
                fatal_error,
                ..
            }
            | ResolvedResult::Unrecoverable {
                retry_errors,
                fatal_error,
                ..
            } => WorkOutput::new_failed(
                id,
                retry_errors
                    .into_iter()
                    .map(full_err)
                    .chain(iter::once(full_err(fatal_error)))
                    .collect(),
                ChatOutput::empty_for_error(passthrough_data),  // NEW: pass passthrough_data
            ),
        }
    }
}
```

### 7. Update `ChatOutput::empty_for_error()` (src/queues/chat.rs:40-43)

Add `passthrough_data` parameter:

```rust
impl ChatOutput {
    /// Create an empty chat output record for use when an error occurs.
    pub fn empty_for_error(passthrough_data: Option<JsonObject>) -> Self {
        Self {
            response: None,
            passthrough_data,
        }
    }
}
```

### 8. Update JSON Schemas

Regenerate schemas:
```bash
cargo run schema chat-input > schemas/ChatInput.json
cargo run schema chat-output > schemas/ChatOutput.json
```

### 9. Create Test Fixture (tests/fixtures/skip_and_passthrough/)

**input.csv**:
```csv
id,skip_processing,passthrough_data,joke
skip1,true,"{""custom"":""data"",""count"":42}",This won't be processed
normal,false,"{""tag"":""test""}",Why did the chicken cross the road?
skip2,true,"{""another"":""value""}",Also skipped
```

**prompt.toml**:
```toml
developer = """
Answer the joke with a short, appropriate punchline.
"""

[response_schema]
description = "The response to a joke."

[response_schema.properties.punchline]
description = "The punchline of the joke."

[[messages]]
user.text = "{{joke}}"
```

### 10. Add Integration Test (tests/cli.rs)

```rust
#[test]
#[ignore = "Needs LiteLLM running"]
fn test_chat_skip_processing_and_passthrough_litellm() {
    use std::process::{Command, Stdio};
    use serde_json::Value;

    let output = cmd()
        .env("OPENAI_API_KEY", LITELLM_API_KEY)
        .env("OPENAI_API_BASE", LITELLM_API_BASE)
        .arg("chat")
        .arg("tests/fixtures/skip_and_passthrough/input.csv")
        .arg("--prompt")
        .arg("tests/fixtures/skip_and_passthrough/prompt.toml")
        .arg("--model")
        .arg(LITELLM_CHEAP_MODELS[0])
        .stdout(Stdio::piped())
        .output()
        .expect("Failed to execute command");

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
```

## Critical Implementation Details

1. **Map Type**: Use `serde_json::Map<String, Value>` (same as JsonObject throughout codebase)
2. **Memory Management**: Extract `skip_processing` and `passthrough_data` BEFORE clearing template_bindings
3. **No Prompt Rendering**: Early return happens before `state.prompt.render()` call
4. **All Code Paths**: Thread passthrough_data through all success/error branches
5. **NOT Flattened**: `passthrough_data` remains as a nested object in output under the `passthrough_data` key
6. **Backwards Compatibility**: Both fields optional with defaults
7. **OCR Not Affected**: OCR pipeline continues using Ok/Incomplete/Failed as before
8. **CSV Parsing**: For CSV input, passthrough_data column contains JSON string (e.g., `"{"key":"value"}"`) which serde_json automatically parses into a Map

## Files Modified

- `src/queues/work.rs` - WorkStatus enum and counter logic
- `src/queues/chat.rs` - ChatInput, ChatOutput, run_chat, from_resolved_result, empty_for_error
- `schemas/ChatInput.json` - regenerated
- `schemas/ChatOutput.json` - regenerated
- `tests/fixtures/skip_and_passthrough/input.csv` - new
- `tests/fixtures/skip_and_passthrough/prompt.toml` - new
- `tests/cli.rs` - new test function

## Example Input/Output

**Input (CSV)**:
```csv
id,skip_processing,passthrough_data,joke
skip1,true,"{""custom"":""data""}",Skipped joke
normal,false,"{""tag"":""test""}",Why did the chicken cross the road?
```

**Output (JSONL)**:
```jsonl
{"id":"skip1","status":"skipped","errors":[],"passthrough_data":{"custom":"data"}}
{"id":"normal","status":"ok","errors":[],"response":{"punchline":"To get to the other side!"},"passthrough_data":{"tag":"test"},"estimated_cost":0.00001,"token_usage":{"prompt_tokens":50,"completion_tokens":10}}
```

Note how `passthrough_data` appears as a nested object in the output, not flattened to the top level.
