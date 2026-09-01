# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Security

- Picked up upstream fixes for RUSTSEC-2026-0190 (`anyhow` unsoundness in `Error::downcast_mut`) and RUSTSEC-2026-0258 (`h2` unbounded empty DATA frames) via dependency updates, and moved off the yanked `chacha20` 0.10.0. Lockfile only, no code changes.

## [0.3.3] - 2026-04-24

### Fixed

- vertex: Fixed `GOOGLE_CLOUD_LOCATION` handling, and provided `GOOGLE_CLOUD_PROJECT` as the standard name for `GCP_PROJECT` (which we still accept).

## [0.3.2] - 2026-04-23

### Added

- Added a `--reasoning-effort` flag, which is used by Vertex and native drivers (but not Bedrock, which ignores it). This helps considerably with some more recent models, where thinking does not help significantly with OCR.

### Fixed

- Changed the `ocr --model` default from `gemini-2.5-flash` to `gemini-2.5-flash-lite`, which is a better OCR default: Cheaper, more throughput, roughly the same quality. This is arguably a breaking change, but the current default is brand new and not actually that good.

## [0.3.1] - 2026-04-23

### Fixed

- Fixed PDF password support for multiple OCR drivers, and verified that OCR drivers logged error messages when encountering password-protected PDFs. The errors for `textract-async` just that processing failed (without an explanation of why), but all the other errors are clear enough.

## [0.3.0] - 2026-04-21

### Added

- OCR: Multipage TIFF support, including CMYK and 8/16-bit color variants. Pages are converted to PNG for LLM consumption; ambiguous SubIFD content is reported as an error rather than silently dropped.
- OCR: `--page-timeout` and `--doc-timeout` options for per-page and per-document timeouts (complementing the existing per-request LLM timeout).
- `--page-memory-limit` option (e.g. `2GiB`, `500MB`) providing explicit backpressure on in-flight image loading. Prevents high-concurrency OCR jobs from materializing thousands of page images in RAM at once.
- `--llm-timeout` as the new name for the previous `--timeout` flag. The old name still works as an alias.
- `--model-cost-data=PATH` option to override the built-in per-token pricing CSV.
- `--base-dir` option controlling how relative paths in input files and prompt templates (`image-data-url`, `text-file-contents`) are resolved. Defaults to the directory of the input file, or the current working directory when reading from stdin.

### Changed

- MAJOR: Paths in input files and prompt templates are now resolved relative to `--base-dir` (defaulting to the input file's directory), not relative to the current working directory. Existing inputs that relied on CWD-relative paths need to be updated or passed with an explicit `--base-dir`.
- The default driver is now `native` instead of `openai`. `--driver=openai` remains accepted as a silent alias for `--driver=native`, so existing scripts keep working.
- `claude-*` and `gemini-*` models now route to Anthropic and Gemini directly by default. These used to require either setting `--driver=native`, or using the old default `--driver=openai` and setting `OPENAI_API_BASE` to point at a gateway like LiteLLM. Users who relied on the old default (routing Claude/Gemini via LiteLLM) must either continue to set `OPENAI_API_BASE` (which still forces every model through the OpenAI-compatible gateway at that URL) or provide `ANTHROPIC_API_KEY` / `GEMINI_API_KEY` for direct access.
- `chat --model` no longer defaults to `gpt-4o-mini`; the flag is now required. (OCR keeps its existing default.)
- Model cost estimation now uses an embedded CSV of per-token pricing (covering OpenAI, Anthropic direct + Bedrock IDs, and Google Gemini for both AI Studio and Vertex) instead of querying LiteLLM's `/model/info` endpoint at runtime.
- The `page_data_url` variable and `image-data-url` prompt helper now produce `file:` URLs instead of `data:` URLs. This is transparent to normal use — drivers load images with their preferred encoding at the last moment.
- Content-filter / RECITATION responses from OpenAI-compatible gateways now surface as `invalid_output` errors, matching how the Vertex and Bedrock drivers already handled the same condition (previously they mapped to a dedicated retryable `PolicyRejection` kind).

### Removed

- LiteLLM-based cost estimation has been removed in favor of a built-in cost CSV.
- We no longer assume a LiteLLM gateway as our default way of talking to models, though setting `OPENAI_API_BASE` will enable this as before. 
- The `example_input_data_url` and `example_output` variables are no longer exposed in OCR prompts; a new sample custom prompt demonstrates the replacement pattern.
- Manually constructed `data:` URLs in a prompt's `user.images` field are no longer accepted. Use the `image-data-url` helper or a plain file path instead.

### Security

- Picked up upstream fixes for RUSTSEC-2026-0007 (`bytes`), RUSTSEC-2026-0049 and RUSTSEC-2026-0099 (`rustls-webpki`), and RUSTSEC-2026-0097 (`rand`) via dependency updates.

## [0.2.20] - 2026-01-22

### Fixed

- bedrock: Fix image uploads failing with "Could not process image" error. The Bedrock API expects decoded bytes, but the driver was passing base64-encoded strings directly.

## [0.2.19] - 2026-01-21

### Fixed

- bedrock: Fix image uploads failing with "unknown enum variant: 'image/jpeg'" error. The AWS SDK's ImageFormat parser expects format names like "jpeg", not full MIME types.

## [0.2.18] - 2025-10-12

### Added

- `echo` driver: New driver for testing programs that wrap prompt-scaler without needing real LLM API access. The driver echoes back the last user message in a structured format.

## [0.2.17] - 2025-10-11

### Added

- Inputs may contain `"skip_processing": true` to skip a specific input record. This is useful for records which will have empty input strings (which cannot be processed by some LLMs). These records will be marked as `"status": "skipped"` in the output.
- Inputs may also contain `"passthrough_data"`, which may contain JSON data that will be copied to the output record as-is. This is useful for associating metadata with input records when the input and output processes are not strictly synchronized.
- Add a `to-string` helper for prompts.

### Fixed

- Pass Vertex system message as system message, not user message.

## [0.2.16] - 2025-09-08

### Added

- textract-async: New OCR driver which operates on S3 URLs instead of file names.
- vertex: New Gemini driver which uses the Vertex APIs instead of the API Studio ones. Both modes are planned to exist going forward.
- aws: Updated from v2025_01_17 credential loading policy to v2025_08_07. This should theoretically add support for `HTTP_PROXY`, `HTTPS_PROXY`, and `NO_PROXY`, but this has not been specifically tested.

## [0.2.15] - 2025-08-07

### Fixed

- clippy: Update to latest Rust compiler and fix warnings.

## [0.2.14] - 2025-08-07

### Fixed

- bedrock: Update our driver to always force tool use, because we were seeing surprisingly high rates of invalid JSON output using the text-based conversational interface and a JSON Schema. Specifically, we were not seeing the kind of numbers AWS saw in these experiments: https://aws.amazon.com/fr/blogs/machine-learning/structured-data-response-with-amazon-bedrock-prompt-engineering-and-tool-use/

## [0.2.13] - 2025-07-28

### Added

- bedrock: Add experimental support for AWS Bedrock. Tested with Claude, but only in a very simple case.

## [0.2.12] - 2025-07-25

### Added

- ocr: Output `page_count`.

### Fixed

- Vastly improved error messages!
- Always output `text` and `error` fields with `null` values, instead of omitting them. This may be technically breaking for certain users, but I don't think any exist.

## [0.2.11] - 2025-06-10

### Fixed

- Fix: Fix "xref num \d+ not found" test so that we actually treat it as a warning, not an error.

## [0.2.10] - 2025-06-10

### Fixed

- PDF: Treat "xref num \d+ not found" as a warning, not an error. This is super common in PDFs, and it shouldn't affect their visual appearance or text extraction, which is what we care about. We do, however, still log this.

## [0.2.9] - 2025-06-09

### Added

- `schema`: Allow passing `--inline-subschemas` for cases where we don't want `$ref`. This is necessary, for example, when talking to many LLMs.
- `ocr`: Allow passing `--include-page-breaks`, which will insert Control-L (Form Feed) characters between pages in the output text. This is useful for scripts that want to keep track of individual pages without doing extra post-processing. 
- Allow using `{{text-file-contents path}}` in prompts to look up the content of external text files, which is common with "loadfile" formats.


### Fixed

- Never send `store: false` to any model named `claude-`, because doing so breaks LiteLLM.
- Fix spelling of "Jaccard" in OCR benchmark code.
- Limit external processes to roughly the number of available CPU cores.
- Try to keep our progress "UI" alive until the very end of the program.

## [0.2.8] - 2025-05-04

### Added

- Added some new example scripts for exporting OCR `text` output to standalone files, and for comparing two different sets of text extractions. As always, these scripts are subject to come and go.

### Fixed

- Fixed `schema ChatPrompt` regression so it actually generates a type named `ChatPrompt` again.
- Fixed `--rate-limit` to always start with full token buckets.

## [0.2.7] - 2025-05-03

### Added

- Allow specifying `--rate-limit` for LLM calls. This also overrides the default rate limit for Tesseract API calls.

## [0.2.6] - 2025-05-03

### Fixed

- Improve error messages for page iteration code.
- ocr: Correctly honor `--allow-reordering`.
- Don't log output of CLI commands if there isn't any.

## [0.2.5] - 2025-05-02

### Added

- It is now possible to pass `--driver=native` to bypass LiteLLM and talk to some LLMs natively. This is handy for large OCR jobs that LiteLLM can't handle without running a LiteLLM cluster. WARNING: The details of this command-line flag will likely change in 0.3.0 soon.

## [0.2.4] - 2025-05-01

### Fixed

- ocr: Don't abort processing early if a document fails during initial preparation. Instead, just mark that document as failed and continue.
- Retry all HTTP errors that do not return an HTTP status code. There are simply too many things that can go wrong, and `reqwest` doesn't provide enough details to be precise. This means the `prompt-scaler` will probably hang nearly forever if you try to connect to a non-existant server, but it should be much more robust on big runs.

## [0.2.3] - 2025-04-30

### Fixed

- Also retry request errors. These seem to mostly be transient errors caused by LiteLLM falling over under heavy load.

## [0.2.2] - 2025-04-30

### Added

- ocr: Capture warnings from PDF tools and include them in the output.
- ocr: Add `--max-pages` option to limit the number of pages to process. Truncted documents will be marked as "partial" in the output.
- chat & ocr: Add `--max-completion-tokens`, `--temperature`, `--top-p` and `--timeout` options. `--max-completion-tokens` and `--timeout` may be useful for runaway responses where you know the output should be short.
- litellm: Added `restart` and RAM limit to example LiteLLM config, for production use.

### Fixed

- Return an error for PDFs where `pdfseparate` prints "PDF Error" on the output. These are often broken in a way that will cause page extraction to fail. Better to flag them as errors and let the user decide what to do with them.

## [0.2.1] - 2025-04-29

### Added

- We support `--offset` and `--limit` options for processing only part of the input.
- `--take-next` is now an alias for a new `--limit` option.
- We support `--allow-reordering` to permit out-of-order output, which should also keep throughput higher in some cases, especially where work item sizes vary greatly.

## [0.2.0] - 2025-04-24

### Added

- ocr: Output results to CSV file.

### Changed

- jsonl: All outuput formats now include `"status": "ok" | "partial" | "failed"` to indicate the result of processing.
- ocr: `failed_page_count` has been removed.
- ocr: `pages` array has been replaced with a single `text` value for now. A new version of `pages` with more detailed information will return.
- Several scripts in `scripts/` have been removed

## [0.1.0] - 2025-04-23

### Added

- Initial release, for internal testing only.
