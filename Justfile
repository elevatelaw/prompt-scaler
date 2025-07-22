# This is a Justfile, a file that describes tasks and how to run them.
#
# It's basically like `make`, except in Rust and more focused on running tasks
# than on compiling things. And it's a lot cleaner with less ancient Unix
# weirdness.
#
# Run `just --list` to see all the tasks in this file.

check:
    cargo fmt -- --check
    cargo deny check
    cargo clippy -- -D warnings
    cargo test --all

# Export our test JSON Schemas from Python and TypeScript to JSON.
update-test-schemas:
    uv run tests/fixtures/external_schemas/schema.py
    npx typescript-json-schema \
        --required --strictNullTypes --noExtraProps \
        -o tests/fixtures/external_schemas/schema_ts.json \
        tests/fixtures/external_schemas/schema.ts ImageInfo

# Export our main JSON Schemas as Pydantic models.
update-pydantic-models:
    mkdir -p scripts/support/models
    for model in ChatInput ChatOutput ChatPrompt OcrInput OcrOutput; do \
        cargo run -- schema $model -o tmp_schema.json; \
        snake_case=$(echo $model | sed 's/\([a-z]\)\([A-Z]\)/\1_\L\2/g' | tr '[:upper:]' '[:lower:]'); \
        uv run datamodel-codegen \
            --input tmp_schema.json \
            --input-file-type jsonschema \
            --output scripts/support/models/$snake_case.py \
            --output-model-type=pydantic_v2.BaseModel \
            --use-annotated \
            --use-subclass-enum; \
    done
    rm tmp_schema.json
