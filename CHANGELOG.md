# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
