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
