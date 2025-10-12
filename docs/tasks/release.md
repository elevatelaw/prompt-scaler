# Release Process

Our version numbers follow [Semantic Versioning](https://semver.org/), with the common Rust extension for pre-1.0 versions (e.g. `0.MAJOR.MINOR_OR_PATCH`).

## Before releasing

1. Run `just check` to make sure everything is OK.
2. Make sure all changes have been committed.

## Preparing a draft release

1. Look at `Cargo.toml` to get the current version number.
2. List all commits since the last release (`git log vX.Y.Z..HEAD`).
3. For each commit, ask:
   - Is this a user-visible change to the CLI tool, _relative to the last release?_
   - If so, we want to mention it.
   - Otherwise, ignore it.
4. Does this change break backwards compatibility, _relative to the last release?_
   - If so, this is a "MAJOR" version change. Update the version number to `0.(X+1).0`.
   - If not, this is a "MINOR" or "PATCH" version change. Update the version number to `0.X.(Y+1)`.
5. Prepare a draft CHANGELOG.md entry with the existing format.
6. Update `Cargo.toml` with the new version number, and run `cargo check` to update `Cargo.lock`.
7. STOP, and give the user a chance to review the `CHANGELOG.md` entry.

DO NOT CONTINUE WITHOUT USER APPROVAL: All commits require two people to examine the release notes, or a one person and a coding agent.

## Making the release

1. Commit the changes to `Cargo.toml`, `Cargo.lock`, and `CHANGELOG.md`.
2. Tag the commit: `git tag v0.X.Y`.
3. Push the commit and tag: `git push && git push --tags`.
