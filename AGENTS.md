# `prompt-scaler` Overview

`prompt-scaler` is a high-volume production LLM client tool designed to run large numbers of requests in parallel and get structured JSON results.

Subcommands:

- `prompt-scaler chat`: Run "chat completion" queries with text and/or image input and structured output.
- `prompt-scaler ocr`: A dedicated OCR mode for PDFs and images. Uses either the same internals as `chat` or dedicated OCR engines
- `prompt-scaler schema`: Print input and output formats as JSON schemas.

## Prompt format

`chat` and `ocr` can take a `--prompt` argument with a prompt template. This is normally a TOML file that will be turned into JSON when read.

- To generated correctly-formed prompts, see `schemas/ChatPrompt.json` and use the JSON Schema to guide your TOML generation.
- For working examples, see `tests/fixtures/**/*prompt*.toml`.

BE CAREFUL! The `[response_schema]` part of prompt files uses an _abbreviated_ version of JSON Schema.

## Source tree

- `benchmarks/`: Various performance benchmarks. Usually not relevant.
- `scripts/`: Rarely-used support scripts.
- `src/`: Main Rust source code.
    - `async_utils/`: Tokio and async stream support code.
    - `cmd/`: Command-line interface. Uses `clap` to parse arguments.
    - `drivers/`: LLM client drivers.
    - `queues/`: Work queues of various types.
        - `work.rs`: Core work queue interface.
        - `chat.rs`: Chat completion work queue.
        - `ocr/`: OCR work queue drivers (not all LLM-based).
    - `prompt.rs`: Main chat prompt types.
    - `schema.rs`: Schema format for constraining structured output.
    - `page_iter.rs`: Splitting PDFs into individual pages.
    - `rate_limit.rs`: Enforcing rate limits to avoid hitting API quotas.
    - `prelude.rs`: Imports that we want to make available everywhere.
- `tests/`: Integration tests.
    - `fixtures/`: Supporting data for tests.
    - `cli.rs`: Integration tests. All significant features SHOULD have a test here.
- `schemas/`: Our data types.
- `Justfile`: Extra maintainer commands.
- `deny.toml`: License and policy file for `cargo deny`.

## Basic theory

- CSV or JSONL input.
- JSONL output. (Post-process using DuckDB SQL for other formats.)
- Async stream from input to output, using a stream of futures plus `.buffered` or `.buffer_unordered` to control concurrency (and thus RAM use, API use, etc).
    - In some cases, we also use `src/queues/work.rs` to run a worker pool.
- Production-hardening: Retries, distinguishing between fatal and transient errors, rate limits, backpressure on all work queues.

## Useful commands

Making sure code is correct:

- `cargo check`: Check syntax, quickly. Use after a set of changes.
- `cargo test`: Run fast tests. Use after `cargo check` passes.
- `cargo test -- --include-ignored`: Run expensive tests. Use only if requested by user.
- `just check`: Run pre-commit checks. Use before committing.

Getting docs:

- `cargo run -- --help`: Shows available subcommands
- `cargo run chat -- --help`: Shows `chat` subcommand options 
- `cargo run ocr -- --help`: Shows `ocr` subcommand options

Rarely used:

- `just update-test-schemas`: Use after editing `tests/fixtures/external_schemas/`. Regenerates JSON schemas for tests.
- `just update-pydantic-models`: Use after changing Rust input/output types. Regenerates Python and TypeScript bindings.

To get more debug information, you can set the following before running `prompt-scaler` code:

- `RUST_LOG`: Set to `prompt_scaler=debug,warn` or `prompt_scaler=trace,warn` for detailed logging

## Environment Configuration

You should **never** need to set up or directly access any of the LLM or other API credentials. These will **always** be provided in a way that `prompt-scaler` detects automatically. If you encounter credential-related errors, immediately stop and ask the user to help.

## Rust coding style

All code will be run through `cargo fmt` to enforce style.

### Error-handling

We use `anyhow::Error` and `anyhow::Result`.  Our `prelude` module automatically includes `anyhow::Result` as `Result`, replacing Rust's standard `Result`. Instead of writing `Result<T, anyhow::Error>`, you should write `Result<T>`.

### Avoiding `unwrap` and `expect`

IMPORTANT: Never use `unwrap` or `expect` for regular error-handling.

You may use `expect` or `unwrap` ONLY to:

- Represent "can't happen" behavior that indicates a programmer mistake, not a user or runtime error.
- Inside unit tests.

### Logging

We use `tracing`. You may use `debug!` and `trace!`. Use `#[instrument(level = ...)]` for all functions that call external network services or CLI commands, with a level of `"trace"` or `"debug"`.

### Philosophy

We strongly encourage correctness.

Avoid using `as` when there's a better alternative. Always use `TYPE::from` or `TYPE::try_from` to convert numeric types.

## Release process

The release process involves the following steps:

1. Make sure all code changes are properly committed. If not, abort and ask the user what to do. **Release commits never contain code changes!**
2. Run `just check` to ensure that the codebase is in a good state.
2. Look at the version number in `Cargo.toml` and `CHANGELOG.md`, and list the commits since the last release.
3. Run `cargo check` to make sure `Cargo.lock` is up to date.
3. Summarize the user-visible changes in `CHANGELOG.md`, using [Keep a Changelog](https://keepachangelog.com/en/1.0.0/) format. Only user-visible changes should be listed!
4. Decide on a new version number, following [Semantic Versioning](https://semver.org/) (for user-visible changes only). We use the Rust convention of `0.MAJOR.MINOR` for pre-1.0.0 releases. For pre-1.0 `PATCH` releases, we increment `MINOR`.
5. Show your proposed CHANGELOG.md changes to the user, and ask for permission to execute the plan.
6. ONLY AFTER APPROVAL: Run `git tag vX.Y.Z && git push && git push --tags`, where `X.Y.Z` is the new version number.
