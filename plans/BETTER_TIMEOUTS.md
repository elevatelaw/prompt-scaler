# Better Timeouts

> **Status**: Implemented.

## Problem

The `--timeout` flag exists but has gaps:

- Only OpenAI and Native drivers call `apply_timeout`; Bedrock and Vertex silently ignore it.
- Non-LLM OCR engines (tesseract, pdftotext, textract) have no timeout support.
- There is no document-level timeout for OCR — a corrupt or huge PDF can block a batch indefinitely.
- `LlmError` and `apply_timeout` are coupled to `LlmOpts`/driver internals, making reuse elsewhere awkward.

## Solution

Three independent timeout flags, each applied at exactly one level:  `--llm-timeout` (replacing the current `--timeout`, with an alias for the old name) wraps individual LLM API calls in the chat queue (with retries), `--page-timeout` wraps per-page OCR processing (graceful failure, document continues), and `--doc-timeout` wraps entire document OCR (hard failure, batch continues). A new `DriverError` type replaces the old `LlmRetryResult` with semantic error kinds and explicit transience, and a reusable `WithTimeout` extension trait in `src/timeouts.rs` provides the timeout machinery for all three levels.

## Three timeout flags

| Flag | Scope | On timeout... | Applies to |
|------|-------|--------------|------------|
| `--llm-timeout` | Per LLM request | Retry (transient error) | All LLM drivers via chat queue |
| `--page-timeout` | Per OCR page | Graceful page failure; doc continues as `Incomplete` | All page-based OCR engines via `SplitPagesOcrEngine` |
| `--doc-timeout` | Per OCR document | Hard doc failure; batch continues | All OCR engines via `ocr_file()` |

No two timeouts are stacked on the same call. Each applies at exactly one level.

## Timeout flow summary

```
ocr_file()                          ← --doc-timeout wraps here
  └─ SplitPagesOcrEngine::ocr_file()
       └─ for each page:
            └─ engine.ocr_page()    ← --page-timeout wraps here
                 └─ (LLM path) chat_handle.process_blocking()
                      └─ run_chat_inner()
                           └─ driver.chat_completion()  ← --llm-timeout wraps here (with retries)

  └─ PdfToTextOcrFileEngine::ocr_file()   (no per-page timeout; doc-timeout only)
  └─ TextractOcrFileEngine::ocr_file()    (no per-page timeout; doc-timeout only)
```

## Implementation

### Commit 1: Introduce `DriverError`, move timeout to chat queue

Replace the `LlmRetryResult` return type on `Driver::chat_completion` with `Result<ChatCompletionResponse, DriverError>`. This lets drivers use standard `Result` combinators instead of non-composable retry macros.

`DriverError` carries a semantic `DriverErrorKind` (`InvalidInput`, `Api`, `PolicyRejection`, `InvalidOutput`) and an explicit `is_transient` flag. Transience is never the default — plain `?` produces fatal errors; transient paths require deliberate construction via named constructors like `api()`, `policy_rejection()`, or `invalid_output_transient()`.

Timeout logic moves out of individual drivers into a reusable `WithTimeout` extension trait in `src/timeouts.rs`, and is applied once in `run_chat_inner()` in `src/queues/chat.rs`, wrapping the `driver.chat_completion()` call. This covers all current and future LLM drivers.

### Commit 2: Rename `--timeout` to `--llm-timeout`

Rename the flag to `--llm-timeout` (keeping `--timeout` as a clap alias) to disambiguate from the new OCR timeout flags.

### Commit 3: Add OCR timeouts, consolidate engine construction

Add `--page-timeout` and `--doc-timeout` CLI options on `OcrOpts`. Also add `TimeoutResultExt` trait with `flatten_timeout_err` and `recover_timeout` for ergonomic timeout error handling.

Consolidate OCR engine construction around `Arc<OcrOpts>` instead of passing individual fields. All engine constructors now take `Arc<OcrOpts>` by value.

## Key types

- **`TimeoutError<E>`** (`src/timeouts.rs`): Wrapper enum (`Native(E)` / `Timeout`) for any error type.
- **`WithTimeout` trait** (`src/timeouts.rs`): Extension trait adding `.with_timeout(Option<Duration>)` to any fallible future.
- **`TimeoutResultExt` trait** (`src/timeouts.rs`): Extension methods on `Result<T, TimeoutError<E>>` — `flatten_timeout_err` converts timeout to a concrete error, `recover_timeout` converts timeout to an `Ok` value.
- **`DriverError`** (`src/drivers/mod.rs`): Semantic error type with `DriverErrorKind` and explicit `is_transient` flag.

