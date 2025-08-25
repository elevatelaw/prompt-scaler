# Rust coding style

All code will be run through `cargo fmt` to enforce style.

## Error-handling

We use `anyhow::Error` and `anyhow::Result`.  Our `prelude` module automatically includes `anyhow::Result` as `Result`, replacing Rust's standard `Result`. Instead of writing `Result<T, anyhow::Error>`, you should write `Result<T>`.

## Avoiding `unwrap` and `expect`

IMPORTANT: Never use `unwrap` or `expect` for regular error-handling.

You may use `expect` or `unwrap` ONLY to:

- Represent "can't happen" behavior that indicates a programmer mistake, not a user or runtime error.
- Inside unit tests.

## Logging

We use `tracing`. You may use `debug!` and `trace!`. Use `#[instrument(level = ...)]` for all functions that call external network services or CLI commands, with a level of `"trace"` or `"debug"`.

## Philosophy

We strongly encourage correctness.
