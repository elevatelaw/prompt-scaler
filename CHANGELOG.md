# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
