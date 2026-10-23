//! OvisOCR3-based OCR engine.
//!
//! [OvisOCR3](https://huggingface.co/ATH-MaaS/OvisOCR3) is a small OCR model
//! trained on fixed prompts. We use its "structured parsing" mode, which
//! returns a JSON array of blocks with bounding boxes, so that we can reuse
//! our normal LLM drivers and constrain the output with a schema.
//!
//! Structured parsing replaces charts and images with `<img>` placeholders. To
//! recover their text, we crop each one out of the page and send it back
//! through the model's "plain OCR" mode, which returns plain text.

use std::{
    fs::File,
    io::{BufReader, Cursor},
    sync::{Arc, LazyLock},
};

use base64::{Engine as _, prelude::BASE64_STANDARD};
use genai::{
    Client,
    chat::{
        ChatMessage, ChatOptions, ChatRequest, ChatRole, ContentPart, MessageContent,
        Usage,
    },
};
use image::ImageFormat;
use regex::Regex;
use schemars::JsonSchema;
use serde_json::Map;

use super::page::{OcrPageEngine, OcrPageInput, OcrPageOutput};
use crate::{
    async_utils::{JoinWorker, blocking_iter_streams::spawn_blocking_propagating_panics},
    cmd::ocr::OcrOpts,
    drivers::{TokenUsage, llm::native::NativeDriver},
    images::ImageFile,
    mem_limit::{MEM_PERMIT_DEADLOCK_TIMEOUT, MemLimiter},
    prelude::*,
    prompts::chat::ChatPrompt,
    queues::{
        chat::{ChatInput, ChatOutput, create_chat_work_queue},
        work::{WorkInput, WorkItemProcessor as _, WorkQueue},
    },
    retry::{
        DEFAULT_JITTER, retry_result_ok, retry_with_backoff, try_potentially_transient,
    },
    schema::Schema,
    timeouts::WithTimeout as _,
    toml_utils::from_toml_str,
};

/// Our fixed OvisOCR3 structured parsing prompt.
const OVIS_OCR_PROMPT: &str = include_str!("ovis/ovis_ocr_prompt.toml");

/// The `plain_ocr.txt` prompt from the OvisOCR3 model card, verbatim. We use
/// this to OCR charts and images cropped out of the page.
const PLAIN_OCR_PROMPT: &str = include_str!("ovis/plain_ocr.txt");

/// Maximum completion tokens for OCRing a single chart or image. These rarely
/// contain much text, and a lower limit keeps repetition loops cheap.
const FIGURE_MAX_COMPLETION_TOKENS: u32 = 4096;

/// Skip crops narrower or shorter than this many pixels.
const MIN_CROP_PIXELS: u32 = 8;

/// Skip crops with a smaller area than this, in pixels. These are usually
/// icons or bullets, and OCRing them tends to produce junk like "m" or "didi".
/// We limit area, not size, so that wide, single-line images like logos
/// still get OCRed.
const MIN_CROP_AREA: u64 = 32 * 32;

/// Does `model` look like an OvisOCR model?
pub fn is_ovis_model(model: &str) -> bool {
    model.contains("OvisOCR")
}

/// The category of a block, as defined by OvisOCR3's structured prompt.
//
// We deliberately don't put doc comments on the variants, because that would
// make `schemars` generate a `oneOf` instead of a simple `enum`.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OvisCategory {
    Abstract,
    Algorithm,
    AsideText,
    Chart,
    Content,
    DisplayFormula,
    DocTitle,
    FigureTitle,
    Footer,
    FooterImage,
    Footnote,
    FormulaNumber,
    Header,
    HeaderImage,
    Image,
    InlineFormula,
    Number,
    ParagraphTitle,
    ReferenceContent,
    Seal,
    Table,
    Text,
    VerticalText,
    VisionFootnote,
}

impl OvisCategory {
    /// Is this an image block, whose content is just an `<img>` placeholder?
    fn is_image(self) -> bool {
        matches!(
            self,
            Self::Chart | Self::FooterImage | Self::HeaderImage | Self::Image
        )
    }
}

/// A block of content on the page.
//
// Field order matters here: it determines the key order allowed by the
// grammar, which should match the order OvisOCR3 was trained on.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct OvisBlock {
    /// Bounding box as `[x_min, y_min, x_max, y_max]`, scaled to [0, 1000).
    bbox: [u32; 4],

    /// The category of this block.
    category: OvisCategory,

    /// The content of this block, as Markdown, LaTeX or an HTML table.
    content: String,

    /// For image blocks, text found by OCRing the cropped image. Not part of
    /// the model's output.
    #[serde(skip)]
    figure_text: Option<String>,
}

/// Convert OvisOCR3 blocks to text.
///
/// Image blocks are replaced by their `figure_text`, if any.
fn blocks_to_text(blocks: &[OvisBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| {
            let content = if b.category.is_image() {
                b.figure_text.clone()?
            } else if b.category == OvisCategory::Table {
                clean_table_breaks(&b.content)
            } else {
                b.content.clone()
            };
            let content = content.trim();
            (!content.is_empty()).then(|| content.to_owned())
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Clean up literal `\n` sequences (a backslash followed by `n`) in an HTML
/// table.
///
/// OvisOCR3 sometimes double-escapes line breaks inside table cells. Breaks at
/// the start or end of a cell are noise, so we remove them. Breaks inside a
/// cell are often meaningful, so we turn them into `<br>`.
fn clean_table_breaks(html: &str) -> String {
    static CELL_START: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(<t[dh]\b[^>]*>)(?:\s|\\n)+").expect("invalid regex")
    });
    static CELL_END: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:\s|\\n)+(</t[dh]>)").expect("invalid regex"));
    let html = CELL_START.replace_all(html, "$1");
    let html = CELL_END.replace_all(&html, "$1");
    html.replace("\\n", "<br>")
}

/// Remove a repeated tail from a runaway response.
///
/// This is a port of `_clean_truncated_repeats` from the OvisOCR3 model card,
/// with the same default thresholds. It keeps one copy of the repeated unit.
fn clean_truncated_repeats(text: &str) -> String {
    const MIN_TEXT_LEN: usize = 8000;
    const MAX_PERIOD: usize = 200;
    const MIN_PERIOD: usize = 1;
    const MIN_REPEAT_CHARS: usize = 100;
    const MIN_REPEAT_TIMES: usize = 5;

    // The original works on Python string indices, so we use chars.
    let chars = text.chars().collect::<Vec<_>>();
    let n = chars.len();
    if n < MIN_TEXT_LEN {
        return text.to_owned();
    }

    let max_period = MAX_PERIOD.min(n - 1);
    for unit_len in MIN_PERIOD..=max_period {
        if chars[n - 1] != chars[n - 1 - unit_len] {
            continue;
        }

        let mut match_len = 1;
        let mut idx = n - 2;
        while idx >= unit_len && chars[idx] == chars[idx - unit_len] {
            match_len += 1;
            idx -= 1;
        }

        let total_len = match_len + unit_len;
        let repeat_times = total_len / unit_len;
        let tail_len = total_len % unit_len;

        if repeat_times >= MIN_REPEAT_TIMES && total_len >= MIN_REPEAT_CHARS {
            return chars[..n - total_len + unit_len]
                .iter()
                .chain(&chars[n - tail_len..])
                .collect();
        }
    }
    text.to_owned()
}

/// Convert a bounding box in [0, 1000) coordinates to pixel `(x, y, width,
/// height)`. Returns `None` if the box is empty or too small to OCR.
fn bbox_to_pixels(
    bbox: [u32; 4],
    width: u32,
    height: u32,
) -> Option<(u32, u32, u32, u32)> {
    let scale = |v: u32, size: u32| {
        let px = u64::from(v.min(1000)) * u64::from(size) / 1000;
        u32::try_from(px).expect("scaled coordinate should be at most the image size")
    };
    let [x1, y1, x2, y2] = bbox;
    let (left, top) = (scale(x1, width), scale(y1, height));
    let (right, bottom) = (scale(x2, width), scale(y2, height));
    let (w, h) = (right.saturating_sub(left), bottom.saturating_sub(top));
    (w >= MIN_CROP_PIXELS
        && h >= MIN_CROP_PIXELS
        && u64::from(w) * u64::from(h) >= MIN_CROP_AREA)
        .then_some((left, top, w, h))
}

/// Crop each bounding box out of the image at `path`, returning PNG data, or
/// `None` for boxes that are too small.
///
/// This decodes the whole page, so call it on a blocking thread.
//
// TODO: This doesn't reserve RAM from a `MemLimiter` for the decoded page.
fn crop_regions(image: &ImageFile, boxes: &[[u32; 4]]) -> Result<Vec<Option<Vec<u8>>>> {
    // Use the MIME type detected from the file contents, not the extension,
    // because real-world files are sometimes misnamed.
    let format = ImageFormat::from_mime_type(image.mime_type())
        .ok_or_else(|| anyhow!("unsupported image type {}", image.mime_type()))?;
    let page = File::open(&image.path)
        .map_err(anyhow::Error::from)
        .and_then(|file| Ok(image::load(BufReader::new(file), format)?))
        .with_context(|| {
            format!("failed to decode page image {}", image.path.display())
        })?;
    boxes
        .iter()
        .map(|bbox| {
            let Some((x, y, w, h)) = bbox_to_pixels(*bbox, page.width(), page.height())
            else {
                return Ok(None);
            };
            let mut png = vec![];
            page.crop_imm(x, y, w, h)
                .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
                .context("failed to encode cropped image")?;
            Ok(Some(png))
        })
        .collect()
}

/// An OvisOCR3-based OCR engine.
pub struct OvisOcrPageEngine {
    /// Pointer to the work queue that talks to the LLM.
    chat_queue: WorkQueue<ChatInput, ChatOutput>,

    /// A `genai` client for plain-text requests, which our chat queue doesn't
    /// support. This honors `OPENAI_API_BASE` like our native driver.
    client: Client,

    /// Our OCR options.
    ocr_opts: Arc<OcrOpts>,
}

impl OvisOcrPageEngine {
    /// Create a new OvisOCR3-based OCR engine. We ignore any user-supplied
    /// prompt, because the model is trained on a fixed prompt.
    #[allow(clippy::new_ret_no_self)]
    pub async fn new(
        concurrency_limit: usize,
        ocr_opts: Arc<OcrOpts>,
    ) -> Result<(Arc<dyn OcrPageEngine>, JoinWorker)> {
        let mut prompt = from_toml_str::<ChatPrompt>(OVIS_OCR_PROMPT)
            .expect("failed to parse built-in OvisOCR prompt");
        prompt.response_schema = Schema::from_type::<Vec<OvisBlock>>();

        // Each page has exactly one image, so there's no multi-image deadlock
        // risk.
        let mem_limiter = ocr_opts
            .llm_opts
            .driver_opts
            .page_memory_limit
            .as_ref()
            .map(|ml| ml.to_mem_limiter(Some(MEM_PERMIT_DEADLOCK_TIMEOUT)))
            .unwrap_or_else(MemLimiter::unlimited);

        let (chat_queue, worker) = create_chat_work_queue(
            concurrency_limit,
            prompt,
            ocr_opts.model.clone(),
            ocr_opts.llm_opts.clone(),
            mem_limiter,
        )
        .await?;

        let client = NativeDriver::new().await?.client;

        Ok((
            Arc::new(Self {
                chat_queue,
                client,
                ocr_opts,
            }),
            worker,
        ))
    }

    /// Fill in `figure_text` for each image block, by cropping it out of the
    /// page and running plain OCR on it. Problems are reported in `errors`,
    /// because we still have the rest of the page.
    ///
    /// We OCR figures one at a time, while holding this page's slot, so we
    /// never have more requests in flight than pages.
    //
    // TODO: A failed figure should probably make the page `Incomplete`, not
    // `Ok`. That needs a completeness flag on `OcrPageOutput` which
    // `SplitPagesOcrEngine` takes into account.
    async fn ocr_figures(
        &self,
        image: &ImageFile,
        blocks: &mut [OvisBlock],
        token_usage: &mut Option<TokenUsage>,
        errors: &mut Vec<String>,
    ) {
        let figure_idxs = (0..blocks.len())
            .filter(|&i| blocks[i].category.is_image())
            .collect::<Vec<_>>();
        if figure_idxs.is_empty() {
            return;
        }

        let image = image.clone();
        let boxes = figure_idxs
            .iter()
            .map(|&i| blocks[i].bbox)
            .collect::<Vec<_>>();
        let crops =
            match spawn_blocking_propagating_panics(move || crop_regions(&image, &boxes))
                .await
            {
                Ok(crops) => crops,
                Err(err) => {
                    errors.push(format!("could not crop figures: {err:#}"));
                    return;
                }
            };

        for (idx, crop) in figure_idxs.into_iter().zip(crops) {
            let Some(png) = crop else {
                continue;
            };
            // To support an `--ocr-image-output=data_url` mode, we could keep
            // `png` here and output it as a `data:image/png;base64,...` URL,
            // instead of (or as well as) the OCRed text.
            match self.plain_ocr(&png).await {
                Ok((text, usage)) => {
                    blocks[idx].figure_text = Some(text);
                    if let Some(usage) = usage {
                        *token_usage.get_or_insert_default() += usage;
                    }
                }
                Err(err) => errors.push(format!(
                    "could not OCR {:?} at {:?}: {err:#}",
                    blocks[idx].category, blocks[idx].bbox
                )),
            }
        }
    }

    /// Run OvisOCR3's plain OCR prompt on a PNG image.
    #[instrument(level = "debug", skip_all)]
    async fn plain_ocr(&self, png: &[u8]) -> Result<(String, Option<TokenUsage>)> {
        let req = ChatRequest {
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: MessageContent::from_parts(vec![
                    ContentPart::from_binary_base64(
                        "image/png".to_owned(),
                        Arc::from(BASE64_STANDARD.encode(png)),
                        None,
                    ),
                    ContentPart::Text(PLAIN_OCR_PROMPT.to_owned()),
                ]),
                options: None,
            }],
            ..ChatRequest::default()
        };
        let llm_opts = &self.ocr_opts.llm_opts;
        let opts = ChatOptions {
            temperature: llm_opts.temperature.map(f64::from),
            max_tokens: Some(
                llm_opts
                    .max_completion_tokens
                    .map_or(FIGURE_MAX_COMPLETION_TOKENS, |max| {
                        max.min(FIGURE_MAX_COMPLETION_TOKENS)
                    }),
            ),
            top_p: llm_opts.top_p.map(f64::from),
            ..ChatOptions::default()
        };

        let (client, model, req, opts) =
            (&self.client, self.ocr_opts.model.as_str(), &req, &opts);
        let timeout = llm_opts.llm_timeout_duration();
        let chat_res = retry_with_backoff(DEFAULT_JITTER, move || async move {
            retry_result_ok(try_potentially_transient!(
                client
                    .exec_chat(model, req.clone(), Some(opts))
                    .with_timeout(timeout)
                    .await
            ))
        })
        .await
        .into_result()?;

        let text =
            clean_truncated_repeats(chat_res.first_text().unwrap_or_default().trim());
        let token_usage = if let Usage {
            prompt_tokens: Some(prompt_tokens),
            completion_tokens: Some(completion_tokens),
            ..
        } = chat_res.usage
        {
            Some(TokenUsage {
                prompt_tokens: u64::try_from(prompt_tokens).unwrap_or_default(),
                completion_tokens: u64::try_from(completion_tokens).unwrap_or_default(),
            })
        } else {
            None
        };
        Ok((text, token_usage))
    }
}

#[async_trait]
impl OcrPageEngine for OvisOcrPageEngine {
    #[instrument(level = "debug", skip_all, fields(id = %input.id, page = %input.page_idx))]
    async fn ocr_page(&self, input: OcrPageInput) -> Result<OcrPageOutput> {
        let chat_handle = self.chat_queue.handle();

        let mut template_bindings = Map::new();
        template_bindings.insert(
            "page_data_url".to_string(),
            Value::String(input.image.to_url()),
        );
        let work_input = WorkInput {
            id: Value::Array(vec![
                input.id.clone(),
                Value::Number(input.page_idx.into()),
            ]),
            skip_processing: None,
            passthrough_data: None,
            data: ChatInput { template_bindings },
        };
        let chat_output = chat_handle.process_blocking(work_input).await?;
        let mut errors = chat_output.errors;
        let mut token_usage = chat_output.token_usage;
        let blocks = serde_json::from_value::<Option<Vec<OvisBlock>>>(
            chat_output.data.response.unwrap_or_default(),
        )
        .context("failed to parse OvisOCR response")?;

        let text = match blocks {
            Some(mut blocks) => {
                self.ocr_figures(
                    &input.image,
                    &mut blocks,
                    &mut token_usage,
                    &mut errors,
                )
                .await;
                Some(blocks_to_text(&blocks))
            }
            // TODO: Fall back to something else here. We decode greedily, so
            // when the structured pass loops until it runs out of tokens, the
            // chat queue's retries just repeat the same loop. Other LLM engines
            // usually escape a loop by retrying, so this costs us extra. We
            // could retry with different sampler settings, or call the plain
            // OCR or Markdown prompts directly, sharing code with
            // `plain_ocr` and `clean_truncated_repeats`.
            None => None,
        };

        Ok(OcrPageOutput {
            text,
            errors,
            analysis: None,
            // We have no cost data for self-hosted models.
            estimated_cost: chat_output.estimated_cost,
            token_usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prompt_parses() {
        let prompt = from_toml_str::<ChatPrompt>(OVIS_OCR_PROMPT).unwrap();
        assert_eq!(prompt.messages.len(), 1);
    }

    #[test]
    fn test_schema_is_simple_array() {
        let Schema::JsonValue(schema) = Schema::from_type::<Vec<OvisBlock>>() else {
            panic!("expected JsonValue schema");
        };
        assert_eq!(schema["type"], "array");
        let properties = schema["items"]["properties"].as_object().unwrap();
        assert_eq!(
            properties.keys().collect::<Vec<_>>(),
            vec!["bbox", "category", "content"]
        );
        let category = &properties["category"];
        assert_eq!(category["enum"].as_array().unwrap().len(), 24);
        assert!(category.get("oneOf").is_none());
    }

    #[test]
    fn test_blocks_to_text() {
        let mut blocks: Vec<OvisBlock> = serde_json::from_value(json!([
            {"bbox": [0, 0, 10, 10], "category": "doc_title", "content": "# Title"},
            {"bbox": [0, 10, 10, 20], "category": "image", "content": "<img src=\"images/bbox_0_10_10_20.jpg\" />"},
            {"bbox": [0, 20, 10, 30], "category": "chart", "content": "<img src=\"images/bbox_0_20_10_30.jpg\" />"},
            {"bbox": [0, 30, 10, 40], "category": "text", "content": " Body. "},
        ]))
        .unwrap();
        blocks[2].figure_text = Some("Sales 42%".to_owned());
        assert_eq!(blocks_to_text(&blocks), "# Title\n\nSales 42%\n\nBody.");
    }

    #[test]
    fn test_clean_table_breaks() {
        let html =
            r#"<table border=1><tr><td>\nA\n</td><td colspan="2">B\nC</td></tr></table>"#;
        assert_eq!(
            clean_table_breaks(html),
            r#"<table border=1><tr><td>A</td><td colspan="2">B<br>C</td></tr></table>"#
        );
    }

    #[test]
    fn test_clean_truncated_repeats() {
        // Short text is left alone, even if it repeats.
        let short = "ab".repeat(100);
        assert_eq!(clean_truncated_repeats(&short), short);

        // A long runaway tail is collapsed to one copy of the repeated unit.
        let prefix = "x".repeat(10) + &"Lorem ipsum dolor sit amet. ".repeat(300);
        let looping = format!("{prefix}END{}", "0".repeat(5000));
        assert_eq!(clean_truncated_repeats(&looping), format!("{prefix}END0"));

        // Long text without a repeated tail is left alone.
        let normal = (0..2000).map(|i| format!("{i} ")).collect::<String>();
        assert_eq!(clean_truncated_repeats(&normal), normal);
    }

    #[test]
    fn test_crop_regions_handles_misnamed_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("really_a_jpeg.png");
        image::RgbImage::new(100, 50)
            .save_with_format(&path, ImageFormat::Jpeg)
            .unwrap();
        let image = ImageFile::from_path(&path).unwrap();
        assert_eq!(image.mime_type(), "image/jpeg");
        let crops = crop_regions(&image, &[[0, 0, 500, 1000], [0, 0, 1, 1]]).unwrap();
        assert!(crops[0].is_some());
        assert!(crops[1].is_none());
    }

    #[test]
    fn test_bbox_to_pixels() {
        assert_eq!(
            bbox_to_pixels([100, 200, 600, 700], 1000, 2000),
            Some((100, 400, 500, 1000))
        );
        // Out-of-range coordinates are clamped to the image.
        assert_eq!(
            bbox_to_pixels([0, 0, 1500, 1000], 200, 100),
            Some((0, 0, 200, 100))
        );
        // Tiny and inverted boxes are skipped.
        assert_eq!(bbox_to_pixels([0, 0, 3, 3], 1000, 1000), None);
        assert_eq!(bbox_to_pixels([500, 500, 100, 100], 1000, 1000), None);
        // Icon-sized boxes are skipped, but wide single-line boxes are kept.
        assert_eq!(bbox_to_pixels([0, 0, 25, 20], 1000, 1000), None);
        assert_eq!(
            bbox_to_pixels([0, 0, 200, 20], 1000, 1000),
            Some((0, 0, 200, 20))
        );
        // Very thin boxes are skipped, even if they're long.
        assert_eq!(bbox_to_pixels([0, 0, 900, 5], 1000, 1000), None);
    }
}
