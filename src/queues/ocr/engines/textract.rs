//! OCR using AWS Textract.

use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::Arc;

use aws_sdk_textract::operation::analyze_document::AnalyzeDocumentOutput;
use aws_sdk_textract::types::{Block, FeatureType, RelationshipType};
use aws_sdk_textract::{primitives::Blob, types::BlockType};
use leaky_bucket::RateLimiter;

use crate::aws::load_aws_config;
use crate::drivers::LlmOpts;
use crate::prelude::*;

use crate::async_utils::JoinWorker;
use crate::rate_limit::{RateLimit, RateLimitPeriod};

use super::page::{OcrPageEngine, OcrPageInput, OcrPageOutput};

/// Our estimated page cost, based on the options we use.
const ESTIMATED_PAGE_COST: f64 = 0.004;

/// OCR engine wrapping the AWS Textract API.
pub struct TextractOcrEngine {
    /// AWS Textract client.
    client: aws_sdk_textract::Client,

    /// A rate limiter to avoid hitting API limits.
    rate_limiter: RateLimiter,

    /// Includes layout debug information in the output.
    debug: bool,
}

impl TextractOcrEngine {
    /// Create a new `textract` engine.
    #[allow(clippy::new_ret_no_self)]
    pub async fn new(
        concurrency_limit: usize,
        llm_opts: &LlmOpts,
    ) -> Result<(Arc<dyn OcrPageEngine>, JoinWorker)> {
        let config = load_aws_config().await?;
        let client = aws_sdk_textract::Client::new(&config);

        // If we don't have a rate limit, set one based on the concurrency
        // limit.
        //
        // TODO: We may want to remove the default rate limit, but that would be
        // a breaking change.
        let rate_limit = llm_opts.rate_limit.clone().unwrap_or_else(|| {
            RateLimit::new(concurrency_limit, RateLimitPeriod::Second)
        });
        let rate_limiter = rate_limit.to_rate_limiter();

        let debug = env::var("TEXTRACT_DEBUG").is_ok();
        Ok((
            Arc::new(Self {
                client,
                rate_limiter,
                debug,
            }),
            JoinWorker::noop(),
        ))
    }
}

#[async_trait]
impl OcrPageEngine for TextractOcrEngine {
    #[instrument(level = "debug", skip_all, fields(id = %input.id, page = %input.page_idx))]
    async fn ocr_page(&self, input: OcrPageInput) -> Result<OcrPageOutput> {
        // Rate limit the request.
        self.rate_limiter.acquire_one().await;

        // Keep track of errors.
        let mut errors = Vec::new();

        // TODO: We may need to convert GIF and WEBP to a supported format.

        // Build our document.
        let document = aws_sdk_textract::types::Document::builder()
            .bytes(Blob::new(input.page.data.clone()))
            .build();

        // Use the Textract API to process the image.
        //
        // TODO: Retry non-fatal errors as we discover them. See the
        // LLM chat client for details.
        let response = self
            .client
            .analyze_document()
            .document(document)
            .set_feature_types(Some(vec![FeatureType::Layout]))
            .send()
            .await;
        match response {
            Err(e) => {
                let err = format!("AWS Textract error: {e:?}");
                error!("{err}");
                errors.push(err);
                return Ok(OcrPageOutput {
                    text: None,
                    errors,
                    analysis: None,
                    estimated_cost: None,
                    token_usage: None,
                });
            }
            Ok(document) => {
                trace!("Document response: {document:#?}");

                // Build a table of blocks by ID.
                let mut blocks_by_id = HashMap::new();
                for block in document.blocks() {
                    let Some(block_id) = block.id() else {
                        continue;
                    };
                    blocks_by_id.insert(block_id, block);
                }

                // Create our output state and get our text.
                let mut output = OutputState::new(self.debug, blocks_by_id);
                output.write_analyzed_document(&document)?;
                let text = output.output;
                debug!(%text, "Extraced text");
                Ok(OcrPageOutput {
                    text: Some(text),
                    errors,
                    analysis: None,
                    estimated_cost: Some(ESTIMATED_PAGE_COST),
                    token_usage: None,
                })
            }
        }
    }
}

/// Our output state.
///
/// This is a slightly _ad hoc_ output system that helps handle recursion and
/// de-duplication, making sure we get all the relevant text, and get it only
/// once.
#[derive(Debug)]
struct OutputState<'a> {
    /// The output string we're building.
    output: String,

    /// Should we output debug information?
    debug: bool,

    /// Blocks by ID. Used to look up child blocks and recurse over them.
    blocks_by_id: HashMap<&'a str, &'a Block>,

    /// The set of already printed blocks, to prevent printing blocks twice.
    printed_block_ids: HashSet<&'a str>,
}

impl<'a> OutputState<'a> {
    /// Create a new output state.
    fn new(debug: bool, blocks_by_id: HashMap<&'a str, &'a Block>) -> Self {
        Self {
            output: String::new(),
            debug,
            blocks_by_id,
            printed_block_ids: HashSet::new(),
        }
    }

    /// How many bytes have we written?
    fn bytes_written(&self) -> usize {
        self.output.len()
    }

    /// Write some text to the output.
    fn write_text(&mut self, text: &str) {
        self.output.push_str(text);
    }

    /// Write an analyzed document.
    fn write_analyzed_document<'d: 'a>(
        &mut self,
        document: &'d AnalyzeDocumentOutput,
    ) -> Result<()> {
        // Iterate over layout blocks and extract their child text.
        for block in document.blocks() {
            // We only want layout blocks with text, which should
            // contain all the text and come in a reasonable order.
            trace!(?block, "Textract layout block");
            let Some(block_type) = block.block_type() else {
                continue;
            };
            if !block_type.as_str().starts_with("LAYOUT_") {
                continue;
            }

            // Print the block.
            let bytes_written = self.bytes_written();
            self.write_block(block, false)?;
            if self.bytes_written() > bytes_written {
                // We wrote something, so add a newline.
                self.write_text("\n");
            }
        }
        Ok(())
    }

    /// Write a block recursively.
    fn write_block(&mut self, block: &'a Block, printed_parent: bool) -> Result<()> {
        // Check to make sure we haven't already printed this block.
        if let Some(id) = block.id()
            && !self.printed_block_ids.insert(id)
        {
            return Ok(());
        }

        // If we haven't printed a parent already, print this block.
        let mut printed_self = false;
        if !printed_parent {
            self.block_start(block);
            if let Some(text) = block.text() {
                self.output.push_str(text);
                match block.block_type() {
                    Some(BlockType::Line) => {
                        self.output.push('\n');
                    }
                    Some(BlockType::Word) => {
                        self.output.push(' ');
                    }
                    _ => {}
                }
                printed_self = true;
            }
        }

        // Recurse into the children.
        for relationship in block.relationships() {
            if relationship.r#type() == Some(&RelationshipType::Child) {
                for id in relationship.ids() {
                    if let Some(child_block) = self.blocks_by_id.get(&id[..]) {
                        self.write_block(child_block, printed_self)?;
                    } else {
                        return Err(anyhow!("Textract child block {} not found", id));
                    }
                }
            }
        }

        if !printed_parent {
            self.block_end(block);
        }
        Ok(())
    }

    /// Print block start.
    fn block_start(&mut self, block: &Block) {
        if self.debug {
            self.output.push('<');
            if let Some(block_type) = block.block_type() {
                self.output.push_str(block_type.as_str());
            } else {
                self.output.push_str("UNKNOWN");
            }
            self.output.push_str(" id=\"");
            if let Some(block_id) = block.id() {
                self.output.push_str(block_id);
            } else {
                self.output.push_str("UNKNOWN");
            }
            self.output.push_str("\">");
        }
    }

    /// Print block end.
    fn block_end(&mut self, block: &Block) {
        if self.debug {
            self.output.push_str("</");
            if let Some(block_type) = block.block_type() {
                self.output.push_str(block_type.as_str());
            } else {
                self.output.push_str("UNKNOWN");
            }
            self.output.push('>');
        }
    }
}
