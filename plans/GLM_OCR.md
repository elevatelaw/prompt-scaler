# GLM-OCR Support

> **Status**: Implemented.

## Problem

GLM-OCR is a highly specialized model served via an OpenAI-compatible `/v1/chat/completions` endpoint. Its OCR mode uses a fixed prompt (`"Text Recognition:"`) with a single attached image, and returns raw text (no JSON structure, no schema). This doesn't fit the existing `ChatPrompt` → `Driver::chat_completion()` → JSON schema validation pipeline.

We need a new code path for raw-text LLM OCR that:
- Bypasses prompt templating, JSON schema, and schema validation
- Still gets PDF splitting, page-level concurrency, timeouts, rate limiting, memory limits, and retry logic
- Reuses the existing `genai` client for OpenAI-compatible API calls

## Architecture

### New trait: `RawDriver` (implemented by `NativeDriver`)

A minimal driver trait for single-image, raw-text completion requests:

```rust
#[async_trait]
pub trait RawDriver: Send + Sync + 'static {
    async fn raw_completion(
        &self,
        model: &str,
        text: &str,          // fixed prompt text (e.g. "Text Recognition:")
        image: &ImageFile,   // page image
        llm_opts: &LlmOpts,
        mem_limiter: &MemLimiter,
    ) -> Result<RawCompletionResponse, DriverError>;
}

pub struct RawCompletionResponse {
    pub text: String,
    pub token_usage: Option<TokenUsage>,
}
```

This is intentionally narrow — no `ChatPrompt`, no `Schema`, no `ChatRequest` construction. The engine caller assembles the message.

### Implementation: `impl RawDriver for NativeDriver`

Rather than creating a new driver type, implement `RawDriver` directly on the existing `NativeDriver`. It already owns the `genai::Client` with `OPENAI_API_BASE` / `OPENAI_API_KEY` routing via `ServiceTargetResolver`. The `raw_completion` impl:
- Constructs a simple `ChatRequest` with one user message (text + image)
- Omit `ChatResponseFormat::JsonSpec` — gets raw text
- Calls `chat_res.first_text()` instead of JSON parsing
- Honors `--llm-timeout`, `--temperature`, `--max-completion-tokens`, `--top-p` via `ChatOptions`
- Reuses `IsKnownTransient` impl for `genai::Error` for retry classification

This lets GLM-OCR be routed through any OpenAI-compatible gateway (vLLM, LiteLLM, Ollama) just by setting env vars, and shares the same client construction and auth logic as the existing structured-chat path.

### New engine: `RawDriverOcrPageEngine`

An `OcrPageEngine` implementation that wraps a `Box<dyn RawDriver>` and a fixed prompt string:

```rust
pub struct RawDriverOcrPageEngine {
    driver: Box<dyn RawDriver>,
    prompt_text: String,
    rate_limiter: RateLimiter,
    mem_limiter: MemLimiter,
}
```

The `ocr_page()` impl, acquiring permits in the same order as Textract:
1. Acquire rate limit permit (`rate_limiter.acquire_one().await`)
2. Load image (`image.load(ImageEncoding::Base64, &mem_limiter).await`) — acquires mem_limiter
3. Call `driver.raw_completion(&model, &prompt_text, &image, ...)`
4. Return `OcrPageOutput { text: Some(response.text), token_usage: response.token_usage, ..default() }`

The rate limiter is acquired before the mem_limiter, matching the Textract order. The `rate_limiter.acquire_one()` is a blocking wait (not a held permit), so there's no risk of circular wait — but consistent ordering across engines eliminates any deadlock surface if the rate limiter implementation changes. No JSON parsing, no `OcrAnalysis` extraction, no schema validation.

### Engine routing

In `engines/mod.rs::ocr_engine_for_model()`, match on `ocr_opts.model` with a case-insensitive substring check:

```rust
if ocr_opts.model.to_lowercase().contains("glm-ocr") => {
    let prompt_text = GLM_OCR_PROMPT; // "Text Recognition:"
    split_pages(RawDriverOcrPageEngine::new(
        concurrency_limit,
        prompt_text,
        ocr_opts,
    ).await?)
}
```

The substring match (rather than exact model name) accommodates variants like `glm-4.5-ocr`, `glm-ocr-2`, etc. without code changes.

## Shared infrastructure factoring

### Move `create_rate_limiter` to `engines/page.rs`

The helper currently lives in `engines/textract.rs`. Both Textract and GLM-OCR (and any future non-chat `OcrPageEngine`) need it. Move it to `page.rs` alongside the `OcrPageEngine` trait:

```rust
// In engines/page.rs
pub fn create_rate_limiter(concurrency_limit: usize, llm_opts: &LlmOpts) -> RateLimiter {
    let rate_limit = llm_opts.rate_limit.clone()
        .unwrap_or_else(|| RateLimit::new(concurrency_limit, RateLimitPeriod::Second));
    rate_limit.to_rate_limiter()
}
```

Update `textract.rs` to import from `super::page` instead of defining it inline.

### Controls inherited from `SplitPagesOcrEngine`

The `RawDriverOcrPageEngine` gets these for free by being wrapped in `SplitPagesOcrEngine`:

| Control | Source |
|---------|--------|
| `--jobs N` (in-flight page limit) | `SplitPagesOcrEngine::buffered(concurrency_limit)` |
| `--page-timeout` | `SplitPagesOcrEngine::with_timeout()` per page |
| `--doc-timeout` | `ocr_file()::with_timeout()` per document |
| `--page-memory-limit` | `RawDriverOcrPageEngine::mem_limiter` (same as Textract) |

### Controls handled inside `RawDriverOcrPageEngine`

| Control | Source |
|---------|--------|
| `--rate-limit` | `rate_limiter.acquire_one().await` before each API call |
| `--llm-timeout` | Applied inside `GenaiRawDriver::raw_completion()` (same as `NativeDriver`) |
| `--temperature` / `--max-completion-tokens` / `--top-p` | Passed via `llm_opts` to `ChatOptions` |

### Controls NOT applicable

| Control | Reason |
|---------|--------|
| `--prompt` / prompt file | GLM-OCR uses a fixed hardcoded prompt |
| `response_schema` | Output is raw text, no schema |
| `--driver` (bedrock/vertex) | GLM-OCR is OpenAI-compatible only; routed via `OPENAI_API_BASE` |

## Files affected

### New files

(No new files.)

### Modified files

- `src/drivers/mod.rs` — Add `RawDriver` trait and `RawCompletionResponse` type
- `src/drivers/native.rs` — Add `impl RawDriver for NativeDriver`
- `src/queues/ocr/engines/page.rs` — Move `create_rate_limiter` here (from `textract.rs`)
- `src/queues/ocr/engines/raw.rs` — `RawDriverOcrPageEngine` implementation
- `src/queues/ocr/engines/mod.rs` — Export `raw` module; add `glm-ocr` routing in `ocr_engine_for_model()`
- `src/queues/ocr/engines/textract.rs` — Import `create_rate_limiter` from `super::page` instead of defining inline

### No changes needed

- `src/cmd/ocr.rs` — No new CLI flags; all controls (`--jobs`, `--rate-limit`, `--page-timeout`, `--doc-timeout`, `--page-memory-limit`, `--llm-timeout`) already exist
- `src/queues/ocr/engines/split_pages.rs` — Works with any `OcrPageEngine`
- `src/queues/ocr/mod.rs` — Engine routing is opaque

## Future work (deferred)

- **Information-extraction mode**: GLM-OCR supports a semi-fixed prompt with JSON structure (e.g. `"请按下列JSON格式输出图中信息: { ... }"`). This could be supported by a `StructuredRawDriver` that takes a JSON schema but still uses a simple prompt string. Defer until there's demand.
- **Batch image support**: GLM-OCR is single-image. If multi-image support is needed, the `RawDriver` trait would need to expand to accept `Vec<ImageFile>`.
