//! The `ocr` subcommand.

use futures::StreamExt;

use crate::{
    page_iter::PageIterOptions,
    prelude::*,
    queues::{
        ocr::{OcrInput, OcrOutput, OcrStreamInfo, default_ocr_prompt, ocr_files},
        work::{WorkInput as _, WorkOutput as _},
    },
};

/// The `ocr` subcommand.
#[instrument(level = "debug", skip_all)]
pub async fn cmd_ocr(
    input_path: Option<&Path>,
    page_iter_opts: &PageIterOptions,
    job_count: usize,
    model: &str,
    allowed_failure_rate: f32,
    output_path: Option<&Path>,
) -> Result<()> {
    // Get our OCR prompt.
    let prompt = default_ocr_prompt();

    // Open up our input stream and parse into records.
    let input = OcrInput::read_stream(input_path).await?;

    let OcrStreamInfo { stream, queue } = ocr_files(
        input,
        page_iter_opts.to_owned(),
        job_count,
        prompt,
        model.to_owned(),
    )
    .await?;
    let output = stream.buffered(job_count).boxed();

    OcrOutput::write_stream(output_path, output, allowed_failure_rate).await?;

    queue.close().await
}
