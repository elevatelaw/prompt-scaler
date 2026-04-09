# Page Memory Limit

> **Status**: Core types implemented. Full design finalized, ready for implementation.

## Problem

With high concurrency (up to 300 pages in flight), page image data can overwhelm VM memory badly enough to require restarts. The `--rate-limit` flag controls API request rate but not memory consumption. Users need a way to cap how much RAM is used to store in-flight image data.

The goal is not byte-perfect accounting, but close enough that `--page-memory-limit=2G` meaningfully caps memory used by image buffers.

## Concurrency model and the O(n²) problem

The OCR pipeline has nested concurrency controlled by a single `--jobs N` parameter:

```
apply_stream_buffering_opts(stream)
    .buffered(job_count)                    ← up to job_count doc futures polled concurrently
        │
        ▼ (inside each doc future)
split_pages::ocr_file()
    page_stream.buffered(concurrency_limit) ← up to job_count pages per doc
        │
        ▼ (inside each page future)
    engine.ocr_page()
        │
        ▼ (LLM path)
    chat_queue.handle().process_blocking()  ← WorkQueue: mpsc(job_count) + job_count workers
```

The chat `WorkQueue` provides backpressure (bounded channel), but `.buffered()` at the page level eagerly polls up to `job_count` futures per document, materializing `Page.data` for each. With `job_count` documents active, this means up to **`job_count²` pages loaded in memory** before backpressure kicks in. At `--jobs 300`, that's potentially 90,000 page buffers.

## Architectural insight: defer image loading

The root cause of the O(n²) problem is that `PageIter` eagerly loads image bytes into `Page.data: Vec<u8>`. But `PageIter` writes pages to tmpdir and then reads them back — the files exist on disk and can be loaded later. If `PageIter` yields lightweight `ImageFile` handles (just mime type + path) instead of loaded data, the O(n²) buffering becomes trivially cheap. Actual image loading happens at the latest responsible moment — in the driver, right before the API call — where the `WorkQueue` already provides concurrency-based backpressure.

This also eliminates the wasteful round-trip where images are base64-encoded into data URL strings during template rendering, only for Bedrock and Vertex drivers to decode them back to binary. With late loading, each driver loads images in the encoding it actually needs.

## Image representation: `file:` URLs

Images flow through the template/prompt system as `file:` URL strings:

```
file:///tmp/tiff-pages-abc123/page-00001.png?mime_type=image/png
```

This is a syntactically valid URI (RFC 8089 + RFC 3986 query parameters). `ImageFile` provides `to_url()` and `from_url()` methods for conversion. Paths are percent-encoded per standard URI rules.

The `file:` URL is cheap to carry through template bindings (it's a short string), cheap to serialize/deserialize, and drivers parse it back to an `ImageFile` to load with their preferred encoding. The `data:` URL format is no longer used — this is a breaking change (pre-1.0, noted in release notes) but compatible with all existing "normal" templates and use cases, since `data:` URLs were only ever produced by our own helpers.

## Image loading paths

All image loading goes through `ImageFile::load()` or `ImageFile::load_blocking()`, which acquire a `MemPermit` from a `MemLimiter` before reading from disk. This is the single enforcement point for memory limits.

Two paths produce `ImageFile` handles:

1. **PageIter** (`src/page_iter/`): Yields `ImageFile` for each page (PDF split, TIFF→PNG, or single image). Files live in tmpdirs owned by the `PageIter`.

2. **Handlebars `image-data-url` helper** (`src/prompt.rs`): Constructs an `ImageFile` from a user-provided path and renders it as a `file:` URL string. (The helper name is retained for backwards compatibility even though it no longer produces `data:` URLs.)

### TIFF decode intermediates

`src/page_iter/tiff.rs` decodes each TIFF IFD through multiple stages (raw data → color conversion → DynamicImage → PNG encoding → tmpdir). Currently up to ~3x the decoded pixel size per IFD can coexist. With explicit `drop()` calls, this can be reduced to two versions at a time. These intermediates are transient (written to disk before entering the pipeline) and are not tracked by the memory limit.

## Design decisions

### Late loading at the driver level

Drivers receive `file:` URLs in rendered prompts and call `ImageFile::load()` with their preferred encoding:

| Driver | Encoding needed | Current waste eliminated |
|--------|----------------|------------------------|
| OpenAI | `DataUrl` | No change (was already string-based) |
| Bedrock | `Binary` | Eliminates base64 encode→decode round-trip |
| Vertex | `Binary` | Eliminates base64 encode→decode round-trip |
| Native (genai) | `Base64` | Eliminates data URL encode→parse round-trip |
| Textract | `Binary` | Eliminates `.clone()` of page bytes |
| Tesseract | `Binary` | Writes directly to temp file |

### Memory permits acquired per-image at load time

Each `ImageFile::load()` call acquires one `MemPermit` from the `MemLimiter`. The permit is held inside `ImageData` and released on drop (after the API call completes). The permit size covers the loaded encoding (e.g., binary size for Bedrock, base64-expanded size for OpenAI).

### Deadlock properties

**OCR path (phase 1): Deadlock-free.** Each page makes exactly one `load()` call. The OCR engines process one page at a time per task. No second acquire means no circular wait.

**Chat path with multiple images per prompt: Potential deadlock.** If a prompt template references multiple images (e.g., `{{image-data-url 'a.jpg'}}` and `{{image-data-url 'b.jpg'}}`), loading them sequentially within one task while other tasks do the same could deadlock under tight memory limits. **For phase 1, the chat path uses `MemLimiter::unlimited()`.** A future phase can address this with child `MemLimiter`s that pre-allocate budget for all images in a single prompt.

### Semaphore primitive: `tokio::sync::Semaphore` with KB granularity

`tokio::sync::Semaphore` supports async `acquire()` and sync `Drop`-based release. By tracking in KB internally (transparent to callers), a single acquisition supports images up to ~4 GB, and the total budget can reach `u32::MAX * 1024` bytes (~4 TB). No new dependencies required.

### Acquire timeout for deadlock recovery

`MemLimiter::acquire()` uses the existing `WithTimeout` machinery. If a permit cannot be acquired within the timeout, the operation fails with an error rather than blocking forever. This limits the blast radius of any future deadlock bugs to a single task/document rather than the whole process.

### CLI argument

`--page-memory-limit=NNN(k|M|G)` using an appropriate size-parsing library. When unset, `MemLimiter::unlimited()` provides a no-op semaphore so all code paths work unconditionally without branching.

### Operational note: external hard limits

Independently of this feature, production deployments should set an external hard memory limit (k8s container limits, `ulimit`, cgroups) to prevent the OOM killer from taking down the VM. The `--page-memory-limit` provides graceful backpressure; the external limit is a safety net. Together they mean a job might slow down or fail, but won't require a VM restart.

## Step 1: Core types (implemented)

### `src/mem_limit.rs`

Two-layer design mirroring the existing `RateLimit` / `RateLimiter` pattern:

- **`MemLimit`** — Newtype wrapping `bytesize::ByteSize`. Deserializable from CLI strings like `"2G"`, `"500M"`, `"4096k"` via `ByteSize`'s `FromStr` impl, which integrates directly with clap. Analogous to `RateLimit`.
  - `MemLimit::to_mem_limiter(&self, acquire_timeout: Option<Duration>) -> MemLimiter` — Constructs the runtime limiter. Analogous to `RateLimit::to_rate_limiter()`.
- **`MemLimiter`** — The runtime semaphore. Wraps `Arc<Semaphore>` (KB-granularity internally, bytes in public API). Stores `ram_limit_bytes` for validation and `acquire_timeout` for deadlock recovery. Analogous to `RateLimiter`.
  - `byte_limit(ram_limit_bytes: usize, acquire_timeout: Option<Duration>) -> Self` — Direct construction (used by `MemLimit::to_mem_limiter()`).
  - `unlimited() -> Self` — No-op limiter for when no CLI flag is set.
  - `async acquire(&self, ram_amount_bytes: usize) -> Result<MemPermit>` — Validates request doesn't exceed total limit, converts to KB, acquires with timeout.
  - `acquire_blocking()` — Sync variant, commented out for future reference. Not needed currently since all image loading happens in async contexts (drivers and OCR engines). Would use `Handle::current().block_on(self.acquire(...))`.
- **`MemPermit`** — Opaque owned wrapper around `OwnedSemaphorePermit`. Releases on drop.

`MemLimit` lives in `LlmOpts` (CLI-parsed, cloneable, serializable). `MemLimiter` lives in `ProcessorState` (runtime, shared via `Arc`), constructed from `MemLimit` during queue setup — the same pattern as `rate_limit` → `rate_limiter` in `create_chat_work_queue()`.

### `src/images.rs`

- **`ImageEncoding`** — Enum: `Binary`, `Base64`, `DataUrl`. Has `max_loaded_size(mime_type, file_size)` to compute upper-bound buffer size for each encoding. Permits are acquired for the `max_loaded_size`, not just the raw file size.
- **`ImageFile`** — Metadata handle (`mime_type` + `path`). Trivially cheap to clone and carry through the pipeline. No tempdir reference — see Step 2 for tempdir lifetime management.
  - `from_path(path) -> Result<Self>` — Detects MIME type.
  - `to_url() -> String` — Serializes as `file:///path?mime_type=mime` using the `url` crate. Percent-encoded per standard URI rules.
  - `from_url(url) -> Result<Self>` — Parses a `file:` URL back to an `ImageFile` using the `url` crate. Returns `Err` on parse failure (since we produce these URLs ourselves, failure is a bug).
  - `async load(encoding, mem_limiter) -> Result<ImageData>` — Acquires a `MemPermit`, loads on a blocking thread via `spawn_blocking`.
  - `load_blocking()` — Sync variant, commented out for future reference. Not needed currently since all loading is async.
- **`ImageData`** — Holds loaded bytes + encoding + mime_type + `MemPermit`. Exposes `data() -> &[u8]`, `mime_type()`, `encoding()`. Permit released on drop.

The async `load()` delegates to a private `load_helper()` method for the actual file I/O and buffer construction. The commented-out `load_blocking()` shares the same helper.

### Design note: no generic `MemHandle<T>`

The original plan included a generic `MemHandle<T>` with `Deref`. The implementation instead uses `ImageData` as a purpose-built type that holds the permit internally. This is simpler and sufficient because all memory-limited data is image data loaded through `ImageFile`.

## Step 2: `PageIter` yields `ImageFile` instead of loaded data

The core architectural change. `PageIter` becomes a lightweight producer of `ImageFile` handles.

### Changes to `Page`

`Page` is replaced by `ImageFile` (or simplified to a thin wrapper if additional metadata is needed). `PageIter` yields `ImageFile` items. The `Page` struct with `data: Vec<u8>` is removed.

This eliminates the O(n²) memory problem: 90,000 `ImageFile` structs in `.buffered()` queues cost essentially nothing.

### Changes to `PageIter`

`PageIter::next()` no longer calls `fs::read()`. It constructs an `ImageFile` from the tmpdir file path and detected MIME type. No `MemLimiter` interaction in `PageIter` at all — it's just producing cheap handles.

Files are **not** deleted eagerly after yielding (unlike today where they're deleted after `fs::read()`). Instead, the tmpdir is cleaned up when it is dropped after all pages are processed.

### Tempdir lifetime

`ImageFile` is serialized/deserialized as a `file:` URL string (see "Image representation" above), so it cannot hold an `Arc<TempDir>` — that wouldn't survive the round-trip.

Instead, `PageIter::from_path` returns both the iterator and an opaque tempdir handle:

```rust
let (page_iter, _tmpdir_handle) = PageIter::from_path(...).await?;
```

The caller (`SplitPagesOcrEngine::ocr_file`) holds `_tmpdir_handle` until all page processing completes. Since `ocr_file` already `.collect()`s all results before returning, the tempdir stays alive through the entire `.buffered()` pipeline. The handle type is `Option<TempDir>` (or a newtype wrapper), and is `None` for single-image inputs that don't use a tmpdir.

### Changes to OCR engines

OCR engines receive `ImageFile` instead of `Page` with loaded data:

- **LLM engine** (`llm.rs`): Converts `ImageFile` to a `file:` URL string and passes it as the `page_data_url` template binding. No image data loaded at this point — just a short URL string. Loading happens later in the driver.
- **Textract engine** (`textract.rs`): Calls `image_file.load(ImageEncoding::Binary, &mem_limiter)` to get bytes for the Textract API `Blob`.
- **Tesseract engine** (`tesseract.rs`): Can either load the image or (better) pass the `ImageFile.path` directly to the tesseract CLI, avoiding a load entirely.

### Removal of `data_url.rs`

The `data_url()` function and `parse_data_url()` are no longer needed. `ImageData` with `DataUrl` encoding handles the creation side; `ImageFile::from_url()` handles the parsing side. The module can be removed.

### Removal of few-shot example image

The LLM OCR engine currently includes an embedded example image (`EXAMPLE_INPUT`) and example output for few-shot prompting. This is removed as a breaking change:

- Few-shot prompting is effectively disabled in practice — it doesn't improve Gemini's instruction-following and historically caused ~20% RECITATION error rates by confusing Gemini's copyright filters.
- The `example_input_data_url` and `example_output` template variables are removed. Custom OCR prompt templates using them will get a clear template rendering error.
- Users who need few-shot examples for specific models (e.g., Qwen, small Gemma visual models) can supply external image files via the `{{image-data-url path}}` helper.
- Noted in release notes as a breaking change affecting custom OCR prompt templates only.

## Step 3: Driver-level image loading

Drivers gain the ability to load images from `file:` URLs using their preferred encoding.

### Changes to driver image handling

Each driver's message-building code currently parses `data:` URLs from the rendered prompt's `images: Vec<String>`. This changes to:

1. Check if the URL is a `file:` URL → parse to `ImageFile` → call `load()` with driver-preferred encoding.
2. Build the API-specific image block from `ImageData`.

Driver trait signatures gain a `&MemLimiter` parameter (or receive it through their existing options/state).

### Per-driver changes

- **OpenAI** (`openai.rs`): `ImageFile::load(DataUrl, &mem_limiter)` → pass data URL string to API. (OpenAI accepts data URLs directly.)
- **Bedrock** (`bedrock.rs`): `ImageFile::load(Binary, &mem_limiter)` → `ImageSource::Bytes(Blob::new(data))`. Eliminates the current base64→decode round-trip.
- **Vertex** (`vertex.rs`): `ImageFile::load(Binary, &mem_limiter)` → `Blob::new().set_data(bytes)`. Same improvement as Bedrock.
- **Native** (`native.rs`): `ImageFile::load(Base64, &mem_limiter)` → `ImageSource::Base64(Arc::from(data))`. Eliminates parsing data URLs.

### Permit lifecycle

The `ImageData` (with its `MemPermit`) is created at driver load time, used to build the API request, and dropped after the request completes (or after serialization, if the SDK takes ownership of the bytes). This minimizes the permit window to just the API call.

## Step 4: Handlebars helper produces `file:` URLs

### Changes to `image-data-url` helper

The helper produces `file:` URL strings instead of `data:` URL strings:

1. Construct `ImageFile::from_path(path)`
2. Write `image_file.to_url()` to the template output

No image loading happens during template rendering. The helper name is retained for backwards compatibility. The resulting `file:` URL flows through the rendered prompt to the driver, where it's loaded with the appropriate encoding.

This is a **breaking change** for the `data:` URL output format, but is compatible with all existing template usage patterns. Noted in 0.x+1 release notes.

### `MemLimit` no longer needed during rendering

Since the helper just produces a URL string, `ChatPrompt::render()` does not need a `MemLimit` parameter. Memory limits are enforced at load time in the drivers.

### Chat path deadlock status

With the helper producing `file:` URLs, images are loaded at the driver level. For prompts with multiple images, the driver loads them sequentially within one task. **Phase 1 uses `MemLimiter::unlimited()` for the user-facing `chat` command** to avoid multi-image deadlocks. The OCR LLM path (which creates its own chat work queue with one image per page) uses the real `MemLimiter`. A future phase can address multi-image chat with child `MemLimiter`s.

## Step 5: CLI integration

- Add `--page-memory-limit` field as `Option<MemLimit>` to `LlmOpts`. `MemLimit` wraps `bytesize::ByteSize`, whose `FromStr` impl handles parsing `"2G"`, `"500M"`, etc. and integrates directly with clap's derive API.
- `Driver::chat_completion()` gains a `&MemLimiter` parameter so drivers can load images with memory tracking.
- In `create_chat_work_queue()`: construct `MemLimiter` from `llm_opts.page_memory_limit` via `mem_limit.to_mem_limiter(acquire_timeout)`, or `MemLimiter::unlimited()` if unset. Store in `ProcessorState`. This mirrors the existing `llm_opts.rate_limit` → `rate_limiter` pattern.
- `ProcessorState::mem_limiter` is passed to `run_chat_inner()`, which passes it to the driver via `chat_completion()`.
- The user-facing `chat` command always passes `MemLimiter::unlimited()` (phase 1 — avoids multi-image deadlocks). The OCR LLM path passes the real `MemLimiter` (one image per page, deadlock-free).
- For OCR engines that load images directly (Textract, Tesseract), construct a `MemLimiter` from `OcrOpts::llm_opts` at engine creation time.
- The acquire timeout can reuse the existing `--llm-timeout` value or have its own flag.

## Step 6: Tests

- Unit tests for `MemLimiter` and `MemPermit` (acquire/release, unlimited, timeout, KB rounding).
- Unit tests for `MemLimit` (parsing from strings like `"2G"`, `"500M"`, conversion to `MemLimiter`).
- Unit tests for `ImageFile` (from_path, to_url/from_url round-trip, load with each encoding).
- Unit tests for `ImageData` (data access, permit release on drop).
- Update an existing CLI OCR integration test in `tests/cli.rs` to pass `--page-memory-limit` (smoke test that the flag is wired up and doesn't break anything; actually hitting the limit under test is impractical).

## Future work

- **Child `MemLimiter` for multi-image prompts**: Pre-allocate a budget for all images in a prompt, allowing the chat path to use real memory limits without deadlock.
- **Path safety for web UI**: Constrain `ImageFile::load()` / `MemLimiter` to only load from within a specified working directory. Single enforcement point makes this straightforward.
- **TIFF decode memory tracking**: Track the transient intermediate buffers during TIFF→PNG conversion. Lower priority since this is sequential per-document and doesn't multiply with concurrency.
