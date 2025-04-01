# This is a Justfile, a file that describes tasks and how to run them.
#
# It's basically like `make`, except in Rust and more focused on running tasks
# than on compiling things. And it's a lot cleaner with less ancient Unix
# weirdness.
#
# Run `just --list` to see all the tasks in this file.

# Export our JSON Schemas from Python and TypeScript to JSON.
update-schemas:
    uv run tests/fixtures/external_schemas/schema.py
    npx typescript-json-schema \
        --required --strictNullTypes --noExtraProps \
        -o tests/fixtures/external_schemas/schema_ts.json \
        tests/fixtures/external_schemas/schema.ts ImageInfo
