//! I/O utilities.
//!
//! This module is responsible for reading JSON, TOML, JSONL, and CSV files, and
//! writing JSONL files. There are a few complicating factors:
//!
//! 1. We use async streams from Tokio, because that's an easy way to handle a
//!    large (but limited) number of failible network operations in parallel.
//! 2. We support multiple input and output formats, including support for
//!    automatic format detection from filenames or the first byte of the file.
//!
//! In general, Tokio and async Rust involve some occasional magic. We try to
//! keep all of it in this file.

use std::{pin::Pin, sync::Arc, task::Context, vec};

use futures::{TryStreamExt, pin_mut, stream::StreamExt as _};
use peekable::tokio::AsyncPeekable;
use serde_json::Map;
use tokio::{
    fs::File,
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt as _, AsyncWrite,
        AsyncWriteExt as _, BufReader, BufWriter, ReadBuf,
    },
};
use tokio_stream::wrappers::LinesStream;

use crate::{
    prelude::*,
    ui::{ProgressConfig, Ui},
};

use super::{BoxedStream, size_hint::WithSizeHintExt};

/// A smart async reader that uses [`AsyncPeekable`] to detect whether the input is JSON
/// or JSONL, or something else.
pub struct SmartReader {
    /// Do we expect our input to be either JSONL or JSONL?
    is_json_like: bool,

    /// A human-readable description of the input source, for error messages.
    description: String,

    /// Our reader. There's some [`Pin`] stuff going on here because we're
    /// defining an async reader, and we don't want the value to get moved while
    /// an async function holds pointers into it.
    reader: Pin<Box<dyn AsyncBufRead + Unpin + Send + Sync + 'static>>,
}

impl SmartReader {
    /// Create a new `SmartReader` from an existing reader.
    pub async fn new_from_reader(
        description: String,
        reader: impl AsyncRead + Unpin + Send + Sync + 'static,
    ) -> Result<Self> {
        let reader = BufReader::new(reader);
        let mut peekable = AsyncPeekable::new(Box::new(reader));
        let mut buffer = vec![0; 1];
        peekable.peek_exact(&mut buffer).await?;
        let is_json_like = buffer[0] == b'{';
        Ok(Self {
            is_json_like,
            description,
            reader: Box::pin(BufReader::new(peekable)),
        })
    }

    /// Create a new `SmartReader` from a [`Path`].
    pub async fn new_from_path(path: &Path) -> Result<Self> {
        let ext = path.extension().unwrap_or_default();
        let is_json_like = ext == "json" || ext == "jsonl";
        let file = File::open(path)
            .await
            .with_context(|| format!("Failed to open file at path: {:?}", path))?;
        Ok(Self {
            is_json_like,
            description: path.to_string_lossy().into_owned(),
            reader: Box::pin(BufReader::new(file)),
        })
    }

    /// Create a new `SmartReader` from either a [`Path`] or standard input.
    pub async fn new_from_path_or_stdin(path: Option<&Path>) -> Result<Self> {
        match path {
            Some(path) => Self::new_from_path(path).await,
            None => {
                let stdin = tokio::io::stdin();
                Self::new_from_reader("stdin".to_owned(), stdin).await
            }
        }
    }

    /// Is our input JSON-like?
    pub fn is_json_like(&self) -> bool {
        self.is_json_like
    }
}

impl AsyncRead for SmartReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // `Pin` is the most mysterious of arts in Rust.
        //
        // See https://stackoverflow.com/a/75728106 and
        // https://users.rust-lang.org/t/impl-future-around-a-poll-method-that-returns-a-ref/39202/4
        Pin::get_mut(self).reader.as_mut().poll_read(cx, buf)
    }
}

impl AsyncBufRead for SmartReader {
    fn poll_fill_buf(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> std::task::Poll<std::io::Result<&[u8]>> {
        Pin::get_mut(self).reader.as_mut().poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        Pin::get_mut(self).reader.as_mut().consume(amt)
    }
}

/// Read TOML or JSON from a file.
pub async fn read_json_or_toml<T>(path: &Path) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let mut reader = SmartReader::new_from_path(path).await?;
    let mut data = String::new();
    // Read all at once because our parsing libraries don't do async I/O.
    reader
        .read_to_string(&mut data)
        .await
        .with_context(|| format!("Failed to read file at path: {:?}", path))?;
    if reader.is_json_like() {
        serde_json::from_str(&data).with_context(|| {
            format!("Failed to parse JSON from file at path: {:?}", path)
        })
    } else {
        toml::from_str(&data).with_context(|| {
            format!("Failed to parse TOML from file at path: {:?}", path)
        })
    }
}

/// Count JSONL or CSV records in a file.
#[instrument(level = "debug", skip_all, fields(path = %path.display()))]
pub async fn count_jsonl_or_csv_records(
    ui: &Ui,
    path: &Path,
) -> Result<(usize, Option<usize>)> {
    // If this isn't a file, we can't count records. This may happen if our
    // input is a named pipe from a tool like Pachyderm.
    if !path.is_file() {
        return Ok((0, None));
    }

    // Create a progress indicator.
    let spinner = ui.new_spinner(&ProgressConfig {
        emoji: "🧮",
        msg: "Counting input records",
        done_msg: "Counted input records",
    });

    // Count records.
    let reader = SmartReader::new_from_path_or_stdin(Some(path)).await?;
    let count = if reader.is_json_like() {
        let lines = LinesStream::new(reader.lines());
        lines
            .try_fold(0, |acc, _line| async move { Ok(acc + 1) })
            .await?
    } else {
        csv_async::AsyncReaderBuilder::new()
            .create_reader(reader)
            .into_byte_records()
            .try_fold(0, |acc, _record| async move { Ok(acc + 1) })
            .await?
    };
    spinner.finish_with_message(format!("Found {count} records"));
    Ok((count, Some(count)))
}

/// A JSON Object value, without the surrounding [`Value::Object`] wrapper.
pub type JsonObject = Map<String, Value>;

/// A stream of [`serde_json::Value`] values.
pub type JsonStream = BoxedStream<Result<Value>>;

/// Read JSONL or CSV from a file or stdin.
///
/// This function returns an async [`Stream`] of JSON [`Map`] objects.
pub async fn read_jsonl_or_csv(ui: Ui, path: Option<&Path>) -> Result<JsonStream> {
    let size_hint = match path {
        Some(path) => count_jsonl_or_csv_records(&ui, path).await?,
        None => (0, None),
    };

    let reader = SmartReader::new_from_path_or_stdin(path).await?;
    let description = Arc::new(reader.description.clone());
    if reader.is_json_like() {
        let lines = LinesStream::new(reader.lines()).with_size_hint(size_hint);
        Ok(Box::pin(lines.then(move |line| {
            let description = description.clone();
            async move {
                let line = line?;
                let map: Value = serde_json::from_str(&line).with_context(|| {
                    format!(
                        "Failed to parse JSON from line in {:?}: {:?}",
                        description, line
                    )
                })?;
                Ok(map)
            }
        })))
    } else {
        let mut reader = csv_async::AsyncReaderBuilder::new().create_reader(reader);
        let headers = Arc::new(
            reader
                .headers()
                .await
                .with_context(|| {
                    format!("Failed to read CSV headers from {:?}", description)
                })?
                .to_owned(),
        );
        Ok(Box::pin(
            reader
                .into_records()
                .with_size_hint(size_hint)
                .then(move |record| {
                    let description = description.clone();
                    let headers = headers.clone();
                    async move {
                        let record = record.with_context(|| {
                            format!("Failed to read CSV record from {:?}", description)
                        })?;
                        let map: Map<String, Value> = headers
                            .iter()
                            .zip(record.iter())
                            .map(|(header, value)| {
                                (header.to_owned(), Value::String(value.to_owned()))
                            })
                            .collect();
                        Ok(Value::Object(map))
                    }
                }),
        ))
    }
}

/// Create an [`AsyncWrite`] for a file or stdout.
async fn create_writer(
    path: Option<&Path>,
) -> Result<Box<dyn AsyncWrite + Unpin + Send + Sync + 'static>> {
    match path {
        Some(path) => {
            let file = File::create(path)
                .await
                .with_context(|| format!("Failed to create file at path: {:?}", path))?;
            Ok(Box::new(file))
        }
        None => Ok(Box::new(tokio::io::stdout())),
    }
}

/// Write a stream of JSON [`Map`] objects to either standard output or a file.
pub async fn write_output(path: Option<&Path>, stream: JsonStream) -> Result<()> {
    let mut writer = BufWriter::new(create_writer(path).await?);
    pin_mut!(stream);
    while let Some(map) = stream.next().await {
        let map = map?;
        let json = serde_json::to_string(&map)
            .with_context(|| format!("Failed to serialize JSON from map: {:?}", map))?;
        writer
            .write_all(json.as_bytes())
            .await
            .context("Failed to write JSON to output")?;
        writer
            .write_all(b"\n")
            .await
            .context("Failed to write newline to output")?;
    }
    writer.flush().await.context("Failed to flush output")?;
    Ok(())
}
