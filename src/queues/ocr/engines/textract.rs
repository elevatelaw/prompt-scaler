//! OCR using AWS Textract.

use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::Arc;

use aws_sdk_textract::types::{
    Block, DocumentLocation, FeatureType, JobStatus, RelationshipType, S3Object,
};
use aws_sdk_textract::{primitives::Blob, types::BlockType};
use leaky_bucket::RateLimiter;
use tokio::time::{Duration, sleep};

use crate::aws::load_aws_config;
use crate::drivers::LlmOpts;
use crate::page_iter::PageIterOptions;
use crate::prelude::*;

use crate::async_utils::JoinWorker;
use crate::rate_limit::{RateLimit, RateLimitPeriod};

use super::file::OcrFileEngine;
use super::page::{OcrPageEngine, OcrPageInput, OcrPageOutput};
use crate::queues::ocr::{OcrInput, OcrOutput};
use crate::queues::work::{WorkInput, WorkOutput, WorkStatus};

/// Our estimated page cost, based on the options we use.
const ESTIMATED_PAGE_COST: f64 = 0.004;

/// Create a Textract client.
async fn create_textract_client() -> Result<aws_sdk_textract::Client> {
    let config = load_aws_config().await?;
    Ok(aws_sdk_textract::Client::new(&config))
}

/// Create a rate limiter, defaulting as best we can.
fn create_rate_limiter(concurrency_limit: usize, llm_opts: &LlmOpts) -> RateLimiter {
    // If we don't have a rate limit, set one based on the concurrency
    // limit.
    //
    // TODO: We may want to remove the default rate limit, but that would be
    // a breaking change.
    let rate_limit = llm_opts
        .rate_limit
        .clone()
        .unwrap_or_else(|| RateLimit::new(concurrency_limit, RateLimitPeriod::Second));
    rate_limit.to_rate_limiter()
}

/// OCR engine wrapping the synchronous AWS Textract `analyze_document` API.
///
/// This needs to be a page engine because Textract does not support multi-page
/// PDFs in this mode.
pub struct TextractOcrPageEngine {
    /// AWS Textract client.
    client: aws_sdk_textract::Client,

    /// A rate limiter to avoid hitting API limits.
    rate_limiter: RateLimiter,
}

impl TextractOcrPageEngine {
    /// Create a new `textract` engine.
    #[allow(clippy::new_ret_no_self)]
    pub async fn new(
        concurrency_limit: usize,
        llm_opts: &LlmOpts,
    ) -> Result<(Arc<dyn OcrPageEngine>, JoinWorker)> {
        let client = create_textract_client().await?;
        let rate_limiter = create_rate_limiter(concurrency_limit, llm_opts);

        Ok((
            Arc::new(Self {
                client,
                rate_limiter,
            }),
            JoinWorker::noop(),
        ))
    }
}

#[async_trait]
impl OcrPageEngine for TextractOcrPageEngine {
    #[instrument(level = "debug", skip_all, fields(id = %input.id, page = %input.page_idx))]
    async fn ocr_page(&self, input: OcrPageInput) -> Result<OcrPageOutput> {
        // Rate limit the request.
        self.rate_limiter.acquire_one().await;

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
                return Ok(OcrPageOutput {
                    text: None,
                    errors: vec![format!("AWS Textract error: {e:?}")],
                    analysis: None,
                    estimated_cost: None,
                    token_usage: None,
                });
            }
            Ok(document) => {
                trace!("Document response: {document:#?}");

                // Create our output state and get our text.
                let mut output = OutputState::new(document.blocks(), false);
                output.write_analyzed_document()?;
                let text = output.output;
                debug!(%text, "Extracted text");
                Ok(OcrPageOutput {
                    text: Some(text),
                    errors: vec![],
                    analysis: None,
                    estimated_cost: Some(ESTIMATED_PAGE_COST),
                    token_usage: None,
                })
            }
        }
    }
}

/// OCR engine wrapping the asynchronous AWS Textract `start_document_analysis`
/// and `get_document_analysis` APIs.
///
/// For now, this can only operate on files stored in S3. All passed-in file
/// paths must be valid S3 URIs.
pub struct TextractOcrFileEngine {
    /// Should we include page breaks between pages using a form-feed character?
    include_page_breaks: bool,

    /// AWS Textract client.
    client: aws_sdk_textract::Client,

    /// A rate limiter to avoid hitting API limits.
    rate_limiter: RateLimiter,
}

impl TextractOcrFileEngine {
    /// Create a new `textract` engine.
    #[allow(clippy::new_ret_no_self)]
    pub async fn new(
        page_iter_opts: &PageIterOptions,
        concurrency_limit: usize,
        include_page_breaks: bool,
        llm_opts: &LlmOpts,
    ) -> Result<(Arc<dyn OcrFileEngine>, JoinWorker)> {
        let client = create_textract_client().await?;
        let rate_limiter = create_rate_limiter(concurrency_limit, llm_opts);

        if page_iter_opts.max_pages.is_some() || page_iter_opts.rasterize {
            return Err(anyhow!(
                "textract-async does not work with --max-pages or --rasterize"
            ));
        }

        Ok((
            Arc::new(Self {
                include_page_breaks,
                client,
                rate_limiter,
            }) as Arc<dyn OcrFileEngine>,
            JoinWorker::noop(),
        ))
    }
}

#[async_trait]
impl OcrFileEngine for TextractOcrFileEngine {
    #[instrument(level = "debug", skip_all, fields(id = %ocr_input.id))]
    async fn ocr_file(
        &self,
        ocr_input: WorkInput<OcrInput>,
    ) -> Result<WorkOutput<OcrOutput>> {
        let id = ocr_input.id.clone();

        // Rate limit the request.
        self.rate_limiter.acquire_one().await;

        // Parse the S3 URI.
        let s3_uri = &ocr_input.data.path;
        let (bucket, key) = parse_s3_uri(s3_uri)
            .with_context(|| format!("Failed to parse S3 URI: {}", s3_uri))?;

        // Start document analysis.
        //
        // TODO: Integrate crate::retry support here, and maybe also rate limit
        // error handling for automatic backoff.
        let document_location = DocumentLocation::builder()
            .s3_object(
                S3Object::builder()
                    .bucket(bucket.clone())
                    .name(key.clone())
                    .build(),
            )
            .build();
        let start_response = self
            .client
            .start_document_analysis()
            .document_location(document_location)
            .client_request_token(uuid::Uuid::new_v4())
            .set_feature_types(Some(vec![FeatureType::Layout]))
            .send()
            .await?;

        let job_id = start_response
            .job_id()
            .ok_or_else(|| anyhow!("No job ID returned from start_document_analysis"))?;

        // Poll for results.
        //
        // TODO: Integrate crate::retry support here.
        let max_retries = 60; // 5 minutes max with 5-second intervals
        let mut retry_count = 0;
        let (status, response) = loop {
            let response = self
                .client
                .get_document_analysis()
                .job_id(job_id)
                .send()
                .await?;

            if let Some(status) = response.job_status() {
                match status {
                    JobStatus::InProgress => {
                        debug!("Job {} still in progress", job_id);
                        retry_count += 1;
                        if retry_count >= max_retries {
                            return Err(anyhow!(
                                "Textract job {} timed out after {} retries",
                                job_id,
                                max_retries
                            ));
                        } else {
                            sleep(Duration::from_secs(5)).await;
                            continue;
                        }
                    }
                    JobStatus::Succeeded => {
                        break (WorkStatus::Ok, response);
                    }
                    JobStatus::Failed => {
                        return Err(anyhow!("Textract job {} failed", job_id));
                    }
                    JobStatus::PartialSuccess => {
                        warn!("Textract job {} completed with partial success", job_id);
                        break (WorkStatus::Incomplete, response);
                    }
                    _ => {
                        return Err(anyhow!(
                            "Textract job {} returned unknown status",
                            job_id
                        ));
                    }
                }
            } else {
                return Err(anyhow!("No job status in response for job {}", job_id));
            }
        };

        trace!("Document analysis response: {:#?}", response);

        // Create our output state and get our text.
        let mut output = OutputState::new(response.blocks(), self.include_page_breaks);
        output.write_analyzed_document()?;
        let text = output.output;
        debug!(%text, "Extracted text");

        // Calculate estimated cost based on pages.
        let page_count = response
            .document_metadata()
            .and_then(|metadata| metadata.pages())
            .unwrap_or(0);
        let estimated_cost = ESTIMATED_PAGE_COST * f64::from(page_count);

        Ok(WorkOutput {
            id,
            status,
            errors: vec![],
            estimated_cost: Some(estimated_cost),
            token_usage: None,
            data: OcrOutput {
                path: ocr_input.data.path,
                text: Some(text),
                page_count: usize::try_from(page_count).ok(),
                analysis: None,
            },
        })
    }
}

/// Parse an S3 URI into bucket and key.
fn parse_s3_uri(uri: &str) -> Result<(String, String)> {
    let uri = uri
        .strip_prefix("s3://")
        .ok_or_else(|| anyhow!("S3 URI must start with s3://"))?;
    let parts: Vec<&str> = uri.splitn(2, '/').collect();
    if parts.len() != 2 {
        return Err(anyhow!("S3 URI must be in the format s3://bucket/key"));
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

/// Our output state.
///
/// This is a slightly _ad hoc_ output system that helps handle recursion and
/// de-duplication, making sure we get all the relevant text, and get it only
/// once.
#[derive(Debug)]
struct OutputState<'a> {
    /// Should we include page breaks between pages?
    include_page_breaks: bool,

    /// Is this the first page?
    first_page: bool,

    /// The output string we're building.
    output: String,

    /// Should we output debug information?
    debug: bool,

    /// Our document.
    all_blocks: &'a [Block],

    /// Blocks by ID. Used to look up child blocks and recurse over them.
    blocks_by_id: HashMap<&'a str, &'a Block>,

    /// The set of already printed blocks, to prevent printing blocks twice.
    printed_block_ids: HashSet<&'a str>,
}

impl<'a> OutputState<'a> {
    /// Create a new output state.
    fn new(all_blocks: &'a [Block], include_page_breaks: bool) -> Self {
        // Build a table of blocks by ID.
        let mut blocks_by_id = HashMap::new();
        for block in all_blocks {
            let Some(block_id) = block.id() else {
                continue;
            };
            blocks_by_id.insert(block_id, block);
        }

        let debug = env::var("TEXTRACT_DEBUG").is_ok();

        Self {
            include_page_breaks,
            first_page: true,
            output: String::new(),
            debug,
            all_blocks,
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
    fn write_analyzed_document<'d: 'a>(&mut self) -> Result<()> {
        // Iterate over layout blocks and extract their child text.
        for block in self.all_blocks {
            trace!(?block, "Textract block");
            let Some(block_type) = block.block_type() else {
                continue;
            };

            // Handle page breaks, if requested.
            if self.include_page_breaks && block_type == &BlockType::Page {
                if self.first_page {
                    trace!("No break before first page");
                    self.first_page = false;
                } else {
                    trace!("Inserting page break");
                    if !self.output.ends_with('\n') {
                        self.write_text("\n");
                    }
                    self.write_text("\x0C");
                }
                continue;
            }

            // We only want layout blocks with text, which should
            // contain all the text and come in a reasonable order.
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
