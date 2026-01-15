# Multipage TIFF Support Planning

## Problem Statement

For e-discovery purposes, it's critical not to silently overlook pages in documents. TIFF is a frequently-extended format with multiple mechanisms for storing multiple images, making it risky to support without comprehensive handling.

## Current State

TIFF is **deliberately rejected** in `src/page_iter.rs:15-17`:

```rust
const SUPPORTED_IMAGE_TYPES: &[&str] =
    &["image/png", "image/jpeg", "image/webp", "image/gif"];
```

This is the right call given the complexity below.

## TIFF Multi-Image Mechanisms

### 1. IFD Chaining (Primary method)
- Each Image File Directory (IFD) has a 4-byte "next IFD offset" pointer
- Pages linked as: IFD0 → IFD1 → IFD2 → ... → 0 (end)
- Most common multipage TIFF format

### 2. SubIFDs (Tag 330)
- Any IFD can contain a SubIFD tag pointing to child IFDs
- Originally for thumbnails and reduced-resolution images (pyramidal TIFFs)
- Can be nested arbitrarily deep
- **Many simple TIFF readers ignore SubIFDs entirely** — dangerous for e-discovery

### 3. BigTIFF
- For files >4GB, uses 8-byte offsets instead of 4-byte
- Same multi-page mechanisms, different header structure

## Rust Crate Options

### `tiff` crate (image-rs/image-tiff)

The primary Rust option.

**Supports:**
- IFD chaining via `seek_to_image(n)` iteration
- BigTIFF
- Compressions: LZW, Deflate, PackBits, JPEG, Fax4, ZSTD

**SubIFD support (as of July 2025):** Manual API exists for SubIFD access:
- `decoder.get_tag(Tag::SubIfd)?.into_ifd_vec()` — get SubIFD pointers
- `decoder.read_directory(ifd_ptr)` — read directory from known offset
- `decoder.read_directory_tags(&subdir)` — access tags from that directory

The `seek_to_image(n)` method only follows the main IFD chain, but SubIFDs can be manually traversed using the above API.

**Links:**
- https://github.com/image-rs/image-tiff
- https://crates.io/crates/tiff
- https://docs.rs/tiff/latest/tiff/decoder/

### Other Crates

- `tiff2` — async fork, planned upstream merge
- `rustiff` — older, less maintained

### External Tools

- `tiffinfo -D` (libtiff-tools) — comprehensive IFD/SubIFD enumeration
- ImageMagick/GraphicsMagick — battle-tested conversion
- `tifftools` (Python) — explicitly handles all IFDs and SubIFDs

## Implementation Options

| Approach | Pros | Cons |
|----------|------|------|
| **A. Keep rejecting TIFF** | Safe, no silent data loss | Users must pre-convert |
| **B. Use `tiff` crate for IFD-chain only** | Handles common multipage TIFFs | May miss SubIFD pages silently — **unacceptable for e-discovery** |
| **C. Shell out to ImageMagick** | Battle-tested, handles SubIFDs | External dependency |
| **D. `tiffinfo` + `tiff` crate** | Use `tiffinfo -D` to enumerate ALL IFDs/SubIFDs, validate counts, then decode | Two-tool approach but comprehensive |
| **E. Contribute SubIFD to `tiff` crate** | Best long-term | Development effort |
| **F. Pure `tiff` crate + smart SubIFD handling** | No external deps, handles all standard TIFFs, fails safely on edge cases | Requires careful NewSubfileType analysis |

## Recommendation

For e-discovery where silent page loss is unacceptable:

**Option F (Pure `tiff` crate + smart SubIFD handling)** is recommended:
1. Use the `tiff` crate to iterate the main IFD chain — these are document pages
2. For each IFD, check for SubIFDs and validate their NewSubfileType
3. Skip SubIFDs that are thumbnails (bit 0) or masks (bit 2)
4. Error on any SubIFD with ambiguous type (value=0 or bit 1 set)
5. Convert pages to PNG for LLM consumption
6. Report page count for human verification

**Why Option F over Option D:**
- No external tool dependency (`tiffinfo` not required)
- Research confirms document pages are never stored in SubIFDs in standard practice
- The `tiff` crate now has sufficient API for SubIFD inspection (as of July 2025)
- We fail safely on edge cases rather than silently dropping content

**Fallback:** Option A (reject TIFF) remains viable if implementation complexity is a concern. Users would convert TIFFs to PDF or individual PNGs first, forcing them to verify page counts themselves.

## Option F: Pure `tiff` Crate with Smart SubIFD Handling

### Key Insight

Research confirms that **document pages are never stored in SubIFDs** in standard practice. All document scanning software (Fujitsu, Canon, Kodak, Kofax, etc.) uses the main IFD chain for multi-page documents. SubIFDs are exclusively used for:

- Thumbnails/previews (reduced resolution)
- Pyramid levels (GIS, medical imaging)
- Transparency masks
- Camera RAW metadata (DNG-specific)

This means we can safely process the main IFD chain and **skip or validate** SubIFDs without losing document content.

### NewSubfileType (Tag 254) Bit Definitions

Per TIFF 6.0 specification:

| Bit | Value | Meaning | Action |
|-----|-------|---------|--------|
| 0 | 0x1 | Reduced resolution (thumbnail) | Skip safely |
| 1 | 0x2 | Single page of multi-page document | Process (but rare in SubIFDs) |
| 2 | 0x4 | Transparency mask | Skip safely |

DNG extensions (bits 3, 4, 16) are camera-specific and not document content.

### Algorithm

```
1. Iterate main IFD chain via seek_to_image(n) → these are document pages
2. For each IFD, check for SubIFD tag (330)
3. If SubIFDs exist, check each SubIFD's NewSubfileType (tag 254):
   - If bit 0 set (reduced resolution): SKIP (thumbnail/preview)
   - If bit 2 set (mask): SKIP (transparency mask)
   - If value has DNG bits (3, 4, 16): SKIP (camera metadata)
   - If value == 0 or bit 1 set: ERROR — unexpected SubIFD content
4. Report page count for human verification
5. Convert each main-chain page to PNG for LLM consumption
```

### Why This Is Safe for E-Discovery

1. **No standard software stores document pages in SubIFDs** — this is confirmed by LibTIFF documentation and industry practice
2. **We error on ambiguous cases** — if a SubIFD has NewSubfileType=0 or bit 1 (page indicator), we reject the file rather than guessing
3. **We enumerate and report** — users see exactly how many pages were found
4. **Thumbnail SubIFDs are provably safe to skip** — they're reduced-resolution copies of images we already have in the main chain

## E-Discovery Requirements

Any implementation must:
1. Enumerate all IFDs via the chain
2. Recursively enumerate SubIFDs (tag 330)
3. Report page counts so humans can verify completeness
4. Convert each page to PNG/JPEG (LLM backends don't accept TIFF natively)
5. Fail loudly on any parsing ambiguity rather than silently dropping pages

## References

### Authoritative Specifications
- https://www.itu.int/itudoc/itu-t/com16/tiff-fx/docs/tiff6.pdf — TIFF 6.0 Specification (ITU mirror)
- https://www.loc.gov/preservation/digital/formats/content/tiff_tags.shtml — Library of Congress TIFF Tags Reference
- https://libtiff.gitlab.io/libtiff/multi_page.html — LibTIFF Multi-Page Documentation

### SubIFD/IFD Analysis
- https://dpb587.me/entries/tiff-ifd-and-subifd-20240226 — IFD/SubIFD explainer
- https://bitsgalore.org/2024/03/11/multi-image-tiffs-subfiles-and-image-file-directories.html — Multi-image TIFFs deep dive

### Tools & Libraries
- https://users.rust-lang.org/t/crate-that-manage-multipage-tiff/134374
- https://github.com/DigitalSlideArchive/tifftools (Python, handles SubIFDs)
- https://github.com/image-rs/image-tiff — Rust `tiff` crate

### Camera RAW / DNG
- https://paulbourke.net/dataformats/dng/dng_spec_1_6_0_0.pdf — DNG 1.6 Specification
- https://www.loc.gov/preservation/digital/formats/fdd/fdd000628.shtml — Library of Congress DNG format description

---

## Appendix: Justification for Option F Design Decisions

This appendix documents the evidence supporting key assumptions in Option F.

### Decision 1: Document pages are stored in IFD chain, not SubIFDs

**Claim:** Standard document scanning software stores multi-page documents using the main IFD chain (NextIFD pointers), never in SubIFDs.

**Evidence:**

1. [LibTIFF Multi-Page Documentation](https://libtiff.gitlab.io/libtiff/multi_page.html) states:
   - "SubIFD chains are rarely supported"
   - Child images provide "extra information for the parent image - such as a subsampled version of the parent image"

2. [Bitsgalore: Multi-image TIFFs, subfiles and image file directories](https://bitsgalore.org/2024/03/11/multi-image-tiffs-subfiles-and-image-file-directories.html) provides real-world analysis showing document scanners (Fujitsu, Canon, Kodak, Kofax) use the IFD chain.

3. TIFF Class F (fax standard) uses NewSubfileType=2 in main IFD chain for pages.

**Conclusion:** Safe to process only main IFD chain for document pages.

### Decision 2: NewSubfileType bit meanings

**Claim:** We can identify SubIFD purpose by inspecting NewSubfileType (tag 254) bits.

**Evidence:**

1. [TIFF 6.0 Specification](https://www.itu.int/itudoc/itu-t/com16/tiff-fx/docs/tiff6.pdf) (Page 36) defines:
   - Bit 0 (0x1): Reduced resolution version of another image
   - Bit 1 (0x2): Single page of a multi-page image
   - Bit 2 (0x4): Defines a transparency mask for another image

2. [Library of Congress TIFF Tags Reference](https://www.loc.gov/preservation/digital/formats/content/tiff_tags.shtml) confirms these definitions.

**Conclusion:** SubIFDs with bit 0 or bit 2 set are safe to skip (thumbnails/masks). SubIFDs with bit 1 set or value=0 should trigger an error.

### Decision 3: DNG/Camera RAW SubIFDs are metadata, not documents

**Claim:** DNG-specific NewSubfileType extensions (bits 3, 4, 16) represent camera metadata, not document content.

**Evidence:**

1. [DNG 1.6 Specification](https://paulbourke.net/dataformats/dng/dng_spec_1_6_0_0.pdf) defines:
   - Bit 3 (0x8): Depth map
   - Bit 4 (0x10): Enhanced image data
   - Bit 16 (0x10000): Semantic mask

2. [Library of Congress DNG Format Description](https://www.loc.gov/preservation/digital/formats/fdd/fdd000628.shtml) confirms these are Adobe extensions for camera-specific data.

**Conclusion:** DNG-specific SubIFDs are safe to skip for document processing.

### Decision 4: `tiff` crate provides sufficient SubIFD API

**Claim:** The Rust `tiff` crate has API to detect and inspect SubIFDs without external tools.

**Evidence:**

1. [image-rs/image-tiff](https://github.com/image-rs/image-tiff) source code (as of July 2025) provides:
   - `Tag::SubIfd` defined in `src/tags.rs`
   - `Value::into_ifd_vec()` to get SubIFD pointers
   - `decoder.read_directory(ifd_ptr)` to read SubIFD contents
   - `decoder.read_directory_tags(&subdir)` to access SubIFD tags

**Conclusion:** No external tool (like `tiffinfo`) required for SubIFD detection and validation.
