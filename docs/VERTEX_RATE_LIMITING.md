# Vertex AI Gemini rate limiting — field notes

> **Status:** A research report straight from Claude. Confirm the sources if you use it.

## Bottom line

Compared to Gemini 2.0 Flash on Vertex (where `--rate-limit=1800/m`, i.e. 30/s, at `--jobs 300` ran cleanly on Tier 1), **Gemini 2.5 Flash is substantially more restrictive at burst.** A `--jobs 50` run (50 concurrent requests, not 50 RPS) reliably draws RESOURCE_EXHAUSTED / 429 responses even when the project is at <5% of its published TPM allowance. The underlying reason appears to be that 2.5 Flash is newer and contends for a tighter shared pool of model-replica capacity, and Google deliberately obscures the per-second enforcement that actually bites.

## How the published quota works (Standard PayGo Usage Tiers)

Gemini 2.5 Flash is on Google's **Standard PayGo usage-tier** system. There is no fixed per-model RPM/TPM default anymore — your org sits in Tier 1/2/3 based on 30-day spend, and each tier gets a **baseline TPM per model family**:

| Tier | 30-day spend | Flash family baseline TPM |
| --- | --- | --- |
| 1 | $10 – $250 | 2,000,000 |
| 2 | $250 – $2,000 | 4,000,000 |
| 3 | > $2,000 | 10,000,000 |

On top of the tier TPM, there is a system-wide ceiling of **30,000 RPM per model per region** — far above typical usage.

Sources:
- [Standard PayGo usage tiers](https://cloud.google.com/vertex-ai/generative-ai/docs/standard-paygo)
- [Dynamic Shared Quota](https://cloud.google.com/vertex-ai/generative-ai/docs/dynamic-shared-quota)
- [Quotas and limits](https://cloud.google.com/vertex-ai/generative-ai/docs/quotas)

## Moving up tiers

**What spend counts:**
- **Org-level aggregation** — spend is pooled across the entire Cloud Organization, not per-project or per-billing-account. Usage split across multiple orgs or standalone billing accounts won't combine.
- **Vertex AI / Agent Platform SKUs only** — Gemini models (all families, including batch, thinking, tuned), caching, priority tiers, Vertex CPU/GPU/TPU prediction, grounding. General GCP spend (BigQuery, Compute outside Vertex, GCS, etc.) does **not** count.
- **TPM is per model family.** Flash-family and Pro-family spend each buys tier for their own ladder; they don't pool.

**Mechanics:**
- Promotion is automatic once 30-day rolling spend crosses a threshold.
- Check current tier in the **Agent Platform Dashboard** in the Cloud console. Cloud Billing shows aggregated spend.
- No form, no ticket required for the standard path.
- The generic IAM quota increase form at `console.cloud.google.com/iam-admin/quotas` does **not** apply to Gemini generative TPM — those limits are governed by the PayGo tier system, not editable quotas. Don't waste time submitting there.

**Enterprise shortcut:** quoting the doc directly —
> If you require higher throughput for an enterprise use case, contact your account team for more information regarding a custom tier.

This is the doc-sanctioned path to skip the 30-day spend-history wait and/or get allocation beyond Tier 3. For a company with an existing Google account rep, this is the direct route. See `docs/VERTEX_QUOTA_NEEDS.md` for the prepared negotiation note.

**Gotchas:**
- Spend has to actually land in Cloud Billing for auto-promotion to trigger. There's a [forum report](https://discuss.google.dev/t/vertex-ai-standard-paygo-tier-not-recognized-billing-shows-us-0-despite-active-gemini-usage/341527) of Gemini traffic not reflecting in billing, blocking promotion — verify billing numbers match observed usage.
- Preview models (2.5 Flash Image, Imagen 4, Virtual Try-On, etc.) are excluded from the tier system.

## The undocumented part: per-second smoothing on replica slots

Official docs repeatedly say things like:

> Avoid sending requests in sharp, second-level spikes.
> — [standard-paygo](https://cloud.google.com/vertex-ai/generative-ai/docs/standard-paygo)

> Your traffic isn't strictly capped at the Baseline Throughput limit. Agent Platform lets traffic burst beyond this limit on a best-effort basis.
> — [dynamic-shared-quota](https://cloud.google.com/vertex-ai/generative-ai/docs/dynamic-shared-quota)

…but **none of these pages publish a number** for the smoothing window or the per-second cap. The only places in Vertex docs where concrete per-second windows appear are the **Provisioned Throughput** pages, where for context, 1 GSU of Gemini 2.5 Flash = "322,800 tokens per 120-second window (2,690 tokens/sec)". DSQ deliberately does not expose its equivalent. ([Provisioned Throughput windows](https://cloud.google.com/gemini-enterprise-agent-platform/models/deploy/provisioned-throughput))

Best inference on what's actually enforced: the backend maintains a pool of **replica slots** per region per model, shared across all PayGo customers (this is literally what Dynamic *Shared* Quota means). A request consumes one slot for its duration regardless of how many tokens it emits. The TPM tier is a sanity cap on top of that. With 2.5 Flash being newer and having tighter provisioned capacity, 50 concurrent requests is enough to contend with whatever else is running in that region that second and draw a 429 — even at ~5% of the Tier 1 TPM headroom.

Supporting evidence:
- Users report 429s at <1% of their published TPM quota. ([forum 112161](https://discuss.ai.google.dev/t/gemini-api-429-error-despite-low-quota-usage-on-paid-tier-gemini-2-5-flash/112161))
- Reports tie 429 spikes to Google-side backend incidents (2025-12-07, 2026-02-24, 2026-04-21), consistent with a shared capacity pool rather than a per-customer metered bucket. ([forum 90957](https://discuss.ai.google.dev/t/vertex-api-gemini-2-0-flash-returns-429-errors-after-minimal-traffic/90957), [forum 116568](https://discuss.ai.google.dev/t/sudden-spike-in-429-errors-with-gemini-2-5-via-vertex-ai-global-endpoint/116568))
- Google's internal quota metric names (visible on the quota page) include `generate_content_requests_per_minute_per_project_per_base_model` — the system meters requests, not just tokens.

### The 429 error body is deliberately opaque

Google's AIP-193 error model allows 429s to carry structured detail:
- `QuotaFailure.violations[]` — naming which metric tripped
- `RetryInfo.retryDelay` — how long to back off

In practice, Vertex PayGo 429s on Gemini **omit all of these**. Every body posted in the forums / GitHub looks like:

```json
{"code":429,"status":"RESOURCE_EXHAUSTED",
 "message":"Resource exhausted. Please try again later.",
 "errors":[{"reason":"rateLimitExceeded","domain":"global"}]}
```

So even if `prompt-scaler` captured the full gRPC status chain, it couldn't tell TPM from RPM from smoothing-window. There is no programmatic signal to react to beyond "back off and retry."

Sources:
- [Error 429 on Vertex](https://cloud.google.com/vertex-ai/generative-ai/docs/error-code-429)
- [Google AIP-193 (error model spec)](https://google.aip.dev/193)
- [Reduce 429 errors on Vertex AI (GCP blog)](https://cloud.google.com/blog/products/ai-machine-learning/reduce-429-errors-on-vertex-ai)
- [python-genai issue #2001](https://github.com/googleapis/python-genai/issues/2001), [#2065](https://github.com/googleapis/python-genai/issues/2065)
- [adk-python #3404](https://github.com/google/adk-python/discussions/3404)

## Thinking mode is on by default and inflates output tokens ~2.35x

Separate from rate limiting but related to how much each request costs: Gemini 2.5 Flash has extended thinking **enabled by default**, with a dynamic budget of up to 8,192 thinking tokens. Thinking tokens are billed as output tokens. Observed 2.35x output-token inflation on an OCR benchmark vs 2.0 Flash on identical inputs.

2.5 Flash is the only 2.5 model that permits `thinking_budget=0` (full disable). Range is 0–24,576 tokens. ([thinking docs](https://cloud.google.com/vertex-ai/generative-ai/docs/thinking))

Disabling thinking reduces per-request output token count and wall time, which frees replica slots faster — so it does help 429 pressure at the margins — but **it does not give you more slots.** The structural shared-pool issue remains.

## Mitigation menu (Vertex-only, no 24/7 commitment)

Ranked for a bursty workload (minutes of saturation, then idle for hours/days) where AI Studio is **not** an option (data residency constraints):

1. **Vertex Batch Inference for Gemini.** Supports 2.5 Flash; reads/writes GCS JSONL or BigQuery; up to 200,000 requests per job. Google explicitly says **"no predefined quota limits ... large, shared pool of resources, dynamically allocated ... batch requests may be queued for capacity"** — i.e. capacity-bound, not RPM-bound. 50% discount vs interactive (2.5 Flash batch: $0.15 in / $1.25 out). Queue holds up to 72h; target processing within 24h. No SLA. Fits bursty OCR perfectly if async latency is acceptable. ([Vertex batch prediction for Gemini](https://cloud.google.com/vertex-ai/generative-ai/docs/multimodal/batch-prediction-gemini))

2. **Multi-region sharding.** Vertex quotas are "per project **per region** per base_model" — each regional endpoint has its own bucket. The `global` endpoint is a separate bucket *on top*, pitched by Google as "improve overall availability while reducing resource exhausted (429) errors." 2.5 Flash is available in us-central1, us-east1/4/5, us-west1/4, us-south1, EU multi-region, and more. Sharding across 3–4 regions gives 3–4x the synchronous headroom. Architecturally endorsed. Caveat: complicates data residency if you have per-region rules. ([locations](https://cloud.google.com/vertex-ai/generative-ai/docs/learn/locations))

3. **Weekly Provisioned Throughput.** Minimum commit is 1 week (not monthly, contrary to popular belief). Overage automatically bills as PayGo. Still pays for idle baseline all week, so doesn't match "idle for days between bursts" — but worth knowing about if burst frequency increases. ([Provisioned Throughput purchase](https://cloud.google.com/vertex-ai/generative-ai/docs/purchase-provisioned-throughput))

4. **Cross-project sharding.** Each GCP project has its own quota bucket. Not officially endorsed or prohibited for this purpose. GCP has warned against "creating multiple projects to circumvent usage limits" in other products. Gray area — reach for this only after #1 and #2 are tapped.

Not viable:
- **Flex PayGo on Vertex.** Only supports 3.x preview models as of April 2026; no 2.5 Flash. ([flex-paygo](https://cloud.google.com/vertex-ai/generative-ai/docs/flex-paygo))
- **AI Studio as a parallel bucket.** Separate quota pool, but data-residency features lag Vertex — not acceptable for our workload.

## Client-side defensive settings

For interactive traffic on a single region (when batch inference isn't an option):

- Use `--rate-limit=30/s` rather than `--rate-limit=1800/m`. The per-minute form lets `prompt-scaler` emit 1,800 requests in the first second and sit idle the rest of the minute; the per-second form actively smooths. Google's own guidance ("avoid sharp, second-level spikes") is explicitly against the per-minute burst pattern.
- Drop `--jobs` to match: if steady state is 30 RPS and typical latency is 2s, `--jobs 60` is the matching concurrency; set it lower (e.g. 20–30) until the burst shape stabilizes.
- Set `thinking_budget=0` in the OCR prompt — cuts output tokens ~60% and roughly the same in wall time, which reduces how long each request holds a replica slot.
- Consider the `global` endpoint. It's a separate quota bucket, so it helps sometimes, but at least one forum user reported it made no difference. ([forum 116568](https://discuss.ai.google.dev/t/sudden-spike-in-429-errors-with-gemini-2-5-via-vertex-ai-global-endpoint/116568))
- Keep our existing exponential-backoff retry loop; it's the only remediation Google actually endorses in the 429 docs. The `RetryInfo.retryDelay` field that should guide backoff is not populated in practice, so a jittered exponential is as good as we can do.

## Sidebar: Gemini 2.5 Flash-Lite (drive-by observations)

We didn't investigate Flash-Lite in depth, but things noticed in passing:

- **Pricing is identical to Gemini 2.0 Flash** on both AI Studio and Vertex: `$0.10/M` input, `$0.40/M` output (compare: 2.5 Flash is $0.30 / $2.50). Source: `src/default_model_costs.csv` lines 66, 68, 70.
- Flash-Lite is **on the same Standard PayGo usage-tier system** as 2.5 Flash and 2.5 Pro, so the shared-pool issues above apply. ([standard-paygo](https://cloud.google.com/vertex-ai/generative-ai/docs/standard-paygo))
- Forum thread [discuss.ai.google.dev/t/125899](https://discuss.ai.google.dev/t/429s-in-vertex-ai-for-gemini-2-5-flash-lite-in-europe/125899) reports 429s against Flash-Lite in Europe — same shared-pool issues affect it, possibly more acutely in smaller regions.
- Worth benchmarking as a potential drop-in replacement for 2.0 Flash at its price point — if quality holds, it would restore both the cost profile and (unclear) possibly a similarly generous capacity pool that 2.0 Flash enjoyed.
