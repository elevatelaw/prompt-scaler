//! The `ocr` subcommand.

use futures::StreamExt;

use crate::{
    async_utils::io::read_json_or_toml,
    page_iter::PageIterOptions,
    prelude::*,
    prompt::ChatPrompt,
    queues::{
        ocr::{
            OcrInput, OcrOutput, OcrStreamInfo, engines::llm::default_ocr_prompt,
            ocr_files,
        },
        work::{WorkInput as _, WorkOutput as _},
    },
    ui::{ProgressConfig, Ui},
};

/// The `ocr` subcommand.
#[instrument(level = "debug", skip_all)]
#[allow(clippy::too_many_arguments)]
pub async fn cmd_ocr(
    ui: Ui,
    input_path: Option<&Path>,
    page_iter_opts: &PageIterOptions,
    job_count: usize,
    model: &str,
    prompt_path: Option<&Path>,
    allowed_failure_rate: f32,
    output_path: Option<&Path>,
) -> Result<()> {
    // Get our OCR prompt.
    let prompt = match prompt_path {
        Some(path) => read_json_or_toml::<ChatPrompt>(path).await?,
        None => default_ocr_prompt(),
    };

    // Open up our input stream and parse into records.
    let input = OcrInput::read_stream(ui.clone(), input_path).await?;

    // Configure our progress bar.
    let pb = ui.new_from_size_hint(
        &ProgressConfig {
            emoji: "📄",
            msg: "OCRing files",
            done_msg: "OCRed files",
        },
        input.size_hint(),
    );

    let OcrStreamInfo { stream, worker } = ocr_files(
        input,
        page_iter_opts.to_owned(),
        job_count,
        prompt,
        model.to_owned(),
    )
    .await?;
    let output = pb.wrap_stream(stream.buffered(job_count)).boxed();

    OcrOutput::write_stream(&ui, output_path, output, allowed_failure_rate).await?;

    worker.join().await
}
