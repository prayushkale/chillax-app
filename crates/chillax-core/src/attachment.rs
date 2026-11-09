//! Image attachments on ticket comments.
//!
//! The core only carries metadata. The bytes live in an [`AttachmentStore`] (a file store
//! in `chillax-store`), content-addressed by SHA-256 so duplicates share one file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::CommandError;

/// Largest single attachment.
pub const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;
/// Most attachments on one comment.
pub const MAX_ATTACHMENTS_PER_COMMENT: usize = 6;

/// An image attached to a comment. `id` is the lowercase hex SHA-256 of the bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub bytes: u64,
}

impl Attachment {
    /// Human size such as `12 KB`.
    pub fn size_label(&self) -> String {
        match self.bytes {
            b if b < 1024 => format!("{b} B"),
            b if b < 1024 * 1024 => format!("{} KB", b / 1024),
            b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
        }
    }
}

/// A recognised image type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageKind {
    pub mime: &'static str,
    pub ext: &'static str,
}

/// The allowed types, by magic bytes only: the file name and the client's content type
/// are never trusted.
pub fn sniff(bytes: &[u8]) -> Option<ImageKind> {
    let kind = |mime, ext| Some(ImageKind { mime, ext });
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        kind("image/png", "png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        kind("image/jpeg", "jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        kind("image/gif", "gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        kind("image/webp", "webp")
    } else {
        None
    }
}

/// The extension for an allowed mime type.
pub fn ext_for_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

/// The mime type for a stored file's extension.
pub fn mime_for_ext(ext: &str) -> Option<&'static str> {
    match ext {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// True for a 64-character lowercase hex id; the only shape that reaches the disk.
pub fn is_valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A file name with no path parts or control characters, at most 120 characters.
pub fn clean_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base.chars().filter(|c| !c.is_control()).take(120).collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "image".into()
    } else {
        cleaned
    }
}

/// Checks the engine runs on a comment's attachments.
pub fn validate(attachments: &[Attachment]) -> Result<(), CommandError> {
    if attachments.len() > MAX_ATTACHMENTS_PER_COMMENT {
        return Err(CommandError::Invalid(format!(
            "a comment takes at most {MAX_ATTACHMENTS_PER_COMMENT} attachments"
        )));
    }
    for a in attachments {
        if !is_valid_id(&a.id) {
            return Err(CommandError::Invalid(format!(
                "attachment {:?} has a bad id",
                a.name
            )));
        }
        if ext_for_mime(&a.mime).is_none() {
            return Err(CommandError::Invalid(format!(
                "attachment {:?} is not an allowed image type ({})",
                a.name, a.mime
            )));
        }
        if a.bytes == 0 || a.bytes > MAX_ATTACHMENT_BYTES {
            return Err(CommandError::Invalid(format!(
                "attachment {:?} must be 1 byte to {} MB",
                a.name,
                MAX_ATTACHMENT_BYTES / 1024 / 1024
            )));
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    #[error("not an allowed image (png, jpeg, gif or webp)")]
    BadType,
    #[error("attachment is empty or larger than {} MB", MAX_ATTACHMENT_BYTES / 1024 / 1024)]
    BadSize,
    #[error("attachment not found")]
    NotFound,
    #[error("{0} is outside the folders an agent may attach from")]
    OutsideRoots(String),
    #[error("{0}")]
    Io(String),
}

/// Where attachment bytes live. Synchronous on purpose: callers in async code use
/// `spawn_blocking`.
pub trait AttachmentStore: Send + Sync {
    /// Validate (by magic bytes and size) and store; duplicates share one file.
    fn put(&self, name: &str, bytes: &[u8]) -> Result<Attachment, AttachmentError>;
    /// Copy in a file from disk. With `roots`, the file's real path (symlinks resolved)
    /// must lie inside one of them.
    fn import(&self, path: &Path, roots: Option<&[PathBuf]>)
    -> Result<Attachment, AttachmentError>;
    /// Metadata of a stored attachment (the name is not stored; it comes back as the id).
    fn stat(&self, id: &str) -> Result<Attachment, AttachmentError>;
    /// The stored file's path, for serving or opening in a viewer.
    fn path(&self, id: &str) -> Result<PathBuf, AttachmentError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

    fn att(id: &str, mime: &str, bytes: u64) -> Attachment {
        Attachment {
            id: id.into(),
            name: "a.png".into(),
            mime: mime.into(),
            bytes,
        }
    }

    #[test]
    fn sniffs_by_magic_bytes() {
        assert_eq!(sniff(PNG).map(|k| k.mime), Some("image/png"));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]).map(|k| k.ext), Some("jpg"));
        assert_eq!(sniff(b"GIF89a..").map(|k| k.ext), Some("gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 ").map(|k| k.ext), Some("webp"));
        assert_eq!(sniff(b"<svg></svg>"), None);
        assert_eq!(sniff(b"MZ\x90\0"), None);
    }

    #[test]
    fn ids_are_hex_only() {
        assert!(is_valid_id(&"a".repeat(64)));
        assert!(!is_valid_id(&"A".repeat(64)));
        assert!(!is_valid_id("../../etc/passwd"));
        assert!(!is_valid_id(&"a".repeat(63)));
    }

    #[test]
    fn names_lose_their_path() {
        assert_eq!(clean_name("../../x/shot.png"), "shot.png");
        assert_eq!(clean_name("C:\\tmp\\a.png"), "a.png");
        assert_eq!(clean_name("  "), "image");
    }

    #[test]
    fn limits_are_enforced() {
        let id = "a".repeat(64);
        assert!(validate(&[att(&id, "image/png", 10)]).is_ok());
        assert!(validate(&[att(&id, "image/svg+xml", 10)]).is_err());
        assert!(validate(&[att(&id, "image/png", MAX_ATTACHMENT_BYTES + 1)]).is_err());
        assert!(validate(&[att("zz", "image/png", 10)]).is_err());
        let many: Vec<_> = (0..7).map(|_| att(&id, "image/png", 1)).collect();
        assert!(validate(&many).is_err());
    }
}
