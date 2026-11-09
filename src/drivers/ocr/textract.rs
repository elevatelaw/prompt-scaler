//! OCR using AWS Textract.

use std::{
    collections::{HashMap, HashSet},
    env,
    sync::Arc,
};

use aws_sdk_textract::{
    operation::{
        get_document_analysis::{GetDocumentAnalysisError, GetDocumentAnalysisOutput},
        start_document_analysis::{
            StartDocumentAnalysisError, StartDocumentAnalysisOutput,
        },
    },
    primitives::Blob,
    types::{
        Block, BlockType, DocumentLocation, FeatureType, JobStatus, RelationshipType,
        S3Object,
    },
};
use leaky_bucket::RateLimiter;
use tokio::time::{Duration, sleep};
use uuid::Uuid;

use super::{
    file::OcrFileEngine,
    page::{OcrPageEngine, OcrPageInput, OcrPageOutput},
};
use crate::{
    async_utils::JoinWorker,
    aws::load_aws_config,
    cmd::ocr::OcrOpts,
    drivers::DriverOpts,
    images::ImageEncoding,
    mem_limit::{MEM_PERMIT_DEADLOCK_TIMEOUT, MemLimiter},
    prelude::*,
    queues::{
        ocr::{OcrInput, OcrOutput},
        work::{WorkInput, WorkOutput, WorkStatus},
    },
    rate_limit::{RateLimit, RateLimitPeriod},
    retry::{
        DEFAULT_JITTER, IsKnownTransient, retry_result_ok, retry_with_backoff,
        try_potentially_transient,
    },
};

/// Our estimated page cost, based on the options we use.
const ESTIMATED_PAGE_COST: f64 = 0.004;

/// Create a Textract client.
async fn create_textract_client() -> Result<aws_sdk_textract::Client> {
    let config = load_aws_config().await?;
    Ok(aws_sdk_textract::Client::new(&config))
}

/// Create a rate limiter, defaulting as best we can.
fn create_rate_limiter(
    concurrency_limit: usize,
    driver_opts: &DriverOpts,
) -> RateLimiter {
    // If we don't have a rate limit, set one based on the concurrency limit.
    //
    // TODO: FUTURE BREAKING: We may want to remove the default rate limit, but
    // that would be a breaking change.
    let rate_limit = driver_opts
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

    /// A memory limiter for image loading.
    mem_limiter: MemLimiter,
}

impl TextractOcrPageEngine {
    /// Create a new `textract` engine.
    #[allow(clippy::new_ret_no_self)]
    pub async fn new(
        concurrency_limit: usize,
        ocr_opts: Arc<OcrOpts>,
    ) -> Result<(Arc<dyn OcrPageEngine>, JoinWorker)> {
        let client = create_textract_client().await?;
        let rate_limiter =
            create_rate_limiter(concurrency_limit, &ocr_opts.llm_opts.driver_opts);
        let mem_limiter = ocr_opts
            .llm_opts
            .driver_opts
            .page_memory_limit
            .as_ref()
            .map(|ml| ml.to_mem_limiter(Some(MEM_PERMIT_DEADLOCK_TIMEOUT)))
            .unwrap_or_else(MemLimiter::unlimited);

        Ok((
            Arc::new(Self {
                client,
                rate_limiter,
                mem_limiter,
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

        // Load the image data from disk.
        let image_data = input
            .image
            .load(ImageEncoding::Binary, &self.mem_limiter)
            .await?;

        // Build our document.
        let document = aws_sdk_textract::types::Document::builder()
            .bytes(Blob::new(image_data.data().to_vec()))
            .build();

        // Use the Textract API to process the image.
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
                let mut output = OutputState::new(false);
                output.write_blocks(document.blocks())?;
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
    /// OCR options.
    ocr_opts: Arc<OcrOpts>,

    /// AWS Textract client.
    client: aws_sdk_textract::Client,

    /// A rate limiter to avoid hitting API limits.
    rate_limiter: RateLimiter,
}

impl TextractOcrFileEngine {
    /// Create a new `textract` engine.
    #[allow(clippy::new_ret_no_self)]
    pub async fn new(
        concurrency_limit: usize,
        ocr_opts: Arc<OcrOpts>,
    ) -> Result<(Arc<dyn OcrFileEngine>, JoinWorker)> {
        let client = create_textract_client().await?;
        let rate_limiter =
            create_rate_limiter(concurrency_limit, &ocr_opts.llm_opts.driver_opts);

        if ocr_opts.page_iter_opts.max_pages.is_some()
            || ocr_opts.page_iter_opts.rasterize
        {
            return Err(anyhow!(
                "textract-async does not work with --max-pages or --rasterize"
            ));
        }

        Ok((
            Arc::new(Self {
                ocr_opts,
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
        let document_location = DocumentLocation::builder()
            .s3_object(
                S3Object::builder()
                    .bucket(bucket.clone())
                    .name(key.clone())
                    .build(),
            )
            .build();
        let idempotency_id = Uuid::new_v4();
        let start_response: StartDocumentAnalysisOutput =
            retry_with_backoff(DEFAULT_JITTER, move || {
                let document_location = document_location.clone();
                async move {
                    retry_result_ok(try_potentially_transient!(
                        self.client
                            .start_document_analysis()
                            .document_location(document_location)
                            .client_request_token(idempotency_id)
                            .set_feature_types(Some(vec![FeatureType::Layout]))
                            .send()
                            .await
                    ))
                }
            })
            .await
            .into_result()?;

        let job_id = start_response
            .job_id()
            .ok_or_else(|| anyhow!("No job ID returned from start_document_analysis"))?;

        // Poll for results.
        //
        // TODO: Integrate crate::retry support here.
        let max_retries = 1440; // 2 hours max with 5-second intervals
        let mut retry_count = 0;
        let (status, response) = loop {
            let response = retry_with_backoff(DEFAULT_JITTER, move || async move {
                retry_result_ok(try_potentially_transient!(
                    self.client
                        .get_document_analysis()
                        .job_id(job_id)
                        .send()
                        .await
                ))
            })
            .await
            .into_result()?;

            if let Some(status) = response.job_status() {
                match status {
                    JobStatus::InProgress => {
                        trace!("Job {} still in progress", job_id);
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

        // Calculate estimated cost based on pages.
        let page_count = response
            .document_metadata()
            .and_then(|metadata| metadata.pages())
            .unwrap_or(0);

        // Create our output state and get our text, fetching the rest of the
        // result pages.
        let mut output = OutputState::new(self.ocr_opts.include_page_breaks);
        write_all_result_pages(&mut output, response, |next_token| async move {
            retry_with_backoff(DEFAULT_JITTER, move || {
                let next_token = next_token.clone();
                async move {
                    retry_result_ok(try_potentially_transient!(
                        self.client
                            .get_document_analysis()
                            .job_id(job_id)
                            .next_token(next_token)
                            .send()
                            .await
                    ))
                }
            })
            .await
            .into_result()
        })
        .await?;
        let text = output.output;
        debug!(%text, "Extracted text");
        let estimated_cost = ESTIMATED_PAGE_COST * f64::from(page_count);

        Ok(WorkOutput {
            id,
            status,
            errors: vec![],
            estimated_cost: Some(estimated_cost),
            token_usage: None,
            passthrough_data: ocr_input.passthrough_data,
            data: OcrOutput {
                path: ocr_input.data.path,
                text: Some(text),
                page_count: usize::try_from(page_count).ok(),
                analysis: None,
            },
        })
    }
}

impl IsKnownTransient for StartDocumentAnalysisError {
    fn is_known_transient(&self) -> bool {
        matches!(
            self,
            StartDocumentAnalysisError::LimitExceededException(_)
                | StartDocumentAnalysisError::ProvisionedThroughputExceededException(_)
                | StartDocumentAnalysisError::ThrottlingException(_)
        )
    }
}

impl IsKnownTransient for GetDocumentAnalysisError {
    fn is_known_transient(&self) -> bool {
        matches!(
            self,
            GetDocumentAnalysisError::InternalServerError(_)
                | GetDocumentAnalysisError::ProvisionedThroughputExceededException(_)
                | GetDocumentAnalysisError::ThrottlingException(_)
        )
    }
}

/// Write the text of a finished Textract job, starting with `first_page` of its
/// results and calling `fetch_page` with each `next_token` until there are no
/// more result pages.
///
/// Each result page holds at most 1,000 blocks, and a long document can have
/// hundreds of thousands, so we write each document page as soon as we have
/// all of its blocks instead of keeping them all.
///
/// ASSUMPTION: Textract returns all of a document page's blocks before any
/// blocks for the next page, and blocks only refer to blocks on their own page.
/// Textract's documentation doesn't promise this, but it holds for real
/// output, and AWS's own response parser (`amazon-textract-response-parser`)
/// also relies on blocks being grouped by page. If the assumption ever fails,
/// we fail this document with an error explaining why.
async fn write_all_result_pages<F, Fut>(
    output: &mut OutputState,
    first_page: GetDocumentAnalysisOutput,
    mut fetch_page: F,
) -> Result<()>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<GetDocumentAnalysisOutput>>,
{
    let mut page_blocks: Vec<Block> = vec![];
    let mut result_page = first_page;
    loop {
        for block in result_page.blocks.unwrap_or_default() {
            let current_page = page_blocks.last().and_then(|b| b.page);
            if let (Some(current_page), Some(page)) = (current_page, block.page)
                && page != current_page
            {
                if page < current_page {
                    return Err(anyhow!(
                        "Textract returned a block for page {} after blocks for page {}, \
                         but textract-async needs each page's blocks together to write \
                         text a page at a time",
                        page,
                        current_page
                    ));
                }
                write_page(output, &page_blocks)?;
                page_blocks.clear();
            }
            page_blocks.push(block);
        }
        let Some(next_token) = result_page.next_token else {
            break;
        };
        result_page = fetch_page(next_token).await?;
    }
    write_page(output, &page_blocks)
}

/// Write the blocks of one document page, explaining the likely cause if a
/// block refers to a block we don't have.
fn write_page(output: &mut OutputState, page_blocks: &[Block]) -> Result<()> {
    output.write_blocks(page_blocks).with_context(|| {
        let page = page_blocks.first().and_then(|block| block.page);
        format!(
            "Failed to write Textract page {}. If a block refers to a block on \
             another page, textract-async can't write text a page at a time",
            page.map_or_else(|| "with no page number".to_owned(), |p| p.to_string())
        )
    })
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
struct OutputState {
    /// Should we include page breaks between pages?
    include_page_breaks: bool,

    /// Is this the first page?
    first_page: bool,

    /// The output string we're building.
    output: String,

    /// Should we output debug information?
    debug: bool,
}

/// The blocks passed to one [`OutputState::write_blocks`] call.
struct BlockIndex<'a> {
    /// Blocks by ID. Used to look up child blocks and recurse over them.
    blocks_by_id: HashMap<&'a str, &'a Block>,

    /// The set of already printed blocks, to prevent printing blocks twice.
    printed_block_ids: HashSet<&'a str>,
}

impl OutputState {
    /// Create a new output state.
    fn new(include_page_breaks: bool) -> Self {
        let debug = env::var("TEXTRACT_DEBUG").is_ok();

        Self {
            include_page_breaks,
            first_page: true,
            output: String::new(),
            debug,
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

    /// Write the text of some blocks of an analyzed document. Blocks may only
    /// refer to other blocks in the same call, so a document may be written
    /// all at once or a page at a time.
    fn write_blocks(&mut self, blocks: &[Block]) -> Result<()> {
        // Build a table of blocks by ID.
        let mut index = BlockIndex {
            blocks_by_id: HashMap::new(),
            printed_block_ids: HashSet::new(),
        };
        for block in blocks {
            let Some(block_id) = block.id() else {
                continue;
            };
            index.blocks_by_id.insert(block_id, block);
        }

        // Iterate over layout blocks and extract their child text.
        for block in blocks {
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
            self.write_block(&mut index, block, false)?;
            if self.bytes_written() > bytes_written {
                // We wrote something, so add a newline.
                self.write_text("\n");
            }
        }
        Ok(())
    }

    /// Write a block recursively.
    fn write_block<'a>(
        &mut self,
        index: &mut BlockIndex<'a>,
        block: &'a Block,
        printed_parent: bool,
    ) -> Result<()> {
        // Check to make sure we haven't already printed this block.
        if let Some(id) = block.id()
            && !index.printed_block_ids.insert(id)
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
                    if let Some(child_block) = index.blocks_by_id.get(&id[..]) {
                        self.write_block(index, child_block, printed_self)?;
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

#[cfg(test)]
mod tests {
    use aws_sdk_textract::types::Relationship;

    use super::*;

    /// Build a block with an ID, a type, a page number, optional text and
    /// optional children.
    fn block(
        id: &str,
        block_type: BlockType,
        page: Option<i32>,
        text: Option<&str>,
        child_ids: &[&str],
    ) -> Block {
        let mut builder = Block::builder()
            .id(id)
            .block_type(block_type)
            .set_page(page)
            .set_text(text.map(str::to_owned));
        if !child_ids.is_empty() {
            builder = builder.relationships(
                Relationship::builder()
                    .r#type(RelationshipType::Child)
                    .set_ids(Some(child_ids.iter().map(|id| id.to_string()).collect()))
                    .build(),
            );
        }
        builder.build()
    }

    /// Build one result page of a finished job's `get_document_analysis`
    /// output.
    fn result_page(
        blocks: Vec<Block>,
        next_token: Option<&str>,
    ) -> GetDocumentAnalysisOutput {
        GetDocumentAnalysisOutput::builder()
            .job_status(JobStatus::Succeeded)
            .set_blocks(Some(blocks))
            .set_next_token(next_token.map(str::to_owned))
            .build()
    }

    /// Run [`write_all_result_pages`] over `first` and then `later` result
    /// pages, checking that each `next_token` asks for the next one.
    async fn write_result_pages(
        first: GetDocumentAnalysisOutput,
        later: Vec<(&str, GetDocumentAnalysisOutput)>,
    ) -> Result<String> {
        let mut later = later.into_iter();
        let mut output = OutputState::new(true);
        write_all_result_pages(&mut output, first, |token| {
            let (expected_token, page) = later.next().expect("too many fetches");
            assert_eq!(token, expected_token);
            async move { Ok(page) }
        })
        .await?;
        assert!(later.next().is_none(), "not every result page was fetched");
        Ok(output.output)
    }

    /// A two-page document split over three result pages, in the order
    /// Textract returns blocks: each page, then its lines, then its layout
    /// blocks. The first page's layout block is on a later result page than
    /// its line.
    fn two_page_document() -> Vec<Block> {
        vec![
            block("p1", BlockType::Page, Some(1), None, &["l1", "lt1"]),
            block("l1", BlockType::Line, Some(1), Some("first line"), &[]),
            block("lt1", BlockType::LayoutText, Some(1), None, &["l1"]),
            block("p2", BlockType::Page, Some(2), None, &["l2", "lt2"]),
            block("l2", BlockType::Line, Some(2), Some("second line"), &[]),
            block("lt2", BlockType::LayoutText, Some(2), None, &["l2"]),
        ]
    }

    #[tokio::test]
    async fn writes_every_result_page() {
        let blocks = two_page_document();
        let text = write_result_pages(
            result_page(blocks[0..2].to_vec(), Some("token-2")),
            vec![
                (
                    "token-2",
                    result_page(blocks[2..4].to_vec(), Some("token-3")),
                ),
                ("token-3", result_page(blocks[4..].to_vec(), None)),
            ],
        )
        .await
        .unwrap();
        assert_eq!(text, "first line\n\n\x0Csecond line\n\n");

        // Writing a page at a time gives the same text as writing the whole
        // document at once.
        let mut whole = OutputState::new(true);
        whole.write_blocks(&blocks).unwrap();
        assert_eq!(text, whole.output);
    }

    #[tokio::test]
    async fn first_result_page_alone_is_missing_text() {
        let blocks = two_page_document();
        let text = write_result_pages(result_page(blocks[0..2].to_vec(), None), vec![])
            .await
            .unwrap();
        assert_eq!(text, "");
    }

    #[tokio::test]
    async fn allows_result_pages_without_blocks() {
        let first = GetDocumentAnalysisOutput::builder()
            .job_status(JobStatus::Succeeded)
            .next_token("token-2")
            .build();
        let text = write_result_pages(
            first,
            vec![("token-2", result_page(two_page_document(), None))],
        )
        .await
        .unwrap();
        assert_eq!(text, "first line\n\n\x0Csecond line\n\n");
    }

    #[tokio::test]
    async fn writes_blocks_without_page_numbers_together() {
        let text = write_result_pages(
            result_page(
                vec![block("l1", BlockType::Line, None, Some("a line"), &[])],
                Some("token-2"),
            ),
            vec![(
                "token-2",
                result_page(
                    vec![block("lt1", BlockType::LayoutText, None, None, &["l1"])],
                    None,
                ),
            )],
        )
        .await
        .unwrap();
        assert_eq!(text, "a line\n\n");
    }

    #[tokio::test]
    async fn fails_if_a_result_page_fails() {
        let mut output = OutputState::new(true);
        let err = write_all_result_pages(
            &mut output,
            result_page(two_page_document(), Some("token-2")),
            |_| async { Err(anyhow!("simulated fetch failure")) },
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "simulated fetch failure");
    }

    #[tokio::test]
    async fn fails_on_pages_out_of_order() {
        let err = write_result_pages(
            result_page(
                vec![
                    block("p2", BlockType::Page, Some(2), None, &[]),
                    block("p1", BlockType::Page, Some(1), None, &[]),
                ],
                None,
            ),
            vec![],
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Textract returned a block for page 1 after blocks for page 2, but \
             textract-async needs each page's blocks together to write text a page \
             at a time"
        );
    }

    #[tokio::test]
    async fn fails_on_children_from_another_page() {
        let err = write_result_pages(
            result_page(
                vec![
                    block("l1", BlockType::Line, Some(1), Some("a line"), &[]),
                    block("lt2", BlockType::LayoutText, Some(2), None, &["l1"]),
                ],
                None,
            ),
            vec![],
        )
        .await
        .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "Failed to write Textract page 2. If a block refers to a block on another \
             page, textract-async can't write text a page at a time: Textract child \
             block l1 not found"
        );
    }
}
