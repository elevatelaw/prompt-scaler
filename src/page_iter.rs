//! Iterate over "pages" in an image.

use std::{
    collections::BTreeMap,
    fs,
    io::{BufReader, Cursor},
    process::Output,
    sync::LazyLock,
    vec,
};

use anyhow::anyhow;
use clap::Args;
use image::{DynamicImage, GrayImage, ImageFormat, RgbImage, RgbaImage};
use regex::Regex;
use tiff::{
    ColorType,
    decoder::{Decoder, DecodingResult, ifd::Value},
    tags::{IfdPointer, Tag},
};
use tokio::process::Command;

use crate::{
    async_utils::{
        blocking_iter_streams::spawn_blocking_propagating_panics,
        check_for_command_failure,
    },
    cpu_limit::with_cpu_semaphore,
    data_url::data_url,
    prelude::*,
};

/// Image types supported as-is.
const SUPPORTED_IMAGE_TYPES: &[&str] =
    &["image/png", "image/jpeg", "image/webp", "image/gif"];

/// TIFF MIME type, handled separately due to multipage complexity.
const TIFF_MIME_TYPE: &str = "image/tiff";

/// A default error regex for checking command output.
static ERROR_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)error").expect("failed to compile regex"));

static DOWNGRADE_TO_WARNING_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)error: xref num").expect("failed to compile regex")
});

/// Does this line contain an error?
fn is_error_line(line: &str) -> bool {
    ERROR_REGEX.is_match(line) && !DOWNGRADE_TO_WARNING_REGEX.is_match(line)
}

/// Information about a page of a file to process.
#[derive(Debug)]
pub struct Page {
    /// The MIME type of our data. Must be one of [`SUPPORTED_IMAGE_TYPES`] or
    /// (if no rasterization has been requested) `application/pdf`.
    pub mime_type: String,
    /// The data for our page.
    pub data: Vec<u8>,
}

impl Page {
    /// Convert to a data URL.
    pub fn to_data_url(&self) -> String {
        data_url(&self.mime_type, &self.data)
    }
}

/// Options for constructing a [`PageIter`].
#[derive(Args, Clone, Debug)]
pub struct PageIterOptions {
    /// Should we rasterize any PDFs to images? Required for most models
    /// except Gemini.
    #[clap(long, default_value = "false")]
    pub rasterize: bool,

    /// The DPI to use for rasterization.
    #[clap(long, default_value = "300")]
    pub rasterize_dpi: u32,

    /// The maximum number of pages to process. If this is set, we will
    /// stop processing after this many pages and record an error.
    #[clap(long)]
    pub max_pages: Option<usize>,
}

/// An stream over PDF pages as PNG images, using Poppler's `pdftocairo` CLI
/// tool.
pub struct PageIter {
    /// An optional temporary directory, which holds extracted versions of pages.
    ///
    /// This is released by [`Drop`].
    #[allow(dead_code)]
    tmpdir: Option<tempfile::TempDir>,
    /// The MIME type of our outputs.
    mime_type: String,
    /// Iterator over the page files in the temporary directory.
    dir_iter: vec::IntoIter<PathBuf>,
    /// Expected number of pages in the document.
    total_pages: usize,
    /// The maximum number of pages we are allowed to process.
    max_pages: Option<usize>,
    /// Any warnings that occurred while processing the document.
    warnings: Vec<String>,
}

impl PageIter {
    /// Create a new [`PageIter`] from a path, based on the detected MIME type.
    ///
    /// TODO: Handle animated image types, either by erroring or by splitting
    /// the frames into pages.
    #[instrument(level = "debug", skip_all, fields(path = %path.display()))]
    pub async fn from_path(
        path: &Path,
        options: &PageIterOptions,
        password: Option<&str>,
    ) -> Result<Self> {
        // Get our MIME type.
        let mime_type = get_mime_type(path)?;

        // Check if we have a supported image type.
        if SUPPORTED_IMAGE_TYPES.contains(&mime_type.as_str()) {
            // We have a supported image type. Return a single-item iterator.
            Ok(Self {
                tmpdir: None,
                mime_type,
                dir_iter: vec![path.to_owned()].into_iter(),
                total_pages: 1,
                max_pages: options.max_pages,
                warnings: vec![],
            })
        } else if mime_type == TIFF_MIME_TYPE {
            // We have a TIFF file. Handle multipage TIFFs specially.
            Self::from_tiff(path, options).await
        } else if mime_type == "application/pdf" {
            // We have a PDF file. If we need to rasterize, do that.
            if options.rasterize {
                Self::from_rasterized_pdf(path, options, password).await
            } else {
                Self::from_split_pdf(path, options, password).await
            }
        } else {
            Err(anyhow!(
                "unsupported MIME type {} for {:?} (supported: PNG, JPEG, WebP, GIF, TIFF, PDF)",
                mime_type,
                path.display()
            ))
        }
    }

    /// Create a new [`PageIter`] from a PDF file, splitting out each page
    /// as an individual PDF file.
    #[instrument(level = "debug", skip_all, fields(path = %path.display()))]
    async fn from_split_pdf(
        path: &Path,
        options: &PageIterOptions,
        password: Option<&str>,
    ) -> Result<Self> {
        // For now, if we have a password, we need to rasterize the PDF.
        //
        // Apparently we could just run:
        //
        //     pdftops -upw <password> <file_name>.pdf <new_file_name>.pdf
        if password.is_some() {
            return Self::from_rasterized_pdf(path, options, password).await;
        }

        // Count the number of pages in the PDF.
        let total_pages = get_pdf_page_count(path).await?;

        // Construct an output filename. pdfseparate will add digits to
        // this if there is more than one page.
        let mut path_scratch = path.to_owned();
        path_scratch.set_extension("");
        let filename = path_scratch
            .file_name()
            .context("failed to get filename from PDF path")?;

        // Create a temporary directory to hold the PDF files.
        let tmpdir = tempfile::TempDir::with_prefix("pages")?;
        let tmpdir_path = tmpdir.path().to_owned();

        // Run pdfseparate to split the PDF into separate files.
        //
        // We use `with_cpu_semaphore` because `pdfseparate` will use 100% of a
        // CPU, and we don't want to run 200 copies of it at once by mistake.
        let out_path = tmpdir_path.join(format!("{}-%d.pdf", filename.to_string_lossy()));
        let mut cmd = Command::new("pdfseparate");
        add_last_page_arg_if_needed(options, total_pages, &mut cmd)?;
        let output = with_cpu_semaphore(|| async {
            cmd.arg(path).arg(out_path).output().await.with_context(|| {
                format!("failed to run pdfseparate on {:?}", path.display())
            })
        })
        .await?;
        check_for_command_failure("pdfseparate", &output, Some(&is_error_line))?;

        Self::from_tempdir(
            options,
            tmpdir,
            "application/pdf".to_string(),
            total_pages,
            &output,
        )
        .await
    }

    /// Create a new [`PdfPageStream`] from a PDF file, rasterizing each page.
    #[instrument(level = "debug", skip_all, fields(path = %path.display(), dpi))]
    async fn from_rasterized_pdf(
        path: &Path,
        options: &PageIterOptions,
        password: Option<&str>,
    ) -> Result<Self> {
        // Count the number of pages in the PDF.
        let total_pages = get_pdf_page_count(path).await?;

        // Construct an output filename. pdftocairo will add digits to this if
        // there is more than one page.
        let filename = path
            .file_name()
            .context("failed to get filename from PDF path")?;

        // Create a temporary directory to hold the PNG files.
        let tmpdir = tempfile::TempDir::with_prefix("pages")?;
        let tmpdir_path = tmpdir.path().to_owned();

        // Run pdftocairo to convert the PDF to PNG files.
        //
        // We use `with_cpu_semaphore` because `pdftocairo` will use _at least_
        // 100% of a CPU, and we don't want to run 200 copies of it at once by
        // mistake.
        let out_path = tmpdir_path.join(filename).with_extension("png");
        let mut cmd = Command::new("pdftocairo");
        cmd.arg("-png")
            .arg("-r")
            .arg(options.rasterize_dpi.to_string());
        if let Some(password) = password {
            cmd.arg("-opw").arg(password);
        }
        add_last_page_arg_if_needed(options, total_pages, &mut cmd)?;
        let output = with_cpu_semaphore(|| async {
            cmd.arg(path).arg(out_path).output().await.with_context(|| {
                format!("failed to run pdftocairo on {:?}", path.display())
            })
        })
        .await?;
        check_for_command_failure("pdftocairo", &output, Some(&is_error_line))?;
        Self::from_tempdir(
            options,
            tmpdir,
            "image/png".to_string(),
            total_pages,
            &output,
        )
        .await
    }

    /// Create a [`PageIter`] from a [`tempdir::TempDir`] full of files
    /// named in lexixal order, plus a MIME type.
    async fn from_tempdir(
        options: &PageIterOptions,
        tmpdir: tempfile::TempDir,
        mime_type: String,
        total_pages: usize,
        output: &Output,
    ) -> Result<Self> {
        // Get the path to the temporary directory.
        let tmpdir_path = tmpdir.path();

        // Get the list of PNG files in the temporary directory.
        let mut dir_paths = tmpdir_path
            .read_dir()
            .with_context(|| {
                format!(
                    "failed to read temporary directory {:?}",
                    tmpdir_path.display()
                )
            })?
            .map(|entry| {
                let entry = entry.with_context(|| {
                    format!(
                        "failed to read entry in temporary directory {:?}",
                        tmpdir_path.display()
                    )
                })?;
                let path = entry.path();
                Ok(path)
            })
            .collect::<Result<Vec<_>>>()?;
        dir_paths.sort();
        let dir_iter = dir_paths.into_iter();

        // Get the output of the command, and save as warnings.
        let mut warnings = vec![];
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            warnings.push(line.trim().to_string());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines() {
            warnings.push(line.trim().to_string());
        }

        // Return our iterator.
        Ok(Self {
            tmpdir: Some(tmpdir),
            mime_type,
            dir_iter,
            total_pages,
            max_pages: options.max_pages,
            warnings,
        })
    }

    /// Create a new [`PageIter`] from a multipage TIFF file.
    ///
    /// This method:
    /// 1. Iterates the main IFD chain to extract document pages
    /// 2. Validates any SubIFDs to ensure no document content is hidden
    /// 3. Converts each page to PNG for LLM consumption
    #[instrument(level = "debug", skip_all, fields(path = %path.display()))]
    async fn from_tiff(path: &Path, options: &PageIterOptions) -> Result<Self> {
        let path_owned = path.to_owned();
        let max_pages = options.max_pages;

        // Run TIFF processing on blocking thread pool (CPU-intensive).
        let (tmpdir, total_pages, warnings) =
            spawn_blocking_propagating_panics(move || {
                process_tiff_sync(&path_owned, max_pages)
            })
            .await?;

        // Get the list of PNG files in the temporary directory.
        let tmpdir_path = tmpdir.path();
        let mut dir_paths = tmpdir_path
            .read_dir()
            .with_context(|| {
                format!(
                    "failed to read temporary directory {:?}",
                    tmpdir_path.display()
                )
            })?
            .map(|entry| {
                let entry = entry.with_context(|| {
                    format!(
                        "failed to read entry in temporary directory {:?}",
                        tmpdir_path.display()
                    )
                })?;
                Ok(entry.path())
            })
            .collect::<Result<Vec<_>>>()?;
        dir_paths.sort();
        let dir_iter = dir_paths.into_iter();

        Ok(Self {
            tmpdir: Some(tmpdir),
            mime_type: "image/png".to_string(),
            dir_iter,
            total_pages,
            max_pages: options.max_pages,
            warnings,
        })
    }

    /// Get any warnings that occurred while processing the document.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Will this iterator return only an incomplete set of pages?
    pub fn is_incomplete(&self) -> bool {
        if let Some(max_pages) = self.max_pages {
            self.total_pages > max_pages
        } else {
            false
        }
    }

    /// If this iterator will return only an incomplete set of pages, return an
    pub fn check_complete(&self) -> Result<()> {
        if self.is_incomplete() {
            Err(anyhow!(
                "Only {}/{} pages processed (because of --max-pages)",
                self.max_pages.expect("max_pages should be set"),
                self.total_pages
            ))
        } else {
            Ok(())
        }
    }
}

impl Drop for PageIter {
    fn drop(&mut self) {
        // Delete our temporary directory, if we have one.
        if let Some(tmpdir) = self.tmpdir.take() {
            let tmpdir_path = tmpdir.path().to_owned();
            if let Err(err) = tmpdir.close() {
                error!(
                    directory = ?tmpdir_path.display(),
                    "failed to delete temporary directory: {}",
                    err
                );
            }
        }
    }
}

impl Iterator for PageIter {
    type Item = Result<Page>;

    fn next(&mut self) -> Option<Self::Item> {
        use std::fs;
        if let Some(path) = self.dir_iter.next() {
            // Read the PNG file into a byte vector.
            let result = fs::read(&path)
                .with_context(|| format!("failed to read file {:?}", path.display()));
            let bytes = match result {
                Ok(bytes) => bytes,
                Err(err) => return Some(Err(err)),
            };

            // Delete the file to recover space a bit early.
            if self.tmpdir.is_some() {
                let result = fs::remove_file(&path).with_context(|| {
                    format!("failed to delete file {:?}", path.display())
                });
                if let Err(err) = result {
                    return Some(Err(err));
                }
            }

            Some(Ok(Page {
                mime_type: self.mime_type.clone(),
                data: bytes,
            }))
        } else {
            None
        }
    }
}

/// Get the number of pages in a PDF file.
#[instrument(level = "debug", skip_all, fields(path = %path.display()))]
pub async fn get_pdf_page_count(path: &Path) -> Result<usize> {
    // Run pdfinfo to get the number of pages.
    let mut cmd = Command::new("pdfinfo");
    let output = cmd
        .arg(path)
        .output()
        .await
        .with_context(|| format!("failed to run pdfinfo on {:?}", path.display()))?;
    check_for_command_failure("pdfinfo", &output, None)?;

    // Parse the output of pdfinfo into properties.
    let output =
        String::from_utf8(output.stdout).context("pdfinfo output was not valid UTF-8")?;
    let mut properties = BTreeMap::new();
    for line in output.lines() {
        let mut parts = line.splitn(2, ':');
        let key = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        properties.insert(key.to_string(), value.to_string());
    }

    // Get the number of pages from the properties.
    let page_count_str = properties
        .get("Pages")
        .ok_or_else(|| anyhow!("failed to find page count in pdfinfo output"))?;
    page_count_str.parse::<usize>().with_context(|| {
        format!(
            "failed to parse page count for {:?} from pdfinfo output",
            path.display()
        )
    })
}

/// Add a "last page" argument to a [`Command`].
fn add_last_page_arg_if_needed(
    options: &PageIterOptions,
    total_pages: usize,
    cmd: &mut Command,
) -> Result<()> {
    if let Some(max_pages) = options.max_pages
        && total_pages > max_pages
    {
        // The command-line tools use 1-based page numbers, as far as I can
        // tell. But they also use an inclusive range.
        let last_page = max_pages;
        cmd.arg("-l").arg(last_page.to_string());
    }
    Ok(())
}

/// Get the MIME type of a file.
pub fn get_mime_type(path: &Path) -> Result<String> {
    Ok(infer::get_from_path(path)
        .with_context(|| format!("failed to get MIME type for {:?}", path.display()))?
        .ok_or_else(|| anyhow!("unknown MIME type for {:?}", path.display()))?
        .mime_type()
        .to_string())
}

// ============================================================================
// TIFF processing helpers
// ============================================================================

/// NewSubfileType bit definitions per TIFF 6.0 specification.
mod tiff_subfile_type {
    /// Bit 0: Reduced resolution image (thumbnail/preview).
    pub const REDUCED_RESOLUTION: u32 = 0x1;
    /// Bit 1: Single page of a multi-page document.
    pub const SINGLE_PAGE: u32 = 0x2;
    /// Bit 2: Transparency mask for another image.
    pub const TRANSPARENCY_MASK: u32 = 0x4;
    /// DNG extensions (bits 3, 4, 16): depth map, enhanced image, semantic mask.
    pub const DNG_BITS: u32 = 0x8 | 0x10 | 0x10000;
}

/// Process a TIFF file synchronously, returning a tempdir with PNG pages.
///
/// We do this the hard way, because we want to be sure to get multiple pages.
/// And unfortunately, multiple pages can be represented in many different ways,
/// depending on source. We attempt to error aggressively on things that we do
/// not understand, in order to prevent accidentally missing data.
///
/// For scanned documents, the most important case by far is pages represented
/// as IFDs, which [`tiff`] handles out of the box. There's another rare SubIFD
/// representation. If we see _that_, we error. Some other cases like thumbnails
/// should not be treated as separate pages, because that will cause a wide
/// range of LLM-based processing to fail.
fn process_tiff_sync(
    path: &Path,
    max_pages: Option<usize>,
) -> Result<(tempfile::TempDir, usize, Vec<String>)> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to open TIFF file {:?}", path.display()))?;
    let mut decoder = Decoder::new(BufReader::new(file)).with_context(|| {
        format!("failed to create TIFF decoder for {:?}", path.display())
    })?;

    let tmpdir = tempfile::TempDir::with_prefix("tiff-pages")?;
    let mut warnings = Vec::new();
    let mut page_count = 0;
    let mut ifd_index = 0;

    loop {
        // Check max_pages limit before processing.
        if let Some(max) = max_pages
            && page_count >= max
        {
            break;
        }

        // Seek to this IFD (skip for first IFD, which is already loaded).
        if ifd_index > 0 {
            if !decoder.more_images() {
                break;
            }
            decoder.next_image().with_context(|| {
                format!(
                    "failed to advance to IFD {} in {:?}",
                    ifd_index,
                    path.display()
                )
            })?;
        }

        // Validate SubIFDs for this IFD.
        validate_subifds(&mut decoder, path, ifd_index, &mut warnings)?;

        // Decode the image.
        let (width, height) = decoder.dimensions().with_context(|| {
            format!(
                "failed to get dimensions for IFD {} in {:?}",
                ifd_index,
                path.display()
            )
        })?;

        let image = decode_tiff_image(&mut decoder, width, height, path, ifd_index)?;

        // Write as PNG to tempdir.
        let png_path = tmpdir.path().join(format!("page-{:05}.png", page_count));
        let mut png_bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut png_bytes), ImageFormat::Png)
            .with_context(|| {
                format!(
                    "failed to encode PNG for IFD {} in {:?}",
                    ifd_index,
                    path.display()
                )
            })?;
        fs::write(&png_path, png_bytes)
            .with_context(|| format!("failed to write PNG {:?}", png_path.display()))?;

        page_count += 1;
        ifd_index += 1;
    }

    // Count total pages (continue iterating without decoding).
    let mut total_pages = page_count;
    while decoder.more_images() {
        decoder.next_image().ok(); // Ignore errors for counting
        total_pages += 1;
    }

    debug!(
        path = %path.display(),
        page_count = page_count,
        total_pages = total_pages,
        "Processed multipage TIFF"
    );

    Ok((tmpdir, total_pages, warnings))
}

/// Validate SubIFDs for an IFD, ensuring no document content is hidden.
///
/// Returns Ok(()) if SubIFDs are safe to skip (thumbnails/masks/DNG metadata).
/// Returns Err if SubIFDs have ambiguous NewSubfileType that might contain
/// document content.
///
/// We are pretty conservative here, preferring to error on things we do not
/// understand.
fn validate_subifds<R: std::io::Read + std::io::Seek>(
    decoder: &mut tiff::decoder::Decoder<R>,
    path: &Path,
    ifd_index: usize,
    warnings: &mut Vec<String>,
) -> Result<()> {
    // Check if this IFD has SubIFDs.
    let subifd_value = match decoder.find_tag(Tag::SubIfd) {
        Ok(Some(value)) => value,
        Ok(None) | Err(_) => return Ok(()), // No SubIFD tag present
    };

    // Try to extract IFD pointers from the SubIFD value.
    let subifd_offsets: Vec<u64> = match subifd_value {
        Value::Ifd(offset) => vec![u64::from(offset)],
        Value::List(list) => list
            .iter()
            .filter_map(|v| match v {
                Value::Ifd(offset) => Some(u64::from(*offset)),
                _ => None,
            })
            .collect(),
        _ => return Ok(()), // Not IFD pointers
    };

    for (sub_idx, &offset) in subifd_offsets.iter().enumerate() {
        // Read the SubIFD directory.
        let subdir = match decoder.read_directory(IfdPointer(offset)) {
            Ok(dir) => dir,
            Err(e) => {
                warnings.push(format!(
                    "Could not read SubIFD {} of IFD {}: {}",
                    sub_idx, ifd_index, e
                ));
                continue;
            }
        };

        // Get NewSubfileType from the SubIFD.
        let subfile_type = get_new_subfile_type(decoder, &subdir);

        // Check if this SubIFD is safe to skip.
        if (subfile_type & tiff_subfile_type::REDUCED_RESOLUTION) != 0 {
            // Bit 0: Reduced resolution (thumbnail) - safe to skip.
            debug!(
                path = %path.display(),
                ifd_index = ifd_index,
                sub_idx = sub_idx,
                subfile_type = subfile_type,
                "Skipping SubIFD: reduced resolution image (thumbnail)"
            );
            continue;
        }

        if (subfile_type & tiff_subfile_type::TRANSPARENCY_MASK) != 0 {
            // Bit 2: Transparency mask - safe to skip.
            debug!(
                path = %path.display(),
                ifd_index = ifd_index,
                sub_idx = sub_idx,
                subfile_type = subfile_type,
                "Skipping SubIFD: transparency mask"
            );
            continue;
        }

        if (subfile_type & tiff_subfile_type::DNG_BITS) != 0 {
            // DNG-specific bits - safe to skip.
            debug!(
                path = %path.display(),
                ifd_index = ifd_index,
                sub_idx = sub_idx,
                subfile_type = subfile_type,
                "Skipping SubIFD: DNG camera metadata"
            );
            continue;
        }

        // Ambiguous SubIFD - error to prevent silent data loss.
        if subfile_type == 0 || (subfile_type & tiff_subfile_type::SINGLE_PAGE) != 0 {
            return Err(anyhow!(
                "TIFF file {:?} has ambiguous SubIFD content in IFD {} (SubIFD {}, \
                 NewSubfileType={}). This SubIFD may contain document pages that \
                 would be silently dropped. To avoid missing data, please convert \
                 this TIFF to PDF or individual images using an appropriate tool \
                 before processing.",
                path.display(),
                ifd_index,
                sub_idx,
                subfile_type
            ));
        }
    }

    Ok(())
}

/// Get the NewSubfileType value from a SubIFD directory.
fn get_new_subfile_type<R: std::io::Read + std::io::Seek>(
    decoder: &mut tiff::decoder::Decoder<R>,
    subdir: &tiff::Directory,
) -> u32 {
    let mut ifd_decoder = decoder.read_directory_tags(subdir);
    match ifd_decoder.find_tag(Tag::NewSubfileType) {
        Ok(Some(value)) => value.into_u32().unwrap_or(0),
        _ => 0,
    }
}

/// Decode a TIFF image from the current IFD to a DynamicImage.
fn decode_tiff_image<R: std::io::Read + std::io::Seek>(
    decoder: &mut tiff::decoder::Decoder<R>,
    width: u32,
    height: u32,
    path: &Path,
    ifd_index: usize,
) -> Result<image::DynamicImage> {
    let color_type = decoder.colortype().with_context(|| {
        format!(
            "failed to get color type for IFD {} in {:?}",
            ifd_index,
            path.display()
        )
    })?;

    let result = decoder.read_image().with_context(|| {
        format!("failed to decode IFD {} in {:?}", ifd_index, path.display())
    })?;

    // Apparently we need to do this the hard way
    let image = match result {
        DecodingResult::U8(data) => match color_type {
            ColorType::Gray(_) => {
                let gray = GrayImage::from_raw(width, height, data).ok_or_else(|| {
                    anyhow!(
                        "failed to create grayscale image for IFD {} in {:?}",
                        ifd_index,
                        path.display()
                    )
                })?;
                DynamicImage::ImageLuma8(gray)
            }
            ColorType::RGB(_) => {
                let rgb = RgbImage::from_raw(width, height, data).ok_or_else(|| {
                    anyhow!(
                        "failed to create RGB image for IFD {} in {:?}",
                        ifd_index,
                        path.display()
                    )
                })?;
                DynamicImage::ImageRgb8(rgb)
            }
            ColorType::RGBA(_) => {
                let rgba = RgbaImage::from_raw(width, height, data).ok_or_else(|| {
                    anyhow!(
                        "failed to create RGBA image for IFD {} in {:?}",
                        ifd_index,
                        path.display()
                    )
                })?;
                DynamicImage::ImageRgba8(rgba)
            }
            other => {
                return Err(anyhow!(
                    "unsupported TIFF color type {:?} in IFD {} of {:?}",
                    other,
                    ifd_index,
                    path.display()
                ));
            }
        },
        DecodingResult::U16(data) => {
            // Convert 16-bit to 8-bit by scaling.
            let data_u8: Vec<u8> = data.iter().map(|&v| (v >> 8) as u8).collect();
            match color_type {
                ColorType::Gray(_) => {
                    let gray =
                        GrayImage::from_raw(width, height, data_u8).ok_or_else(|| {
                            anyhow!(
                                "failed to create 16-bit grayscale image for IFD {} in {:?}",
                                ifd_index,
                                path.display()
                            )
                        })?;
                    DynamicImage::ImageLuma8(gray)
                }
                ColorType::RGB(_) => {
                    let rgb =
                        RgbImage::from_raw(width, height, data_u8).ok_or_else(|| {
                            anyhow!(
                                "failed to create 16-bit RGB image for IFD {} in {:?}",
                                ifd_index,
                                path.display()
                            )
                        })?;
                    DynamicImage::ImageRgb8(rgb)
                }
                other => {
                    return Err(anyhow!(
                        "unsupported 16-bit TIFF color type {:?} in IFD {} of {:?}",
                        other,
                        ifd_index,
                        path.display()
                    ));
                }
            }
        }
        other => {
            return Err(anyhow!(
                "unsupported TIFF sample format in IFD {} of {:?}: {:?}",
                ifd_index,
                path.display(),
                std::any::type_name_of_val(&other)
            ));
        }
    };

    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_PDF_PATH: &str = "tests/fixtures/ocr/two_pages.pdf";

    #[test]
    fn is_error_line_works() {
        assert!(is_error_line("error: something went wrong"));
        assert!(is_error_line("ERROR: something went wrong"));
        assert!(!is_error_line("Warning: something is odd"));
        assert!(!is_error_line(
            "Internal Error: xref num 1234 not found but needed, document has changes, reconstruct aborted"
        ));
    }

    #[tokio::test]
    #[ignore = "Requires poppler-utils to be installed"]
    async fn page_count_returns_correct_number_of_pages() -> Result<()> {
        let page_count = get_pdf_page_count(Path::new(TEST_PDF_PATH)).await?;
        assert_eq!(page_count, 2);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "Requires poppler-utils to be installed"]
    async fn page_iter_returns_correct_number_of_pages() -> Result<()> {
        let page_iter = PageIter::from_path(
            Path::new(TEST_PDF_PATH),
            &PageIterOptions {
                rasterize: true,
                rasterize_dpi: 300,
                max_pages: None,
            },
            None,
        )
        .await?;
        let pages = page_iter.collect::<Result<Vec<_>, _>>()?;
        assert_eq!(pages.len(), 2);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "Requires poppler-utils to be installed"]
    async fn page_iter_obeys_max_pages() -> Result<()> {
        let page_iter = PageIter::from_path(
            Path::new(TEST_PDF_PATH),
            &PageIterOptions {
                rasterize: false,
                rasterize_dpi: 300,
                max_pages: Some(1),
            },
            None,
        )
        .await?;
        assert!(page_iter.is_incomplete());
        assert!(page_iter.check_complete().is_err());
        let pages = page_iter.collect::<Result<Vec<_>, _>>()?;
        assert_eq!(pages.len(), 1);
        Ok(())
    }

    static TEST_TIFF_PATH: &str = "tests/fixtures/ocr/two_pages.tiff";

    #[tokio::test]
    async fn tiff_page_iter_returns_correct_number_of_pages() -> Result<()> {
        let page_iter = PageIter::from_path(
            Path::new(TEST_TIFF_PATH),
            &PageIterOptions {
                rasterize: false,
                rasterize_dpi: 300,
                max_pages: None,
            },
            None,
        )
        .await?;
        let pages = page_iter.collect::<Result<Vec<_>, _>>()?;
        // The TIFF has 2 pages (converted from 2-page PDF).
        assert_eq!(pages.len(), 2);
        // Each page should be PNG.
        for page in &pages {
            assert_eq!(page.mime_type, "image/png");
        }
        Ok(())
    }

    #[tokio::test]
    async fn tiff_page_iter_obeys_max_pages() -> Result<()> {
        let page_iter = PageIter::from_path(
            Path::new(TEST_TIFF_PATH),
            &PageIterOptions {
                rasterize: false,
                rasterize_dpi: 300,
                max_pages: Some(1),
            },
            None,
        )
        .await?;
        assert!(page_iter.is_incomplete());
        assert!(page_iter.check_complete().is_err());
        let pages = page_iter.collect::<Result<Vec<_>, _>>()?;
        assert_eq!(pages.len(), 1);
        Ok(())
    }
}
