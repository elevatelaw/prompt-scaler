//! TIFF processing helpers

use std::{
    fs,
    io::{BufReader, Cursor},
};

use ::tiff::{
    ColorType,
    decoder::{Decoder, DecodingResult, ifd::Value},
    tags::{IfdPointer, Tag},
};
use anyhow::anyhow;
use image::{DynamicImage, GrayImage, ImageFormat, RgbImage, RgbaImage};

use crate::prelude::*;

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

/// Result of processing a TIFF file.
pub struct ProcessedTiffResult {
    /// Temporary directory containing PNG pages.
    pub tmpdir: tempfile::TempDir,
    /// Total number of pages in the TIFF file.
    pub total_pages: usize,
    /// Warnings encountered during processing.
    pub warnings: Vec<String>,
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
pub fn process_tiff_sync(
    path: &Path,
    max_pages: Option<usize>,
) -> Result<ProcessedTiffResult> {
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
        decoder.next_image()?;
        total_pages += 1;
    }

    debug!(
        path = %path.display(),
        page_count = page_count,
        total_pages = total_pages,
        "Processed multipage TIFF"
    );

    Ok(ProcessedTiffResult {
        tmpdir,
        total_pages,
        warnings,
    })
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

/// Convert CMYK u8 pixel to RGB u8.
fn cmyk_to_rgb(cmyk: &[u8]) -> [u8; 3] {
    let c = cmyk[0] as u16;
    let m = cmyk[1] as u16;
    let y = cmyk[2] as u16;
    let k = cmyk[3] as u16;
    let r = ((255 - c) * (255 - k) / 255) as u8;
    let g = ((255 - m) * (255 - k) / 255) as u8;
    let b = ((255 - y) * (255 - k) / 255) as u8;
    [r, g, b]
}

/// Convert CMYK u16 pixel to RGB u16.
fn cmyk16_to_rgb(cmyk: &[u16]) -> [u16; 3] {
    let c = cmyk[0] as u32;
    let m = cmyk[1] as u32;
    let y = cmyk[2] as u32;
    let k = cmyk[3] as u32;
    let r = ((65535 - c) * (65535 - k) / 65535) as u16;
    let g = ((65535 - m) * (65535 - k) / 65535) as u16;
    let b = ((65535 - y) * (65535 - k) / 65535) as u16;
    [r, g, b]
}

/// Decode a TIFF image from the current IFD to a DynamicImage.
///
/// Note that we do not handle all possible TIFF color types, including [`ColorType::GrayA`]
/// and [`ColorType::Palette`]. These types were not supported by the underlying `tiff` decoder
/// at the time of writing, and so we return an error.
///
/// We ignore color profiles, since we're just feeding these images to an LLM, mostly for OCR.
fn decode_tiff_image<R: std::io::Read + std::io::Seek>(
    decoder: &mut tiff::decoder::Decoder<R>,
    width: u32,
    height: u32,
    path: &Path,
    ifd_index: usize,
) -> Result<image::DynamicImage> {
    let color_type: ColorType = decoder.colortype().with_context(|| {
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
        DecodingResult::U8(data) => {
            decode_8_bit_tiff_image(width, height, path, ifd_index, color_type, data)?
        }
        DecodingResult::U16(data) => {
            decode_16_bit_tiff_image(width, height, path, ifd_index, color_type, data)?
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

/// Decode an 8-bit-per-channel TIFF image to a DynamicImage.
fn decode_8_bit_tiff_image(
    width: u32,
    height: u32,
    path: &Path,
    ifd_index: usize,
    color_type: ColorType,
    data: Vec<u8>,
) -> Result<DynamicImage> {
    Ok(match color_type {
        ColorType::Gray(_) => {
            let gray = GrayImage::from_raw(width, height, data)
                .ok_or_else(|| image_creation_error("grayscale", ifd_index, path))?;
            DynamicImage::ImageLuma8(gray)
        }
        ColorType::RGB(_) => {
            let rgb = RgbImage::from_raw(width, height, data)
                .ok_or_else(|| image_creation_error("RGB", ifd_index, path))?;
            DynamicImage::ImageRgb8(rgb)
        }
        ColorType::RGBA(_) => {
            let rgba = RgbaImage::from_raw(width, height, data)
                .ok_or_else(|| image_creation_error("RGBA", ifd_index, path))?;
            DynamicImage::ImageRgba8(rgba)
        }
        ColorType::CMYK(_) => {
            let rgb_data: Vec<u8> = data
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|cmyk| cmyk_to_rgb(cmyk))
                .collect();
            let rgb = RgbImage::from_raw(width, height, rgb_data)
                .ok_or_else(|| image_creation_error("CMYK→RGB", ifd_index, path))?;
            DynamicImage::ImageRgb8(rgb)
        }
        ColorType::CMYKA(_) => {
            let rgba_data: Vec<u8> = data
                .as_chunks::<5>()
                .0
                .iter()
                .flat_map(|cmyka| {
                    let [r, g, b] = cmyk_to_rgb(&cmyka[..4]);
                    [r, g, b, cmyka[4]]
                })
                .collect();
            let rgba = RgbaImage::from_raw(width, height, rgba_data)
                .ok_or_else(|| image_creation_error("CMYKA→RGBA", ifd_index, path))?;
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
    })
}

/// Decode a 16-bit-per-channel TIFF image to a DynamicImage.
fn decode_16_bit_tiff_image(
    width: u32,
    height: u32,
    path: &Path,
    ifd_index: usize,
    color_type: ColorType,
    data: Vec<u16>,
) -> Result<DynamicImage> {
    let to_u8 = |value: u16| -> u8 { (value >> 8) as u8 };

    Ok(match color_type {
        ColorType::Gray(_) => {
            let data_u8: Vec<u8> = data.into_iter().map(to_u8).collect();
            let gray = GrayImage::from_raw(width, height, data_u8).ok_or_else(|| {
                image_creation_error("16-bit grayscale", ifd_index, path)
            })?;
            DynamicImage::ImageLuma8(gray)
        }
        ColorType::RGB(_) => {
            let data_u8: Vec<u8> = data.into_iter().map(to_u8).collect();
            let rgb = RgbImage::from_raw(width, height, data_u8)
                .ok_or_else(|| image_creation_error("16-bit RGB", ifd_index, path))?;
            DynamicImage::ImageRgb8(rgb)
        }
        ColorType::RGBA(_) => {
            let data_u8: Vec<u8> = data.into_iter().map(to_u8).collect();
            let rgba = RgbaImage::from_raw(width, height, data_u8)
                .ok_or_else(|| image_creation_error("16-bit RGBA", ifd_index, path))?;
            DynamicImage::ImageRgba8(rgba)
        }
        ColorType::CMYK(_) => {
            let rgb_data: Vec<u8> = data
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|cmyk| {
                    let [r, g, b] = cmyk16_to_rgb(cmyk);
                    [to_u8(r), to_u8(g), to_u8(b)]
                })
                .collect();
            let rgb = RgbImage::from_raw(width, height, rgb_data).ok_or_else(|| {
                image_creation_error("16-bit CMYK→RGB", ifd_index, path)
            })?;
            DynamicImage::ImageRgb8(rgb)
        }
        ColorType::CMYKA(_) => {
            let rgba_data: Vec<u8> = data
                .as_chunks::<5>()
                .0
                .iter()
                .flat_map(|cmyka| {
                    let [r, g, b] = cmyk16_to_rgb(&cmyka[..4]);
                    [to_u8(r), to_u8(g), to_u8(b), to_u8(cmyka[4])]
                })
                .collect();
            let rgba =
                RgbaImage::from_raw(width, height, rgba_data).ok_or_else(|| {
                    image_creation_error("16-bit CMYKA→RGBA", ifd_index, path)
                })?;
            DynamicImage::ImageRgba8(rgba)
        }
        other => {
            return Err(anyhow!(
                "unsupported 16-bit TIFF color type {:?} in IFD {} of {:?}",
                other,
                ifd_index,
                path.display()
            ));
        }
    })
}

/// Build an error for when `ImageBuffer::from_raw` returns `None`.
fn image_creation_error(
    image_type: &str,
    ifd_index: usize,
    path: &Path,
) -> anyhow::Error {
    anyhow!(
        "failed to create {} image for IFD {} in {:?}",
        image_type,
        ifd_index,
        path.display()
    )
}
