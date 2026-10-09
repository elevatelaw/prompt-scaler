# Notes: "System One" / decision-model APIs

Research notes (2026-10-08) on Jev-style decision models and their APIs, for possible
`prompt-scaler` support. Verified empirically against our llama.cpp router
(b11485, `localhost:8080`) plus the upstream docs listed at the bottom.

## Resources

- llama.cpp decision-model support (announcement, model table, tips):
  <https://huggingface.co/blog/ggml-org/decision-models-in-llamacpp>
- First-party open decision models (Julia-1, Laya, Kev, lev, OpenJev, Clef):
  <https://huggingface.co/collections/ggml-org/decision-models>
- Decision Index — benchmarks of open and proprietary decision models:
  <https://multimodalart-jev-decision-index.static.hf.space/index.html>
- OpenJev Multimodal API spec (the de-facto reference for multimodal System
  One; llama.cpp's README cites it directly): <https://jev-skills.github.io/openjev-multimodal/api>
  — also machine-readable at `GET /openapi.json` on any conforming server

## The category

Decision models answer *typed questions* about content instead of generating text:
one forward pass over the state, answers read from next-token probabilities of
single-token labels. Answers are always one of the supplied options, with
probabilities and a TypeSafe-style confidence `c = (n·p_max − 1)/(n − 1)`
(0 = uniform guessing, 1 = unanimous; NOT a calibrated probability).

Jev (TypeSafe) started the category; llama.cpp 0.6.0+ (Oct 2026) made it an
open ecosystem with `/v1/systemone`. Known open models (median 1-question
latency, RTX PRO 6000): Julia-1 144M/3ms, Laya 421M/5ms, **Kev-4B 12ms**,
lev 36ms, OpenJev/Clef 27B (vision) ~43ms.

Request semantics — the thing to get straight, because "batching" is used for
three different things:

1. **One `state` per request.** Universal (Jev, llama.cpp, Pi). There is NO
   multi-state batch endpoint anywhere. N items = N concurrent requests
   (llama.cpp continuous batching handles the GPU side; our buffered streams
   are the natural fit).
2. **N questions per request** — native everywhere and cheap. Kev-4B/lev/OpenJev
   process the state once regardless of question count. Put ALL questions for a
   document in one call. (Exception: `clef` reads all questions jointly.)
3. **N items packed inside one structured `state`** (backtick field references,
   choice options as item ids — see Jev's "line-by-line search" cookbook).
   Legal on the wire, but small models cross-contaminate; don't rely on it
   without evals.
   - We've tried this and breaks pretty frequently with many models. Not
     worth designing around.

## llama.cpp `/v1/systemone` (open, self-hosted)

Wire format follows the TypeSafe API: request
`{model?, state, questions: {id: {type, instructions, criteria}}}`, response
`{model, answers: {id: {...}}, usage}`. `model` is optional/ignored with one
model loaded; router mode uses it to route (and auto-load).

- Question types: `choice` (options map, ≤52–255 per model), `score` (2–10
  ordered levels → weighted index), `noul` (yes/no → P(yes)). `criteria`
  descriptions matter a lot for small models.
- Errors: `400` invalid request (helpful messages), `501` *not a decision
  model* — **chat models cannot use this endpoint**; no server-side emulation.
- Decision models are flagged in `/v1/models` via
  `architecture.output_modalities: ["decisions"]`, read from GGUF metadata
  without loading. (Pi's classifier catalog only lists them once loaded, as an
  observed quirk.)
- Probabilities are scaled by temperature baked into the GGUF and are "not
  guaranteed calibrated for your data" — calibrate per-model cutoffs on our
  own data. (Client-side knob in Pi: per-request `temperature` divides label
  logits before normalizing; changes confidences, never argmax.)

### Image support (exists!)

Contrary to first impression: llama.cpp extends the text-only TypeSafe spec
with multimodal input, per the OpenJev multimodal spec:

- `files` (or alias `images`): array of data URLs (`data:image/...`, audio for
  lfm2-d1-omni); images are placed before the state in the prompt.
- `state` as chat messages with `image_url` parts (data URLs only), same shape
  as chat completions — handy if we already emit chat-format content.
- Needs a vision-capable decision model (openjev, clef, pplx-decider,
  lfm2-d1, lfm2-d1-omni) plus multimodal projector (`--mmproj`, auto-downloads
  for `-hf` repos); otherwise `501`.
- OpenJev is CC BY-NC; **Clef is Apache 2.0** and is the commercial-safe 27B
  vision decision model (note: clef answers questions jointly, and its prompt
  must fit `--ubatch-size`; clef-only servers disable text generation).

### The OpenJev Multimodal API spec (provenance + deltas)

What llama.cpp implements is the **OpenJev Multimodal API**
(<https://jev-skills.github.io/openjev-multimodal/api>): an independent
community project (jev-skills org, a local Mac server over llama.cpp;
not affiliated with TypeSafe, created 2026-09-22, little standalone traction so
far). Its significance is that **llama.cpp's README cites it as the reference
for the multimodal extension**, and ggml-org's OpenJev 27B shares the name —
so the shape has an influential adopter even if the spec repo itself is small.

Beyond multimodal, the spec differs from TypeSafe's docs in ways we should not
stumble over:

- **Confidence formula**: `1 − H(p)/log n` (normalized-entropy concentration)
  — NOT TypeSafe's documented `(n·p_max − 1)/(n − 1)`. Both are 0-at-uniform /
  1-at-delta, but values differ; Pi uses the TypeSafe formula. Treat
  `confidence` as opaque per-dialect; don't mix thresholds across servers.
- **Limits**: 1–64 questions; choice 2–255; score **2–64 levels** (vs.
  TypeSafe/llama.cpp docs' 2–10).
- **`usage.output_tokens` = number of questions** (vs. llama.cpp's always-0,
  vs. Jev's real billed counts).
- Response `timing` object + `x-openjev-*` / `Server-Timing` headers
  (server-side latency breakdown, also on errors); disableable via
  `OPENJEV_RESPONSE_TIMING=false` for strict-field clients — a nice pattern.
- Discovery: `GET /v1/limits` (admission limits), `/health`, and a real
  **`/openapi.json`** — worth diffing against our serde types per server.
- Error envelope is minimal `{"error": {"message": ...}}` with
  401/413/422/502/503/504/529 (`Retry-After` on 529) — a third error dialect
  (vs. Jev's 401/422/429/529 and llama.cpp's OpenAI-style envelope).

### TypeScript schema (basis for Rust serde types)

Provenance tags: **[JEV]** = directly from TypeSafe's published API (the
portable core); **[LCPP]** = llama.cpp-specific extension or deviation, which a
hosted Jev endpoint may not accept; **[LCPP?]** = likely shared but
unverified on Jev.

```ts
// ---------- Request ----------
type JsonValue = string | number | boolean | null | JsonValue[] | { [k: string]: JsonValue };

interface SystemOneRequest {
  model?: string;                  // [JEV] required on Jev ("jev-latest" or a version);
                                   // [LCPP] optional here — router dispatch, ignored when a
                                   //        single model is loaded
  state: string | JsonValue;       // [JEV] non-string values are given to the model as JSON text
                                   // [LCPP] may additionally be chat messages with image_url parts
  files?: string[];                // [LCPP] data URLs (image/*, audio/* for lfm2-d1-omni); Jev is text-only
  images?: string[];               // [LCPP] alias of `files`; both may be used together
  questions: Record<string, SystemOneQuestion>; // [JEV] ids echoed back; not sent to the model
}

type SystemOneQuestion =
  // [JEV] ≤255 options — but llama.cpp caps per model (e.g. 52 for openjev), so use the lower bound
  | { type: "choice";  instructions: string | JsonValue; criteria: Record<string, string | null> }
  // [JEV] 2–10 levels, lowest first; criteria values implicitly count 0, 1, 2...
  | { type: "score";   instructions: string | JsonValue; criteria: string[] }
  // [JEV] criteria optional on both; note Jev's noul criteria are typed true/false descriptions
  | { type: "noul";    instructions: string | JsonValue; criteria?: { true?: string; false?: string } };
// [JEV] `instructions` may be a string, object, or array on both APIs; llama.cpp accepts
//       JsonValue here and elsewhere without Jev's backtick field-reference semantics documented

// ---------- Response ----------
interface SystemOneResponse {
  model: string;                   // [JEV] both return it; Jev gives a resolved version
                                   //       (e.g. "jev-1.13.0"), llama.cpp the alias/quant tag
  answers: Record<string, SystemOneAnswer>;      // [JEV] keyed by the request's question ids
  usage: { input_tokens: number; output_tokens: 0 }; // [JEV] shape; [LCPP] output always 0
}

type SystemOneAnswer =
  | { type: "choice"; choice: string; probabilities: Record<string, number>; confidence: number }  // [JEV]
  | { type: "score";  score: number;  // [JEV] expected value of 0-based index, may fall between levels
      legend: Record<string, string>;         // [JEV] level index (as string) -> level description
      probabilities: Record<string, number>;  // [JEV] level index (as string) -> P(level)
      confidence: number }                    // [JEV] Formula varies
  | { type: "noul";   noul: number };           // [JEV] P(true); no confidence field

// ---------- Error ----------
// [LCPP] OpenAI-style envelope, codes 400 (invalid request) / 501 (not a decision
//        model, or images with no mmproj). Jev instead uses 401/422/429/529 with
//        its own body shape and expects client-side exponential backoff.
interface SystemOneError {
  error: { code: number; message: string; type: "invalid_request_error" | "not_supported_error" };
}
```

**Dialect note for our types:** the portable Jev core is
`{state, questions}` → `{answers, usage}` with the three question types and
answer shapes above — everything else (`model` field, `files`/`images`,
chat-message states, the error envelope, and the model-specific option/ubatch
caps) is a llama.cpp dialect layer. Worth modeling as a shared core type plus
an optional extensions block rather than one union, so a Jev-compatible
request is expressible without lying about the wire shape.

## OpenAI Decisions API (gpt-6-luna) — different API, same category

Public beta; `POST /v1/decisions`, model `gpt-6-luna`. Same idea, envelope
differs from System One:

- Request: `{model, input, questions: [...]}` — questions are an **array of
  named objects** (`name` echoed in answers), not an id-keyed map.
  `input` is a string OR user messages with `input_text` + `input_image`
  (data URL) parts → images natively supported.
- Question types: `predicate` (≈ noul; **no criteria field** — true/false
  meanings must be folded into `instructions`), `choice` (`choices:
  [{value, description}]`), `score` (`levels: [{label, description}]`).
- Answers: array of `{type, name, ...}`; `predicate` → `probability`;
  `choice`/`score` → `choice`/`score` + `probabilities` as an **array of
  `{value, label, probability}`** (System One uses maps) + `confidence`.
  Extra answer type: `{"type": "refusal", "name": ...}` — moderation can
  refuse a question, must be handled as a first-class outcome.
- Limits/ops (from Pi's docs + client constants; reverify against live API):
  rejects >922K input tokens; gateway times out ~5s in (~>600K tokens);
  Pi caps at 128 images/request; API keys only — ChatGPT sign-in/OAuth tokens
  are rejected on this route.
- Pricing: **$0.10/M input tokens only** — no output, cache-read, or cache-write
  charges (output_tokens concept doesn't exist). ZDR + HIPAA eligible;
  US/EU data residency. Advertised ~10× faster than Responses API.

### TypeScript schema (basis for Rust serde types)

Same tags as above; everything here is OpenAI's own design, so [JEV] marks
only where it happens to match the Jev core.

```ts
// ---------- Request ----------
interface DecisionsRequest {
  model: "gpt-6-luna";
  input: string | DecisionsInputMessage[];   // string OR user messages (images require messages)
  questions: DecisionsQuestion[];            // ARRAY of named questions, not a map
}

interface DecisionsInputMessage {
  role: "user";                              // only user messages documented
  content: Array<{ type: "input_text"; text: string } | { type: "input_image"; image_url: string }>;
                                             // image_url: data URL only
}

type DecisionsQuestion =
  | { type: "predicate"; name: string; instructions: string }                              // no criteria field!
  | { type: "choice";    name: string; instructions: string; choices: Array<{ value: string; description?: string }> }
  | { type: "score";     name: string; instructions: string; levels:  Array<{ label: string; description?: string }> };

// ---------- Response ----------
interface DecisionsResponse {
  answers: DecisionsAnswer[];                // array; match back by `name`
  usage?: { input_tokens: number; output_tokens?: number }; // input-only billing
}

type DecisionsAnswer =
  | { type: "predicate"; name: string; probability: number }                                // singular field, no confidence
  | { type: "choice";    name: string; choice: string;
      probabilities: Array<{ value: string; probability: number }>                           // string-valued, no label
      confidence: number }
  | { type: "score";     name: string; score: number;   // expected level index, may fall between levels
      probabilities: Array<{ value: number; label: string; probability: number }>            // numeric value + label
      confidence: number }
  | { type: "refusal";   name: string };     // moderation refused this question; still billed

// ---------- Error (standard OpenAI envelope) ----------
interface DecisionsError {
  error: { message: string; type: string; param: string | null; code: string | null };
}
```

Serde notes: `predicate.probability` is a bare field (vs. choice/score
`probabilities` arrays); choice probabilities are `string`-keyed while score
probabilities are `number`-keyed with a separate `label` — two distinct variant
structs, don't share one generic type. Answers are `deny_unknown_fields`-risky
in beta; tolerate unknown variants by mapping to a catch-all.

## Shape for prompt-scaler (sketch)

- New driver kind alongside chat drivers: one request per input row
  (state = row/JSONL record or rendered text), all prompt-template questions in
  the same call; images (PDF pages!) ride along for vision-capable models.
- Two transports to implement: `systemone` (llama.cpp, incl. router `model`
  field) and `openai-decisions` (Luna). Mapping is mechanical
  (`bool`↔`noul`/`predicate`, map↔array question envelopes, refusal → fatal
  per-question error or a null answer in JSONL).
- Structured output for decision rows differs from chat: answers are
  `{choice, probabilities, confidence}` / `{score, confidence}` / `{probability}`
  — worth emitting per-question in the JSONL, plus confidence passthrough so
  post-processing (DuckDB) can threshold.
- Calibration caveat applies doubly: probabilities are per-model, not portable;
  any thresholds belong in post-processing, not the driver.

## Rust client crates (glanced, 2026-10-08)

Roughly 30 System One client crates exist on crates.io, most a few weeks old
(judgment's own [field survey](https://github.com/chussenot/judgment/blob/main/docs/project/research/client-comparison.md)
reads the top nine from source, with dependency footprints). The two worth
knowing:

- **`kunobi-decision`** (0.3.0, Oct 3; renamed from `kunobi-jev`; Apache-2.0,
  ~550 dl) — an unofficial port of TypeSafe's JavaScript SDK. Multi-provider
  (TypeSafe, Cloudflare Workers AI, Liquid, Vercel, OpenRouter, plus
  `Provider::Compatible` + base URL for "any System One wire"). Tokio-based,
  optional `blocking` client, solid retry/timeout story (`total_timeout`
  bounds retries, `Retry-After` honored, per-attempt credential providers,
  secret hygiene), and a good forward-compat pattern: `.extra()` passthrough
  for unknown request/question fields and `Answer::Unknown` keeping unmapped
  JSON intact. `labels!` macro maps choice options to enums; `AnswerKey`
  handles make `result.answer(&key)` type-correct. `FakeSystemOne` for tests.
- **`judgment`** (0.11.1, Oct 8; MIT, ~150 dl but moving fast — pin the minor)
  — more of a decision-workflow toolkit than a bare client: `.jud` rubric
  files + a `jud` CLI (`triage`/`record`/`eval`/`tune`, threshold tuning from
  recorded answers, accuracy gates for CI, keyless replay), plus a Rust API
  with `options!` enums, typed handles, and every answer verified against the
  question before it can be read. Backends: hosted TypeSafe, Ollama (≥0.35
  serves decision models, e.g. `tev1` 0.8B CPU), laya-serve, Cloudflare, any
  base-URL server. Heavy emphasis on validating probabilities and stated
  backend differences.

### Coverage vs. what we need

| Capability | kunobi-decision | judgment | Notes |
|---|---|---|---|
| Jev core wire (`state`/`questions` → `answers`/`usage`) | ✓ | ✓ | both faithful; both check ≤255 / 2–10 client-side |
| llama.cpp router (`model` per request, base URL) | ◐ `Compatible` provider + `request.model()` | ◐ base URL + model arg | should work; neither models llama.cpp explicitly |
| llama.cpp `files`/`images` multimodal extension | ✗ | ✗ | **the gap** — state is text/JSON only in both |
| OpenAI Decisions (`/v1/decisions`, Luna, refusals) | ✗ | ✗ | neither speaks it; System One only |
| Confidence gates | ✓ `decide(threshold)` | ✓ `confidence.at_least()` | same idea both |
| Retries / budgets / jitter | ✓ detailed | ✓ | kunobi's `total_timeout`-bounds-retries is the nicest design |
| Forward compat (unknown fields/answers) | ✓ `extra()` / `Answer::Unknown` | ◐ verifies vs. questions | copy kunobi's pattern for our serde |
| Structured per-question answers as data (bulk/JSONL-friendly) | ◐ typed handles, per-call | ✓/◐ | neither is built for N-row bulk pipelines; that's our work-queue's job |
| Testability without key | ✓ `FakeSystemOne` | ✓ `Fake` + record/replay | judgment's replay/eval loop matches our eval needs |

**Decision (2026-10-08): roll our own.** The wire is small (two envelopes,
§schemas above), no crate supports multimodal or Luna, and the resilience/rate-
limit/pacer machinery these crates compete on is already solved in
prompt-scaler (`rate_limit.rs`, the work queues) — we reuse ours, not theirs.
From these crates we take ideas only: kunobi's `extra()`/`Answer::Unknown`
forward-compat serde pattern; judgment's answer-vs-question verification.
(Ecosystem note: Ollama ≥0.35 also serves decision models on this wire; we are
not interested in Ollama as a target, usual reasons.)

**Outlook:** image input is the direction of travel (llama.cpp already ships
it; Luna has it) but is far less standardized than the question/answer core —
three incompatible shapes today (llama.cpp `files`/`images` data-URL arrays,
llama.cpp chat-message `image_url` states, OpenAI `input_image` message
parts). Expect per-dialect adapters; keep our core type free of image fields
so the text-only path stays wire-exact everywhere (see dialect note above).

### Remaining second-look candidates (NOT yet investigated)

No crate speaks OpenAI Decisions, and no crate checks distribution arithmetic
(sum≈1, choice==argmax) — open territory for our wrapper. Rate limiting,
throttles, and pacer designs (jev-client's shared throttle,
typesafeai-sdk-community's `Retry-After` pacer) are **deprioritized**: we
reuse prompt-scaler's own rate limiting. What still has residual value:

1. **`typesafe-client` 0.1.0** — the only other crate (with judgment) that
   verifies every response against the questions sent (ids, kinds, offered
   options); strict-decode tradeoff (unknown answer kinds hard-error, unlike
   kunobi's keep-intact approach) — worth reading both before we lock our
   serde enums. OpenAPI contract tests as a testing pattern.
2. **`typesafeai-sdk-community` 0.5.0** — eval metrics only (Brier, ECE, AUC,
   F1), relevant to the per-model probability-calibration problem; skip its
   pacer.
3. **`typesafe-sdk` 0.2.0** — cautionary only (leaks API keys via `Debug`,
   re-sends key + state on redirects); listed so nobody re-recommends it
   based on its download count.

## Appendix: Pi's chat-model fallback (we will NOT support this)

Older llama.cpp had no decisions concept, so Pi emulates a classifier on *any*
chat GGUF (`llama-cpp-classify` API) purely client-side, via three native
(non-OpenAI) server endpoints, once per question:

1. `POST /tokenize` — resolve each answer label to a token id (tokenizes
   `"\n"+label`; errors on label/token collisions).
2. `POST /apply-template` — server-side chat template,
   `chat_template_kwargs: {enable_thinking: false}`; appends `</think>` if
   the rendered prompt opens a thinking block.
3. `POST /completion` — `{prompt, n_predict: 1, n_probs: depth,
   post_sampling_probs: false, cache_prompt: true, temperature: 0}`; reads
   `completion_probabilities[0].top_logprobs[] = {id, logprob}`, looks up label
   token ids, softmaxes just those (pre-sampling probs = full-vocab logits,
   sampler-independent). Depth starts at `max(256, 16·#labels)` and escalates
   if a label falls outside the top-N.

Prompt scheme per question: `[state, overview of ALL questions, state again,
this question with labeled options]` — state is intentionally duplicated so the
second copy is read with the questions in view (measured accuracy gain on
JevBench with small models); the shared prefix relies on `cache_prompt` so it's
processed once per request. Labels: `A–Za–z0–9` (choice), `Yes`/`No` (bool),
digits (score).

We already have plenty of decision models; this emulation path has fiddly
failure modes (label misses, multi-token labels, thinking-forcing templates)
and is deliberately out of scope.

## Sources

- <https://huggingface.co/blog/ggml-org/decision-models-in-llamacpp> (Oct 2 2026 announcement; model table, tips)
- llama.cpp `tools/server/README.md`, "POST /v1/systemone" (master; PR #29818)
- TypeSafe API: <https://docs.typesafe.ai/api>, <https://docs.typesafe.ai/concepts/state>
- OpenAI Decisions: <https://developers.openai.com/api/docs/guides/decisions>
- Pi: `docs/llama-cpp.md#classification`, `docs/models.md#use-classifier-models`,
  `pi-ai/dist/api/{system-one-shared,llama-cpp-classify,openai-decisions}.js`
