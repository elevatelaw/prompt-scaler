//! Iterate over "pages" in an image.

use std::{collections::BTreeMap, process::Output, vec};

use anyhow::anyhow;
use clap::Args;
use tokio::process::Command;

use crate::{
    async_utils::{DEFAULT_ERROR_REGEX, check_for_command_failure},
    data_url::data_url,
    prelude::*,
};

/// Image types supported as-is.
const SUPPORTED_IMAGE_TYPES: &[&str] =
    &["image/png", "image/jpeg", "image/webp", "image/gif"];

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
        let mime_type = infer::get_from_path(path)
            .with_context(|| format!("failed to get MIME type for {:?}", path.display()))?
            .ok_or_else(|| anyhow!("unknown MIME type for {:?}", path.display()))?
            .mime_type()
            .to_string();

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
        } else if mime_type == "application/pdf" {
            // We have a PDF file. If we need to rasterize, do that.
            if options.rasterize {
                Self::from_rasterized_pdf(path, options, password).await
            } else {
                Self::from_split_pdf(path, options, password).await
            }
        } else {
            Err(anyhow!(
                "unsupported image or PDF MIME type {} for {:?}",
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
        let out_path = tmpdir_path.join(format!("{}-%d.pdf", filename.to_string_lossy()));
        let mut cmd = Command::new("pdfseparate");
        add_last_page_arg_if_needed(options, total_pages, &mut cmd)?;
        let output = cmd
            .arg(path)
            .arg(out_path)
            .output()
            .await
            .with_context(|| {
                format!("failed to run pdfseparate on {:?}", path.display())
            })?;
        check_for_command_failure("pdfseparate", &output, Some(&*DEFAULT_ERROR_REGEX))?;
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
        let out_path = tmpdir_path.join(filename).with_extension("png");
        let mut cmd = Command::new("pdftocairo");
        cmd.arg("-png")
            .arg("-r")
            .arg(options.rasterize_dpi.to_string());
        if let Some(password) = password {
            cmd.arg("-opw").arg(password);
        }
        add_last_page_arg_if_needed(options, total_pages, &mut cmd)?;
        let output = cmd
            .arg(path)
            .arg(out_path)
            .output()
            .await
            .with_context(|| {
                format!("failed to run pdftocairo on {:?}", path.display())
            })?;
        check_for_command_failure("pdftocairo", &output, Some(&*DEFAULT_ERROR_REGEX))?;
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
                "this iterator is partial: {} pages requested, but only {} pages available",
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
    if let Some(max_pages) = options.max_pages {
        if total_pages > max_pages {
            // The command-line tools use 1-based page numbers, as far as I can
            // tell. But they also use an inclusive range.
            let last_page = max_pages;
            cmd.arg("-l").arg(last_page.to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_PDF_PATH: &str = "tests/fixtures/ocr/two_pages.pdf";

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
}
