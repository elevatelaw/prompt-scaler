# LiteLLM Driver Replacement Plan

## Context

We want to eliminate the LiteLLM Docker proxy (`docker-compose.yml`) from
our development and production workflow. LiteLLM currently serves two
purposes:

1. **Cost estimation metadata** — per-token pricing via `/model/info`
   (handled separately, not in this plan).
2. **OpenAI-compatible gateway** — translates OpenAI-format requests into
   provider-native API calls for non-OpenAI providers (primarily Anthropic
   Claude via `ANTHROPIC_API_KEY`).

This plan covers (2): making all currently-supported providers work without
LiteLLM.

## Current driver coverage (without LiteLLM)

| Provider | Driver | Structured JSON method | Status |
|----------|--------|----------------------|--------|
| OpenAI | `openai` (`async-openai`) | `response_format: json_schema` | Works |
| Google Gemini | `native` (`genai` 0.2.3) | `ChatResponseFormat::JsonSpec` | Works |
| Google Vertex AI | `vertex` (GCP SDK) | `response_mime_type: application/json` | Works |
| AWS Bedrock (Claude) | `bedrock` (AWS SDK) | Tool-calling (`report_result`) | Works |
| Ollama | `openai` (speaks OpenAI) | `response_format: json_schema` | Works |
| **Anthropic direct API** | **None** | — | **Gap** |

The only provider that requires LiteLLM is **Anthropic's direct Messages
API** (i.e., using `ANTHROPIC_API_KEY` without Bedrock). We already have
Claude via Bedrock, so this is about adding a more direct path.

## Approach 1: Upgrade `genai` crate (preferred starting point)

The `native` driver uses the `genai` crate, currently pinned at **0.2.3**.
Test comments note that Claude "does not return JSON" via this driver.

However, `genai` has since added Anthropic JSON schema support. The
changelog at <https://github.com/jeremychone/rust-genai> shows:

- **v0.6.0-beta** — "anthropic - add JSON schema support"
- v0.5.2 — "Anthropic - Add separate reasoning content"
- v0.5.1 — Tool call handling and prompt caching fixes for Anthropic

### What would need to change

1. **Bump `genai` in `Cargo.toml`** — from `0.2.3` to latest (0.6.x as of
   April 2026). This is a major version jump and the API has likely changed
   significantly.

2. **Update `src/drivers/native.rs`** — The current code uses:
   - `genai::chat::{ChatMessage, ChatOptions, ChatRequest, ChatResponseFormat, ChatRole, ContentPart, ImageSource, JsonSpec, MessageContent, Usage}`
   - `genai::webc` for error handling
   - `Client::default()`, `client.resolve_service_target()`, `client.exec_chat()`

   These types and methods may have been renamed or restructured in 0.6.x.
   The driver is ~237 lines; expect moderate refactoring.

3. **Update error handling** — `IsKnownTransient` impls for `genai::Error`
   and `genai::webc::Error` (lines 144-173) reference specific error
   variants that may have changed.

4. **Enable Claude in tests** — Uncomment `claude-3-5-haiku-20241022` in
   `NATIVE_CHEAP_MODELS` (`tests/cli.rs:39`) and verify structured output
   works.

5. **Add `ANTHROPIC_API_KEY`** — Currently not in `.env`. Would need to be
   added for the native driver to auto-detect Anthropic as a provider.

### Risks

- **Beta status** — genai 0.6.x may still be in beta. Evaluate stability
  before depending on it in production.
- **Breaking API changes** — 0.2 -> 0.6 is a large jump. Imports, types,
  and method signatures will likely differ.
- **Unverified claim** — The changelog says "add JSON schema support" for
  Anthropic but we haven't verified whether this maps to Anthropic's GA
  `output_config.format` parameter or uses tool-calling internally.
- **genai is a third-party abstraction** — We lose visibility into exactly
  what API calls are being made. Debugging provider-specific issues is
  harder.

### Effort estimate

- Low if the genai API hasn't changed much: ~1-2 hours.
- Medium if significant refactoring needed: ~half a day.
- Start with `cargo update -p genai` and see what breaks.

## Approach 2: Dedicated Anthropic driver

Write a new `src/drivers/anthropic.rs` that talks directly to Anthropic's
Messages API.

### Anthropic API status (as of early 2026)

Anthropic's Messages API now has **GA structured output support**:

- Parameter: `output_config.format` (not the older beta `output_format`)
- Supports Claude 4.5+ models (Sonnet, Opus, Haiku)
- Full JSON Schema validation with constrained decoding
- No beta headers required

This means a dedicated driver could use native structured output directly —
no tool-calling trick needed (unlike our Bedrock driver).

### What would need to be built

1. **New file: `src/drivers/anthropic.rs`** (~400-500 lines), following the
   pattern of `bedrock.rs`:
   - Client initialization from `ANTHROPIC_API_KEY`
   - `ChatPrompt<Rendered>` -> Anthropic Messages format conversion
   - JSON schema via `output_config.format` (or tool-calling fallback)
   - Token usage extraction
   - `IsKnownTransient` error classification

2. **Rust client crate** — No official Anthropic Rust SDK exists.
   Community options (as of April 2026):
   - `anthropic-sdk-rust` — Most complete, builder pattern
   - `async-anthropic` — Lightweight
   - `anthropic-ai-sdk` — Less maintained
   - Raw `reqwest` — Maximum control, more boilerplate

3. **Wire into `DriverType`** — Add `Anthropic` variant to
   `src/drivers/mod.rs`, update `create_driver()`.

4. **Reference: Bedrock's tool-calling pattern** — `bedrock.rs` forces
   Claude to call a `report_result` tool with JSON matching the schema
   (`ToolChoice::Any`). This pattern works but is less clean than native
   structured output. It could serve as a fallback if `output_config.format`
   doesn't work for some models.

### Risks

- Community SDK crates may have gaps or lag behind API changes.
- More code to write and maintain (~500 lines vs ~15 for genai upgrade).
- Need to handle Anthropic-specific quirks (message format differs
  significantly from OpenAI).

### Effort estimate

- ~4-6 hours for initial implementation.
- Additional time for edge cases and testing.

## Recommendation

**Try Approach 1 first.** Bump `genai`, fix compilation, test with Claude.
If it works, we get Anthropic support with minimal code. If genai's
Anthropic support is unreliable or the API churn is too painful, fall back
to Approach 2.

Either way, Bedrock already provides working Claude access with existing AWS
credentials, so this is about adding convenience (direct API key) rather
than filling a critical gap.

## Other LiteLLM removal tasks (not covered here)

Once cost estimation is handled separately and direct Anthropic support
works:

- Remove `LiteLlmModel` parameter from `Driver::chat_completion()` trait
  method and all implementations.
- Delete `src/litellm.rs`.
- Remove `docker-compose.yml` and `litellm_config.yml` (or repurpose for
  other uses).
- Update `src/drivers/openai.rs` store-parameter logic (the
  `starts_with("claude-")` fallback already handles the non-LiteLLM case).
- Update module-level doc comments that reference LiteLLM.
