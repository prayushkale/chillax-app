//! The attachment file store: `.chillax/state/attachments/<sha256>.<ext>`.
//!
//! Content-addressed, so the same image attached twice is one file. The type comes from
//! the bytes (magic numbers), never from the name or the client's content type.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use chillax_core::attachment::{
    Attachment, AttachmentError, AttachmentStore, MAX_ATTACHMENT_BYTES, clean_name, ext_for_mime,
    is_valid_id, mime_for_ext, sniff,
};
use sha2::{Digest, Sha256};

pub struct FileAttachmentStore {
    dir: PathBuf,
}

impl FileAttachmentStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn io(err: std::io::Error) -> AttachmentError {
        AttachmentError::Io(err.to_string())
    }

    fn find(&self, id: &str) -> Result<(PathBuf, &'static str), AttachmentError> {
        if !is_valid_id(id) {
            return Err(AttachmentError::NotFound);
        }
        for ext in ["png", "jpg", "gif", "webp"] {
            let path = self.dir.join(format!("{id}.{ext}"));
            if path.is_file() {
                return Ok((path, mime_for_ext(ext).expect("known extension")));
            }
        }
        Err(AttachmentError::NotFound)
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl AttachmentStore for FileAttachmentStore {
    fn put(&self, name: &str, bytes: &[u8]) -> Result<Attachment, AttachmentError> {
        if bytes.is_empty() || bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
            return Err(AttachmentError::BadSize);
        }
        let kind = sniff(bytes).ok_or(AttachmentError::BadType)?;
        let id = hex_sha256(bytes);
        let path = self.dir.join(format!("{id}.{}", kind.ext));
        if !path.is_file() {
            fs::create_dir_all(&self.dir).map_err(Self::io)?;
            // Write beside, then rename, so a reader never sees half a file.
            let tmp = self.dir.join(format!(".{id}.tmp"));
            fs::write(&tmp, bytes).map_err(Self::io)?;
            fs::rename(&tmp, &path).map_err(Self::io)?;
        }
        Ok(Attachment {
            id,
            name: clean_name(name),
            mime: kind.mime.to_string(),
            bytes: bytes.len() as u64,
        })
    }

    fn import(
        &self,
        path: &Path,
        roots: Option<&[PathBuf]>,
    ) -> Result<Attachment, AttachmentError> {
        // Resolve symlinks first: the check is on where the bytes really are.
        let real = fs::canonicalize(path)
            .map_err(|e| AttachmentError::Io(format!("{}: {e}", path.display())))?;
        if let Some(roots) = roots {
            let inside = roots
                .iter()
                .filter_map(|r| fs::canonicalize(r).ok())
                .any(|r| real.starts_with(r));
            if !inside {
                return Err(AttachmentError::OutsideRoots(path.display().to_string()));
            }
        }
        let meta = fs::metadata(&real).map_err(Self::io)?;
        if !meta.is_file() {
            return Err(AttachmentError::Io(format!(
                "{} is not a file",
                path.display()
            )));
        }
        if meta.len() == 0 || meta.len() > MAX_ATTACHMENT_BYTES {
            return Err(AttachmentError::BadSize);
        }
        let mut bytes = Vec::with_capacity(meta.len() as usize);
        fs::File::open(&real)
            .and_then(|f| f.take(MAX_ATTACHMENT_BYTES + 1).read_to_end(&mut bytes))
            .map_err(Self::io)?;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("image");
        self.put(name, &bytes)
    }

    fn stat(&self, id: &str) -> Result<Attachment, AttachmentError> {
        let (path, mime) = self.find(id)?;
        let bytes = fs::metadata(path).map_err(Self::io)?.len();
        debug_assert!(ext_for_mime(mime).is_some());
        Ok(Attachment {
            id: id.to_string(),
            name: id.to_string(),
            mime: mime.to_string(),
            bytes,
        })
    }

    fn path(&self, id: &str) -> Result<PathBuf, AttachmentError> {
        self.find(id).map(|(p, _)| p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nimage-data";

    fn store() -> (tempfile::TempDir, FileAttachmentStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = FileAttachmentStore::new(dir.path().join("attachments"));
        (dir, s)
    }

    #[test]
    fn put_round_trips_and_dedupes() {
        let (_d, s) = store();
        let a = s.put("shot.png", PNG).expect("stores");
        let b = s.put("other-name.png", PNG).expect("stores again");
        assert_eq!(a.id, b.id);
        assert_eq!((a.mime.as_str(), a.bytes), ("image/png", PNG.len() as u64));
        let stat = s.stat(&a.id).expect("stat");
        assert_eq!(stat.mime, "image/png");
        assert_eq!(fs::read(s.path(&a.id).expect("path")).expect("read"), PNG);
        assert_eq!(fs::read_dir(&s.dir).expect("dir").count(), 1);
    }

    #[test]
    fn the_extension_is_not_trusted() {
        let (_d, s) = store();
        // A script named like an image is refused, and a png named .txt is a png.
        assert!(matches!(
            s.put("evil.png", b"#!/bin/sh\necho hi"),
            Err(AttachmentError::BadType)
        ));
        assert!(matches!(
            s.put("a.svg", b"<svg onload=alert(1)/>"),
            Err(AttachmentError::BadType)
        ));
        assert_eq!(s.put("a.txt", PNG).expect("png").mime, "image/png");
    }

    #[test]
    fn sizes_are_capped() {
        let (_d, s) = store();
        assert!(matches!(s.put("a.png", b""), Err(AttachmentError::BadSize)));
        let mut big = PNG.to_vec();
        big.resize(MAX_ATTACHMENT_BYTES as usize + 1, 0);
        assert!(matches!(
            s.put("a.png", &big),
            Err(AttachmentError::BadSize)
        ));
    }

    #[test]
    fn ids_cannot_walk_the_disk() {
        let (_d, s) = store();
        for bad in ["../../etc/passwd", "..", "", "A".repeat(64).as_str()] {
            assert!(matches!(s.stat(bad), Err(AttachmentError::NotFound)));
            assert!(matches!(s.path(bad), Err(AttachmentError::NotFound)));
        }
        assert!(matches!(
            s.stat(&"a".repeat(64)),
            Err(AttachmentError::NotFound)
        ));
    }

    #[test]
    fn import_stays_inside_the_roots() {
        let (d, s) = store();
        let work = d.path().join("work");
        fs::create_dir_all(&work).expect("mkdir");
        fs::write(work.join("ok.png"), PNG).expect("write");
        let outside = d.path().join("secret.png");
        fs::write(&outside, PNG).expect("write");
        let roots = [work.clone()];

        let a = s
            .import(&work.join("ok.png"), Some(&roots))
            .expect("inside");
        assert_eq!(a.name, "ok.png");
        // `..` back out is refused once resolved.
        assert!(matches!(
            s.import(&work.join("../secret.png"), Some(&roots)),
            Err(AttachmentError::OutsideRoots(_))
        ));
        assert!(s.import(&outside, None).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn import_refuses_a_symlink_that_escapes() {
        let (d, s) = store();
        let work = d.path().join("work");
        fs::create_dir_all(&work).expect("mkdir");
        let outside = d.path().join("secret.png");
        fs::write(&outside, PNG).expect("write");
        std::os::unix::fs::symlink(&outside, work.join("link.png")).expect("symlink");
        assert!(matches!(
            s.import(&work.join("link.png"), Some(std::slice::from_ref(&work))),
            Err(AttachmentError::OutsideRoots(_))
        ));
    }
}
