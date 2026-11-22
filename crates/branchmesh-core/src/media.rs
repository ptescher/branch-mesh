//! Binary media (images) stored by content hash, outside the JSON object
//! store. Messages reference media by [`MediaRef`]; the bytes are only read
//! when a conversation is sent to a model or shown in a UI, so branch
//! history and the execution journal stay small.

use serde::{Deserialize, Serialize};

/// Largest image accepted (the Messages API limit per image).
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Image formats models accept, by media type.
pub const IMAGE_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/gif", "image/webp"];

/// A stored piece of media.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRef {
    /// SHA-256 of the bytes, hex.
    pub hash: String,
    pub media_type: String,
    pub size: u64,
}

impl MediaRef {
    /// The content block stored in a message for this image. Turned into a
    /// base64 image block only when the conversation is sent to a model.
    pub fn image_block(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "image",
            "source": { "type": "media", "hash": self.hash, "media_type": self.media_type },
        })
    }
}

/// The image type of `bytes`, from their signature (never trust a declared type).
pub fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Some("image/png"),
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

/// Check that `bytes` are a supported image within the size limit; returns its media type.
pub fn validate_image(bytes: &[u8]) -> crate::Result<&'static str> {
    if bytes.is_empty() {
        return Err(crate::Error::Invalid("empty image".into()));
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(crate::Error::Invalid(format!(
            "image is {:.1} MB; the limit is {} MB",
            bytes.len() as f64 / 1_048_576.0,
            MAX_IMAGE_BYTES / 1_048_576
        )));
    }
    sniff_image(bytes).ok_or_else(|| crate::Error::Invalid("not a JPEG, PNG, GIF or WebP image".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_formats() {
        assert_eq!(sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff_image(b"GIF89a.."), Some("image/gif"));
        assert_eq!(sniff_image(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_image(b"<svg xmlns="), None);
        assert!(validate_image(&[]).is_err());
        assert!(validate_image(&vec![0xFF; MAX_IMAGE_BYTES + 1]).is_err());
    }
}
