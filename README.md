# `prompt-scaler`: High-volume production LLM client

> **NOTE:** This is a work in progress! It's under very active development and not all features are implemented yet.

`prompt-scaler` is a tool for running large numbers of LLM requests in parallel, and getting JSON results using structured output. This is most useful when:

1. You have a lot of requests to run, but
2. You can't afford to wait for the 24-hour turnaround guarantee for an LLM's batch API. (We _do_ plan to provide integrated support for using the batch API.)

## Environment

The following variables can be specified in the environment, or using a `.env` file:

- `OPENAI_API_KEY`: API key for OpenAI
- `OPENAI_API_BASE` (optional): Base URL for an alternate implementation of the OpenAI API, for use with tools like LiteLLM or Ollama.
- `RUST_LOG` (optional): Set to `prompt_scaler=debug,warn` or `prompt_scaler=trace,warn` to produce detailed logs. This uses the [`env-logger` syntax](https://docs.rs/env_logger/latest/env_logger/).

## Tested models

We have automated regression tests showing that we can talk to the following models:

| Vendor | Model | Via | Text Input | Image Input | JSON Output |
| --|--|--|--|--|--|
| OpenAI | gpt-4o-mini | Direct, LiteLLM | ✅ | ✅ | ✅ |
| Google | gemini-2.0-flash | LiteLLM | ✅ | ✅ | ✅ |
| Anthropic | claude-3-5-haiku-20241022 | LiteLLM | ✅ | ✅ | ✅ |
| Google (open) | gemma3:4b | Ollama | ✅ | ✅ | ✅ |

We recommend the use [LiteLLM](https://www.litellm.ai/) to talk any API besides OpenAI and Ollama. LiteLLM currently appears to have poor Ollama support, but Ollama's native server endpoint works fine on its own.

## Example usage

`prompt-scaler` is invoked as a command-line tool:

```sh
prompt-scaler chat tests/fixtures/texts/input.csv \
    --prompt tests/fixtures/texts/prompt.toml \
    --model gpt-4o-mini \
    --out output.json
```

Given the input:

```csv
id,joke
road,Why did the chicken cross the road?
doctor,Why did the chicken go to the doctor?
```

...plus an appropriate prompt:

```toml
# Put the actual question in the developer message (aka "system message").
developer = """
Answer the joke with a short, appropriate punchline.
"""

# Define the schema for the response.
[response_schema]
description = "The response to a joke."

[response_schema.properties.punchline]
description = "The punchline of the joke."

# Provide 0 or more example messages and responses.
[[messages]]
user.text = "Why did the scarecrow win an award?"

[[messages]]
assistant.json.punchline = "Because he was outstanding in his field."

[[messages]]
user.text = "I’m reading a book on anti-gravity."

[[messages]]
assistant.json.punchline = "It’s impossible to put down."

# Finally, provide the actual input joke.
[[messages]]
user.text = "{{joke}}"
```

...this will produce [JSON Lines](https://jsonlines.org/) (JSONL) output like:

```jsonl
{"id":"road","response":{"punchline":"To get to the other side!"}}
{"id":"doctor","response":{"punchline":"He wasn't feeling very chicken!"}}
```

For example input files, see:

- [input.csv](./tests/fixtures/texts/input.csv) or [input.jsonl](tests/fixtures/texts/input.jsonl): Input data in either CSV or JSONL format.
- [prompt.toml](./tests/fixtures/texts/prompt.toml): Example prompt template. Values from the input file will be filled in using [Handlebars](https://handlebarsjs.com/) templates.

### Example image usage

Let's say we have three images of various beings holding signs:

<img alt='Turtle holding sign saying "Go!"' src='tests/fixtures/images/turtle.jpg' width="128px"> <img alt='Capybara holding sign saying "HELLO, WORLD!"' src='tests/fixtures/images/capybara.jpg' width="128px"> <img alt='Alien holding sign saying "TAKE US TO YOUR LLMS, PLEASE"' src='tests/fixtures/images/alien.jpg' width="128px">

We'll hold out the turtle for use as an example image, and create a CSV file describing the other two:

```csv
id,path
1,tests/fixtures/images/capybara.jpg
2,tests/fixtures/images/alien.jpg
```

#### Providing a prompt

Now we can define out prompt, using the `image-data-url` helper to include the images:

```toml
# Place the actual instructions in the developer message.
developer = """
Extract the specified information from the supplied images.
"""

# Specify what fields we want the model to respond with.
[response_schema]
description = "Information to extract from each image."

[response_schema.properties.sign_text]
description = "Text appearing on the sign in the image."

[response_schema.properties.sign_holder]
description = "A one-word description of the entity holding the sign."

# We provide an example of what we want, using the turtle image.
#
# Including 1-3 examples will often produce much better output.
[[messages]]
user.images = ["{{image-data-url 'tests/fixtures/images/turtle.jpg'}}"]

# Expected output.
[[messages]]
assistant.json.sign_text = "Go!"
assistant.json.sign_holder = "Turtle"

# The prompt which contains our actual input.
[[messages]]
user.images = ["{{image-data-url path}}"]
```

#### Running the LLM

Finally, we could analyze our images as follows:

```sh
prompt-scaler chat tests/fixtures/images/input.csv \
    --prompt tests/fixtures/images/prompt.toml
```

This will produce the following output:

```jsonl
{"id":"1","response":{"sign_holder":"Capybara","sign_text":"HELLO, WORLD!"}}
{"id":"2","response":{"sign_holder":"Alien","sign_text":"TAKE US TO YOUR LLMS, PLEASE"}}
```

This JSONL output can be easily converted to CSV or another format using Python. If you provide sample input and output, your favorite LLM can probably write the script for you!

### Extracting schemas from Python or TypeScript

See [tests/fixtures/external_schemas](tests/fixtures/external_schemas) and our [Justfile](Justfile) for examples.

## License

Copyright 2025 Elevate. Some earlier code copyright 2024 Eric Kidd, and used with permission.

This software is licensed under either the [Apache License, Version 2.0](./LICENSE-APACHE.txt) or the [MIT License](./LICENSE-MIT.txt), at your option.