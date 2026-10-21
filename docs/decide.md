# The `decide` Subcommand

`decide` asks a **decision model** a fixed set of questions about each input row. It works
differently from `chat`.

A decision model does not write text. It reads your input once, and picks one label for each
question. Because of this, every answer comes back with:

- the chosen label;
- the probability of every possible label;
- a confidence value.

This is faster and cheaper than `chat`. The answers are also always valid, so there is no
"retry until the JSON parses" step.

## Quick Start

```sh
prompt-scaler decide input.jsonl \
    --prompt prompt.json \
    --model ggml-org/Kev-4B-GGUF:Q4_K_M \
    --out output.jsonl
```

If you leave out the input path, `decide` reads standard input. If you leave out `--out`, it
writes to standard output. Working examples live in
[`tests/fixtures/decide/`](../tests/fixtures/decide/).

## Server and Credentials

Right now `decide` supports one driver: `system_one`. It is the default, so you do not need
to name it. This driver sends `POST {url}/v1/systemone`, which llama.cpp and Jev servers
implement.

Two environment variables control the connection. You can set them in the environment, or in
a `.env` file:

| Variable | Required | Default | Meaning |
| --- | --- | --- | --- |
| `SYSTEM_ONE_API_URL` | No | `http://localhost:8080` | Base URL of the server. Do not add `/v1/systemone`. |
| `SYSTEM_ONE_API_KEY` | Only if your server needs a key | Unset | Sent as a Bearer token. |

For a local server, start `llama-server` with a model that can answer decisions:

```sh
llama-server -m ggml-org/Kev-4B-GGUF:Q4_K_M --port 8080
```

Not every model supports decisions. If the model does not, the server returns an error, and
`prompt-scaler` shows the server's own message.

To use a server on another machine, set the URL:

```sh
SYSTEM_ONE_API_URL=http://gpu.example.com:8080 \
    prompt-scaler decide input.jsonl --prompt prompt.json --model kev-4b
```

The model name is part of every request. A server that loads models on demand uses it to
choose the right one.

## The Prompt File

The prompt must be JSON. `decide` does not accept TOML prompts.

```json
{
  "state": "{{text}}",
  "questions": {
    "category": {
      "type": "choice",
      "instructions": "What kind of document is this?",
      "criteria": {
        "invoice": "A bill for goods or services.",
        "memo": "An internal memorandum."
      }
    },
    "urgent": {
      "type": "noul",
      "instructions": "Does this document need action within 24 hours?",
      "criteria": {
        "true": "Payment or reply is due within 24 hours.",
        "false": "Nothing is due within 24 hours."
      }
    },
    "quality": {
      "type": "score",
      "instructions": "How clearly is this document written?",
      "criteria": [
        "Confusing or incomplete.",
        "Understandable but sloppy.",
        "Clear and precise."
      ]
    }
  }
}
```

There are three kinds of question:

| `type` | Answer | `criteria` |
| --- | --- | --- |
| `choice` | One of your labels. | An object: each label maps to a description. Two labels or more. |
| `noul` | `true` or `false`, as a probability. | An object with `"true"` and `"false"` descriptions. |
| `score` | A number from 0 to N−1. It can fall between two labels. | A list of descriptions. The first description is 0, not 1. Two to ten items. |

**Write the criteria.** They describe what each label means. Small models answer much better
with them, and they are easy to skip by accident. Use `null` for a description only when you
must.

`noul` is the name upstream servers use for a true/false question. We kept it so our prompt
files match theirs.

We check the questions when we read the prompt, before we send any request. A question needs
instructions, a `choice` needs at least two labels, and a `score` needs two to ten labels.

### Templates

Template markers like `{{text}}` work in `state` today. We never run the questions through the
template engine, so `{{` inside a question stays plain text. (Images can also carry templates,
but images are not supported yet — see *Not Supported Yet*.)

## `state`: What the Model Reads

Exactly one of these must supply `state`:

1. **`state` in the prompt.** We fill in `{{column}}` markers from the current row. This works
   at any depth inside nested JSON.
2. **`state` in the input row.** We send it exactly as it is. We do not replace anything
   inside it, even if it contains `{{...}}`.

If both sides supply `state`, or neither does, the whole run stops with an error before we
send any request.

Two more points:

- A template value always becomes a string. `{{amount}}` gives `"42"`, not `42`.
- **One row is one request.** Do not put several documents in one `state`. Small models mix
  them up.

## The Input File

Input is CSV or JSONL. Each row can hold:

| Field | Meaning |
| --- | --- |
| `id` | Required. Copied to the output. |
| `state` | Only when the prompt has no `state`. |
| Any other column | A value for templates in the prompt's `state`. Columns you do not use are ignored. |
| `skip_processing` | Optional `true`. We write a `skipped` record and do not call the server. |
| `passthrough_data` | Optional JSON. Copied to the output unchanged. Use it for gold labels. |

```jsonl
{ "id": "invoice", "text": "INVOICE #2026-041 ... Total due: $4,250.00." }
{ "id": "memo", "text": "MEMO From: Facilities ... No action is required." }
```

## Output

`decide` writes one JSONL record per input row. We show this one on several lines to make it
readable. In the file, each record is one line:

```jsonc
{"id":"invoice","status":"ok","errors":[],"token_usage":{"prompt_tokens":278,"completion_tokens":3},
 "answers":{
   "category":{"type":"choice","choice":"invoice","confidence":0.87,
               "probabilities":{"invoice":0.93,"memo":0.07}},
   "urgent":{"type":"noul","noul":0.91},
   "quality":{"type":"score","score":2.4,"confidence":0.60,
              "legend":{"0":"Confusing or incomplete.","1":"Understandable but sloppy.","2":"Clear and precise."},
              "probabilities":{"0":0.02,"1":0.18,"2":0.61,"3":0.19}}}}
```

- `status` is `"ok"`, `"failed"`, or `"skipped"`.
- `errors` lists problems, including temporary ones we recovered from.
- `completion_tokens` is the number of answers. These models write one token per answer, even
  when the server reports zero.
- `estimated_cost` appears only when we have price data. Local models have none, so we report
  no cost.
- `confidence` comes from the server. Its formula is different for every model, so compare it
  only within one model. Use `probabilities` when you want numbers you can compare.

`decide` always writes the probabilities, and it never applies a threshold to them. Set your
thresholds afterwards, for example with DuckDB over the JSONL file. Then you can change your
mind without running the job again.

## Error Handling

- **Problems with your prompt or your input** stop the run at once. Every row in a file has
  the same shape, so one clear error is better than 600,000 failed records.
- **Server problems** are retried with exponential backoff. We retry network errors and HTTP
  429, 502, 503, 504, and 529. Other errors are not retried. The record gets
  `status: "failed"` and the run continues.
- **Answers that do not match the questions** — a missing answer, or a label you never
  offered — end that record without a retry. These models are deterministic: the same input
  gives the same answer, so a retry cannot help.
- `--allowed-failure-rate` (default `0.01`) fails the whole run if too many records fail.

## Other Useful Options

| Option | Meaning |
| --- | --- |
| `-j`, `--jobs` | Requests to run at one time. Default 8. |
| `--rate-limit` | Requests per second or minute, such as `10/s` or `600/m`. |
| `--decide-timeout` | Seconds before we give up on one request. A hosted server may still charge you for a request that timed out. |
| `--limit`, `--offset` | Process only part of the input. |
| `--allow-reordering` | Output may arrive out of order. This can help when some requests are much slower. |
| `--base-dir` | Directory used to resolve relative paths. |

## Not Supported Yet

- **Images.** The prompt may name `images`, but we do not load them yet. The server will not
  see the image. Run `ocr` first, then run `decide` on the text.
- **TOML prompts.** JSON only.
- **Other drivers.** OpenAI's `/v1/decisions` may be added later. Our types are already
  written so that a second driver can be added on its own.
- **Cost estimates for local models.** There is no price data for them, so we report nothing.

## Reference

- `prompt-scaler decide --help` lists every option.
- `prompt-scaler schema DecidePrompt`, `DecideInput`, and `DecideOutput` print the JSON
  Schemas for the prompt, the input row, and the output record.
- `RUST_LOG=prompt_scaler=debug,warn` prints detailed logs.
