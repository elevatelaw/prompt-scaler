//! Converting binary data to a `data:` URL.

use base64::{Engine as _, prelude::BASE64_STANDARD};
//use ::utf8_percent_encode;

/// Convert binary data to a `data:` URL.
pub fn data_url(mime_type: &str, data: &[u8]) -> String {
    let base64_data = BASE64_STANDARD.encode(data);
    // Some sources indicate that the Base64 data should be percent-encoded, but
    // in practice this breaks Gemini and probably several other LLMs.
    //
    // let percent_encoded =
    //     percent_encoding::utf8_percent_encode(&base64_data, percent_encoding::NON_ALPHANUMERIC);
    format!("data:{};base64,{}", mime_type, base64_data)
}

/// Regex for parsing a `data:` URL.
pub const DATA_URL_RE: &str = r"^data:(?P<mime_type>[^;]+);base64,(?P<data>.+)$";

/// Parse a `data:` URL into a MIME type and Base64-encoded data.
///
/// TODO: This is less than ideal when working with lots of huge images, but
/// improving it would require changes to how we handle images in prompt
/// templates.
pub fn parse_data_url(data_url: &str) -> Option<(String, &str)> {
    let re = regex::Regex::new(DATA_URL_RE).ok()?;
    let caps = re.captures(data_url)?;
    let mime_type = caps.name("mime_type")?.as_str().to_string();
    let data = caps.name("data")?.as_str();
    Some((mime_type, data))
}
