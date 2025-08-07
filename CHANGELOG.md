# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
