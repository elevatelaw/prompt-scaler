# `prompt-scaler`: High-volume production LLM client

> **NOTE:** This is a work in progress! It's under very active development and not all features are implemented yet.

`prompt-scaler` is a tool for running large numbers of LLM requests in parallel, and getting JSON results using structured output. This is most useful when:

1. You have a lot of requests to run, but
2. You can't afford to wait for the 24-hour turnaround guarantee for an LLM's batch API. (We _do_ plan to provide integrated support for using the batch API.)

## Example usage

`prompt-scaler` is invoked as a command-line tool:

```sh
prompt-scaler chat tests/fixtures/input.csv \
    --prompt tests/fixtures/prompt.toml \
    --schema tests/fixtures/schema.json \
    --model gpt-4o-mini \
    --out output.json
```

Given the input:

```csv
id,joke
road,Why did the chicken cross the road?
doctor,Why did the chicken go to the doctor?
```

...plus an appropriate prompt and schema, this will produce [JSON Lines](https://jsonlines.org/) (JSONL) output like:

```jsonl
{"id":"road","response":{"punchline":"To get to the other side!"}}
{"id":"doctor","response":{"punchline":"He wasn't feeling very chicken!"}}
```

For example input and output files, see:

- [input.csv](./tests/fixtures/input.csv) or [input.jsonl](tests/fixtures/input.jsonl): Input data in either CSV or JSONL format.
- [prompt.toml](./tests/fixtures/prompt.toml): Example prompt template. Values from the input file will be filled in using [Handlebars](https://handlebarsjs.com/) templates.
- [schema.json](./tests/fixtures/schema.json): A [JSON Schema](https://json-schema.org/) generated from [schema.py](./tests/fixtures/schema.py), specifying the output we want receive. The `description=` fields will be passed to the LLM.

## License

License TBD, probably MIT+Apache 2, like Rust.

Copyright 2025 Elevate.
Some earlier code copyright ???? Eric Kidd.
