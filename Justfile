# This is a Justfile, a file that describes tasks and how to run them.
#
# It's basically like `make`, except in Rust and more focused on running tasks
# than on compiling things. And it's a lot cleaner with less ancient Unix
# weirdness.
#
# Run `just --list` to see all the tasks in this file.

# Export our JSON Schema from Python to JSON.
update-schema:
    uv run tests/fixtures/schema.py