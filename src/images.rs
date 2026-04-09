//! Common interface for loading images.
//!
//! This is used by both [`crate::page_iter::PageIter`] and the
//! [`crate::prompt::ChatPrompt`] rendering code to represent images. One of our
//! big constraints, especially in OCR mode, is that we need to be _very_ careful about
//! how much RAM we use.

use std::{
    fs,
    io::{self, Read as _},
};

use base64::{prelude::BASE64_STANDARD, write::EncoderWriter};
use url::Url;

use crate::{
    async_utils::blocking_iter_streams::spawn_blocking_propagating_panics,
    mem_limit::{MemLimiter, MemPermit},
    page_iter::get_mime_type,
    prelude::*,
};

/// An opaque handle that keeps a temporary directory alive until dropped.
///
/// Returned alongside `PageIter` so the caller can hold it while page
/// processing is in flight. With late image loading, `ImageFile` handles
/// point to files in a tmpdir that must outlive all in-flight processing.
#[derive(Debug)]
pub struct TempDirHandle {
    _tempdir: Option<tempfile::TempDir>,
}

impl TempDirHandle {
    /// Create a new `TempDirHandle`.
    pub fn new(tempdir: Option<tempfile::TempDir>) -> Self {
        Self { _tempdir: tempdir }
    }
}

/// Data URL scheme.
static DATA_URL_SCHEME: &str = "data:";

/// Data URL encoding string.
static DATA_URL_ENCODING_STR: &str = ";base64,";

/// Different encodings that can be used to load a file's data into memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageEncoding {
    Binary,
    Base64,
    DataUrl,
}

impl ImageEncoding {
    /// Get an upper bound on the size of the loaded data, in bytes, for a file of the given size.
    ///
    /// This is used to estimate how much RAM an image will use when loaded, so that we can provide
    /// backpressure based on total RAM usage.
    fn max_loaded_size(&self, mime_type: &str, file_size: usize) -> usize {
        match self {
            ImageEncoding::Binary => file_size,
            ImageEncoding::Base64 => (4 * file_size.div_ceil(3)) + 4,
            ImageEncoding::DataUrl => {
                ImageEncoding::Base64.max_loaded_size(mime_type, file_size)
                    + DATA_URL_SCHEME.len()
                    + mime_type.len()
                    + DATA_URL_ENCODING_STR.len()
            }
        }
    }
}

/// A handle pointing to an image file.
#[derive(Clone, Debug)]
pub struct ImageFile {
    /// The MIME type of the image, e.g. "image/png".
    pub mime_type: String,

    /// The path to the image file.
    pub path: PathBuf,
}

impl ImageFile {
    /// Create a new `ImageHandle` from a path.
    pub fn from_path(path: &Path) -> Result<Self> {
        let mime_type = get_mime_type(Path::new(path))?;
        Ok(Self {
            mime_type,
            path: path.to_path_buf(),
        })
    }

    /// Get the MIME type of the image.
    #[allow(dead_code)]
    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    /// Serialize as a `file:` URL string, e.g.
    /// `file:///tmp/pages/page-1.png?mime_type=image/png`.
    pub fn to_url(&self) -> String {
        let mut url = Url::from_file_path(&self.path)
            .expect("ImageFile path should be absolute and produce a valid file: URL");
        url.query_pairs_mut()
            .append_pair("mime_type", &self.mime_type);
        url.to_string()
    }

    /// Parse an `ImageFile` from a `file:` URL string produced by
    /// [`Self::to_url`].
    pub fn from_url(url_str: &str) -> Result<Self> {
        let url = Url::parse(url_str)
            .with_context(|| format!("failed to parse image URL: {url_str}"))?;
        if url.scheme() != "file" {
            return Err(anyhow!(
                "expected file: URL scheme, got {:?} in: {url_str}",
                url.scheme()
            ));
        }
        let path = url
            .to_file_path()
            .map_err(|()| anyhow!("failed to extract file path from URL: {url_str}"))?;
        let mime_type = url
            .query_pairs()
            .find(|(k, _)| k == "mime_type")
            .map(|(_, v)| v.into_owned())
            .ok_or_else(|| {
                anyhow!("missing mime_type query parameter in URL: {url_str}")
            })?;
        Ok(Self { mime_type, path })
    }

    /// Load the image data into memory as bytes.
    fn load_and_append_bytes(&self, buf: &mut Vec<u8>) -> Result<()> {
        let mut file = fs::File::open(&self.path).with_context(|| {
            format!("could not open image file: {}", self.path.display())
        })?;
        file.read_to_end(buf).with_context(|| {
            format!("could not read image file: {}", self.path.display())
        })?;
        Ok(())
    }

    /// Load and appended Base64-encoded image data to the given buffer.
    fn load_and_append_base64(&self, buf: &mut Vec<u8>) -> Result<()> {
        let mut file = fs::File::open(&self.path).with_context(|| {
            format!("could not open image file: {}", self.path.display())
        })?;
        let mut wtr = EncoderWriter::new(buf, &BASE64_STANDARD);
        io::copy(&mut file, &mut wtr).with_context(|| {
            format!("could not read image file: {}", self.path.display())
        })?;
        wtr.finish().with_context(|| {
            format!(
                "could not finish Base64 encoding for image file: {}",
                self.path.display()
            )
        })?;
        Ok(())
    }

    fn file_size(&self) -> Result<usize, anyhow::Error> {
        Ok(fs::metadata(&self.path)
            .with_context(|| {
                format!(
                    "could not get metadata for image file: {}",
                    self.path.display()
                )
            })?
            .len() as usize)
    }

    /// Synchronous internal helper for loading an image.
    fn load_helper(
        &self,
        encoding: ImageEncoding,
        mem_permit: MemPermit,
        max_loaded_size: usize,
    ) -> Result<ImageData> {
        // Pre-allocate a buffer for the loaded data.
        let mut data = Vec::with_capacity(max_loaded_size);
        match encoding {
            ImageEncoding::Binary => self.load_and_append_bytes(&mut data),
            ImageEncoding::Base64 => self.load_and_append_base64(&mut data),
            ImageEncoding::DataUrl => {
                data.extend_from_slice(DATA_URL_SCHEME.as_bytes());
                data.extend_from_slice(self.mime_type.as_bytes());
                data.extend_from_slice(DATA_URL_ENCODING_STR.as_bytes());
                self.load_and_append_base64(&mut data)
            }
        }?;
        Ok(ImageData {
            mime_type: self.mime_type.clone(),
            encoding,
            data,
            _mem_permit: mem_permit,
        })
    }

    /// Load an image using the specified encoding.
    ///
    /// This may block until sufficient RAM can be reserved for the loaded
    /// image.
    ///
    /// DEADLOCK WARNING: If you hold one [`ImageData`] on a worker task, and
    /// try to load another one before releasing the first, then you may deadlock
    /// if another task is doing the same thing.
    pub async fn load(
        &self,
        encoding: ImageEncoding,
        mem_limit: &MemLimiter,
    ) -> Result<ImageData> {
        // Acquire memory permits for final file size _before_ loading any data.
        let max_loaded_size =
            encoding.max_loaded_size(&self.mime_type, self.file_size()?);
        let mem_permit = mem_limit.acquire(max_loaded_size).await?;

        // Load the data on a background thread to avoid blocking the executor.
        let self_clone = self.clone(); // Cheap clone to work around lifetimes.
        spawn_blocking_propagating_panics(move || {
            self_clone.load_helper(encoding, mem_permit, max_loaded_size)
        })
        .await
    }

    // Proof of concept code for synchronous loads. Probably not needed, but it
    // was sufficiently non-obvious that we want to keep it around as an example
    // of how to do this if we need to.
    //
    // /// A synchronous, blocking version of [`Self::load`]. This is for use in
    // /// contexts like Handlebars, where we don't have async support.
    // ///
    // /// DEADLOCK WARNING: If you hold one [`ImageData`] on a worker thread, and
    // /// try to load another one before releasing the first, then you may deadlock
    // /// if another thread is doing the same thing.
    // pub fn load_blocking(
    //     &self,
    //     encoding: ImageEncoding,
    //     mem_limit: &MemLimit,
    // ) -> Result<ImageData> {
    //     // Acquire memory permits for final file size _before_ loading any data.
    //     let max_loaded_size =
    //         encoding.max_loaded_size(&self.mime_type, self.file_size()?);
    //     let mem_permit = mem_limit.acquire_blocking(max_loaded_size)?;

    //     self.load_helper(encoding, mem_permit, max_loaded_size)
    // }
}

/// A handle for a loaded image's data.
pub struct ImageData {
    /// The MIME type of the image, e.g. "image/png".
    mime_type: String,

    /// The encoding of the image data.
    #[allow(dead_code)]
    encoding: ImageEncoding,

    /// The raw bytes of the image data, encoded according to [`Self::encoding`].
    data: Vec<u8>,

    /// A permit for the RAM used by this image. This will be released when the
    /// `ImageData` is dropped, allowing other images to be loaded.
    _mem_permit: MemPermit,
}

impl ImageData {
    /// Get the MIME type of the image.
    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    /// Get the encoding of the image data.
    #[allow(dead_code)]
    pub fn encoding(&self) -> &ImageEncoding {
        &self.encoding
    }

    /// Get the raw bytes of the image data, encoded according to [`Self::encoding`].
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use base64::Engine as _;

    use super::*;

    // -- ImageFile URL round-trip tests --

    #[test]
    fn to_url_from_url_round_trip_simple_path() {
        let image = ImageFile {
            mime_type: "image/png".to_string(),
            path: PathBuf::from("/tmp/pages/page-1.png"),
        };
        let url = image.to_url();
        assert!(url.starts_with("file:///"));
        assert!(
            url.contains("mime_type=image%2Fpng") || url.contains("mime_type=image/png")
        );

        let parsed = ImageFile::from_url(&url).unwrap();
        assert_eq!(parsed.path, image.path);
        assert_eq!(parsed.mime_type, image.mime_type);
    }

    #[test]
    fn to_url_from_url_round_trip_special_chars() {
        let image = ImageFile {
            mime_type: "image/jpeg".to_string(),
            path: PathBuf::from("/tmp/my pages/file (1).jpg"),
        };
        let url = image.to_url();
        let parsed = ImageFile::from_url(&url).unwrap();
        assert_eq!(parsed.path, image.path);
        assert_eq!(parsed.mime_type, image.mime_type);
    }

    #[test]
    fn from_url_rejects_non_file_scheme() {
        let result =
            ImageFile::from_url("https://example.com/image.png?mime_type=image/png");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("file: URL scheme"));
    }

    #[test]
    fn from_url_rejects_malformed_url() {
        let result = ImageFile::from_url("not a url at all");
        assert!(result.is_err());
    }

    #[test]
    fn from_url_rejects_missing_mime_type() {
        let result = ImageFile::from_url("file:///tmp/image.png");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mime_type"));
    }

    // -- ImageEncoding::max_loaded_size tests --

    #[test]
    fn max_loaded_size_binary() {
        assert_eq!(
            ImageEncoding::Binary.max_loaded_size("image/png", 1000),
            1000
        );
    }

    #[test]
    fn max_loaded_size_base64() {
        // Base64 expands by 4/3, rounded up, plus padding.
        let size = ImageEncoding::Base64.max_loaded_size("image/png", 1000);
        assert!(size > 1000);
        assert!(size <= 1340); // ceil(1000/3)*4 + 4
    }

    #[test]
    fn max_loaded_size_data_url() {
        let base64_size = ImageEncoding::Base64.max_loaded_size("image/png", 1000);
        let data_url_size = ImageEncoding::DataUrl.max_loaded_size("image/png", 1000);
        // DataUrl adds "data:" + mime_type + ";base64," overhead.
        assert!(data_url_size > base64_size);
        let overhead = "data:".len() + "image/png".len() + ";base64,".len();
        assert_eq!(data_url_size, base64_size + overhead);
    }

    // -- ImageFile::load and ImageData tests --

    /// Create a temp file with known PNG-like content for load tests.
    fn create_test_image_file() -> (tempfile::NamedTempFile, ImageFile) {
        let mut tmp = tempfile::Builder::new().suffix(".bin").tempfile().unwrap();
        let content = b"fake image data for testing";
        tmp.write_all(content).unwrap();
        tmp.flush().unwrap();
        let image_file = ImageFile {
            mime_type: "image/png".to_string(),
            path: tmp.path().to_path_buf(),
        };
        (tmp, image_file)
    }

    #[tokio::test]
    async fn load_binary_encoding() {
        let (_tmp, image_file) = create_test_image_file();
        let limiter = MemLimiter::unlimited();
        let data = image_file
            .load(ImageEncoding::Binary, &limiter)
            .await
            .unwrap();
        assert_eq!(data.mime_type(), "image/png");
        assert_eq!(data.encoding(), &ImageEncoding::Binary);
        assert_eq!(data.data(), b"fake image data for testing");
    }

    #[tokio::test]
    async fn load_base64_encoding() {
        let (_tmp, image_file) = create_test_image_file();
        let limiter = MemLimiter::unlimited();
        let data = image_file
            .load(ImageEncoding::Base64, &limiter)
            .await
            .unwrap();
        assert_eq!(data.encoding(), &ImageEncoding::Base64);
        // Verify it's valid base64 that decodes back to original.
        let decoded = base64::prelude::BASE64_STANDARD
            .decode(data.data())
            .unwrap();
        assert_eq!(decoded, b"fake image data for testing");
    }

    #[tokio::test]
    async fn load_data_url_encoding() {
        let (_tmp, image_file) = create_test_image_file();
        let limiter = MemLimiter::unlimited();
        let data = image_file
            .load(ImageEncoding::DataUrl, &limiter)
            .await
            .unwrap();
        assert_eq!(data.encoding(), &ImageEncoding::DataUrl);
        let data_str = std::str::from_utf8(data.data()).unwrap();
        assert!(data_str.starts_with("data:image/png;base64,"));
    }
}
