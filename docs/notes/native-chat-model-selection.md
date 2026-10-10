# Notes: how `genai` picks a wire protocol

Research notes (2026-10-09) on how the `genai` crate (pinned at **0.5.3** in
`Cargo.toml`) decides which LLM REST dialect to speak for a given model name,
and what that implies for our `NativeDriver` in
`src/drivers/native.rs`. Read from the vendored crate source at
`~/.cargo/registry/src/index.crates.io-*/genai-0.5.3/`; all `file:line`
references below are relative to that directory.

## TL;DR

The protocol is chosen **lexically from the model-name string**, once, before
any HTTP happens. `genai` never inspects the endpoint URL, never sniffs a
response, and never consults a live `/models` list. The decision is opaque
afterwards — it lives on as `AdapterKind` inside `ModelIden` and drives a giant
`match` in `AdapterDispatcher`.

## The resolution pipeline

A call to `Client::exec_chat` (`src/client/client_impl.rs:51`) runs four
distinct stages:

### 1. Model name → `AdapterKind`

`Client::default_model` (`src/client/client_impl.rs:29`) is literally
`AdapterKind::from_model(model_name)?` plus `ModelIden::new`. That function
(`src/adapter/adapter_kind.rs:172`) is the whole routing brain:

- **Explicit namespace first.** The name is split on the first `::`
  (`ModelName::split_as_namespace_and_name`, `src/common/model_name.rs`). If the
  prefix matches `AdapterKind::from_lower_str` — `openai`, `openai_resp`,
  `gemini`, `anthropic`, `fireworks`, `together`, `groq`, `mimo`, `nebius`,
  `xai`, `deepseek`, `zai`, `bigmodel`, `cohere`, `ollama` — that wins outright.
  Plus one hardcoded exception, `zai-coding` (`adapter_kind.rs:223`), which maps
  to `Zai` but with a different base URL.
- **Otherwise a prefix / exact-membership cascade** (order matters):

  | Match on name | `AdapterKind` |
  | --- | --- |
  | `gpt*` containing `codex` or `pro` | `OpenAIResp` |
  | `o1*` `o3*` `o4*` `chatgpt*` `codex*` `gpt*` (except `gpt-oss*`) `text-embedding*` | `OpenAI` |
  | `gemini*` | `Gemini` |
  | `claude*` | `Anthropic` |
  | *contains* `fireworks` | `Fireworks` |
  | exact hit in `groq::MODELS` | `Groq` |
  | exact hit in `mimo::MODELS` | `Mimo` |
  | `command*`, `embed-*` | `Cohere` |
  | exact hit in `deepseek::MODELS` | `DeepSeek` |
  | `grok*` | `Xai` |
  | `glm*` | `Zai` |
  | **anything else** | **`Ollama`** |

- **The fallback is `Ollama`, not an error.** The doc comment says so outright:
  "Note: At this point, this will never fail as the fallback is the Ollama
  adapter." `Ollama` means OpenAI-compatible protocol against
  `http://localhost:11434/v1/` with a dummy key `"ollama"`.
- Only three of those `MODELS` lists (`groq`, `mimo`, `deepseek`) participate in
  detection. The others (`xai`, `zai`, `nebius`, `openai`, `gemini`, `cohere`,
  `anthropic`, …) exist solely to back `Client::all_model_names` and are
  hand-maintained snapshots — several are literally empty (`fireworks`,
  `together`, `bigmodel`). Nothing is ever fetched from a provider during
  resolution, so a new model ID on Groq is invisible to detection until the crate
  is updated.

### 2. Derive auth + endpoint from the kind

`ClientConfig::resolve_service_target` (`src/client/config.rs:105`):

1. apply `ModelMapper` if configured (we don't set one);
2. `AdapterDispatcher::default_auth(kind)` (`src/adapter/dispatcher.rs:53`) —
   normally `AuthData::FromEnv(("<PROVIDER>_API_KEY"))`, resolved **lazily** at
   payload-build time, so a missing key surfaces as `ApiKeyEnvNotFound` from
   inside `to_web_request_data`, not at client construction;
3. `AdapterDispatcher::default_endpoint(kind)` (`dispatcher.rs:33`) — each
   adapter's hardcoded `BASE_URL` constant, e.g. `https://api.openai.com/v1/`,
   `https://api.anthropic.com/v1/`,
   `https://generativelanguage.googleapis.com/v1beta/`,
   `https://api.cohere.com/v1/`, `https://api.z.ai/api/paas/v4/`.

### 3. `ServiceTargetResolver` — our hook, runs last

`src/resolver/service_target_resolver.rs`. Gets the fully-formed
`ServiceTarget { endpoint, auth, model }` and has final say over all three
fields. This is the only supported override point, and it is why
`src/drivers/native.rs` must rewrite `model` to
`ModelIden::new(AdapterKind::OpenAI, ...)` and not merely swap the URL — the
kind derived in stage 1 is what actually picks the body format and headers.

### 4. Static dispatch to bytes

`AdapterDispatcher` (`src/adapter/dispatcher.rs`) is ~15 `match
model.adapter_kind` blocks over the `Adapter` trait
(`src/adapter/adapter_types.rs`), producing `WebRequestData { url, headers,
payload }`. `ServiceType` (`Chat` / `ChatStream` / `Embed`) is an orthogonal
input that only chooses the path suffix and `stream` flag.

## The actual wire dialects

There are only about five distinct protocols; most "adapters" are OpenAI-compat
with cosmetic differences (they call
`OpenAIAdapter::util_to_web_request_data`, sometimes with a
`ToWebRequestCustom` tweak).

| `AdapterKind` | Path | Auth | Dialect |
| --- | --- | --- | --- |
| `OpenAI` | `{base}chat/completions` | `Authorization: Bearer` | Chat Completions |
| `OpenAIResp` | `{base}responses` | `Authorization: Bearer` | Responses API |
| `Anthropic` | `{base}messages` | `x-api-key` + `anthropic-version: 2023-06-01` + `anthropic-beta: effort-2025-11-24` | Messages API |
| `Gemini` | `{base}models/{name}:generateContent` (`:streamGenerateContent`) | `x-goog-api-key` | Gemini native (thinking budget, `responseSchema`) |
| `Cohere` | `{base}chat` | `Authorization: Bearer` | Cohere native |
| `Ollama` | `{base}chat/completions` | dummy key `"ollama"` | OpenAI-compat shim |
| `Fireworks`, `Together`, `Groq`, `Mimo`, `Nebius`, `Xai`, `DeepSeek`, `Zai`, `BigModel` | `{base}chat/completions` | `Authorization: Bearer` | OpenAI-compat |

Verified against each adapter's own `get_service_url` / `to_web_request_data`.
`Zai`, `BigModel`, `Fireworks`, `Together`, `Nebius` all build
`{base}chat/completions` by hand or via `OpenAIAdapter::util_get_service_url`;
`Fireworks` is the one adapter passing `Some(ToWebRequestCustom)`
(`adapters/fireworks/adapter_impl.rs:73`). `BigModel` and `Nebius` are reachable
*only* through their namespaces (`bigmodel::`, `nebius::`) because the name
cascade has no pattern that matches their model IDs.

Namespaces are stripped before hitting the wire — every
`to_web_request_data` destructures `namespace_and_name()` into `(_, model_name)`,
so `openai::gpt-4.1` sends `"model": "gpt-4.1"`.

## What does *not* influence the choice

- **The endpoint host.** No URL/authority sniffing anywhere in `src/` (grepping
  `host_str` / `authority` / `starts_with("http` → nothing). Pointing
  `AdapterKind::Anthropic` at an OpenAI-compatible gateway yields a POST to
  `/v1/messages` with `x-api-key`, which the gateway will reject.
- **Anything the server says.** Detection is not adaptive; a wrong guess is not
  corrected by a failed request.

## Consequences for `src/drivers/native.rs`

1. **`ChatResponseFormat::JsonSpec` is silently dropped on some backends.** Our
   driver passes `JsonSpec` unconditionally (`native.rs`, in `ChatOptions`). It is
   consumed only in `openai/adapter_impl.rs:270`,
   `openai_resp/adapter_impl.rs:134`, and `gemini/adapter_impl.rs:186`. Grepping
   `response_format` in `adapters/anthropic/` and `adapters/cohere/` returns
   **nothing** — so `--driver native --model claude-*` (or `command-*`) sends no
   schema at all and structured output rests entirely on the prompt text. The
   Bedrock and Vertex drivers take `schema` as their own argument and handle it
   independently, so this is a native-path-only hazard. TODO: either route
   Claude through `vertex`/`bedrock`, or detect the ignored-schema case and
   return an error rather than trusting the model to obey.
2. **Unrecognized model names fail slowly and misleadingly.** Without
   `OPENAI_API_BASE`, a typo or an `org/model` HF-style ID falls through to
   `Ollama` and produces a connection-refused `Error::WebModelCall`. Our
   `IsKnownTransient for genai::Error` impl (`native.rs`) does match
   `WebModelCall { webc_error }` → `webc::Error::Reqwest(error)`, so whether it
   retries depends entirely on reqwest's own transient classification; the user
   sees a localhost error for a model they never asked to run locally.
3. **The resolver closure runs twice per request.** `native.rs` calls
   `self.client.resolve_service_target(model)` purely to log the adapter kind,
   and `exec_chat` resolves again internally. Harmless while our closure is a
   pure read of `OPENAI_API_BASE`, but any stateful or nondeterministic resolver
   would log a different kind than the one actually used.
4. **Forcing OpenAI-compat requires rewriting `model`, not just `endpoint`.**
   Already handled correctly, but easy to regress — see stage 3 above.
