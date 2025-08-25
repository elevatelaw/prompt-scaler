# Useful commands

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