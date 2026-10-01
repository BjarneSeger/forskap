//! Project avatars as local files: desktop launchers want a file, and a
//! private project's avatar is only readable with the token.
//!
//! The sync worker is the only writer of the directory. Each fetch leaves an
//! [`Avatar`] row naming its file, so reads never look at the filesystem.
//!
//! When to download and what to call the file are two questions. GitLab
//! appends `?v=<updated_at>` to a project's avatar URL, so the URL changes
//! with any update of the project, and it is also the only sign of a new
//! image uploaded under the same file name: the job downloads whenever the
//! URL changes. The file is named by its bytes ([`file_name`]), so its path
//! changes only when the image does, and a launcher caching images by path
//! keeps its cache. Files named before (by the job's fingerprint) keep
//! their name until their project's next download moves them.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::warn;
use xxhash_rust::xxh3::xxh3_64;

use super::model::{Resource, RowKey};

/// Largest image kept. GitLab caps avatars at 200 KiB itself.
pub const MAX_BYTES: usize = 1 << 20;

/// What the last fetch of a project's avatar left behind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Avatar {
    pub project_id: i64,
    /// File name inside the avatar directory; empty when GitLab had no
    /// usable image.
    #[serde(default)]
    pub file: String,
}

impl Resource for Avatar {
    const NAME: &'static str = "avatars";
    const KEYSPACE: &'static str = "sync_avatars_v1";
    const SCHEMA: u32 = 1;
    fn key(&self) -> RowKey {
        (self.project_id.max(0) as u64, 0)
    }
    fn is_valid(&self) -> bool {
        self.project_id > 0
    }
}

/// The file extension of the image format `bytes` holds, by its magic
/// numbers; `None` for anything a launcher couldn't load.
pub fn extension(bytes: &[u8]) -> Option<&'static str> {
    let ext = match bytes {
        [0x89, b'P', b'N', b'G', ..] => "png",
        [0xff, 0xd8, 0xff, ..] => "jpg",
        [b'G', b'I', b'F', b'8', ..] => "gif",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => "webp",
        [0, 0, 1, 0, ..] => "ico",
        [b'B', b'M', ..] => "bmp",
        [b'I', b'I', 42, 0, ..] | [b'M', b'M', 0, 42, ..] => "tiff",
        _ if is_svg(bytes) => "svg",
        _ => return None,
    };
    Some(ext)
}

/// Markup whose head opens an `<svg` element.
fn is_svg(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]);
    let head = head.trim_start_matches('\u{feff}').trim_start();
    head.starts_with('<') && head.contains("<svg")
}

/// The file for `image` as a project's avatar, named by its content: the
/// same image downloaded again lands on the same path, a new one on a new
/// path, so no launcher shows a cached old one. `None` for an image a
/// launcher couldn't show: too large or of no known format.
///
/// XXH3 is stable across runs and platforms (its output is specified), and
/// only images of one project can meet on a name, so 64 bits make an
/// accidental collision negligible. A crafted one would only keep the path
/// of a changed image: the file still gets the new bytes.
pub fn file_name(project_id: i64, image: &[u8]) -> Option<String> {
    let ext = extension(image).filter(|_| image.len() <= MAX_BYTES)?;
    Some(format!("{project_id}-{:016x}.{ext}", xxh3_64(image)))
}

/// The directory the avatar files live in.
#[derive(Debug, Clone)]
pub struct AvatarDir(Arc<Path>);

impl AvatarDir {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self(dir.into().into())
    }

    pub fn path_of(&self, file: &str) -> PathBuf {
        self.0.join(file)
    }

    /// Write `file` whole or not at all: a launcher may read it any time. A
    /// file already holding `bytes` is left alone, so downloading the same
    /// image again changes nothing a launcher could notice.
    pub fn write(&self, file: &str, bytes: &[u8]) -> io::Result<()> {
        if self.holds(file, bytes) {
            return Ok(());
        }
        std::fs::create_dir_all(&self.0)?;
        let tmp = self.0.join(format!(".{file}.tmp"));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, self.path_of(file)).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    pub fn exists(&self, file: &str) -> bool {
        self.path_of(file).is_file()
    }

    /// Whether `file` exists and holds exactly `bytes`.
    fn holds(&self, file: &str, bytes: &[u8]) -> bool {
        let path = self.path_of(file);
        let same_size =
            std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() == bytes.len() as u64);
        same_size && std::fs::read(&path).is_ok_and(|held| held == bytes)
    }

    pub fn remove(&self, file: &str) {
        if let Err(e) = std::fs::remove_file(self.path_of(file))
            && e.kind() != io::ErrorKind::NotFound
        {
            warn!(error = %e, file, "removing an avatar file failed");
        }
    }

    /// Remove every file `keep` doesn't name; returns how many.
    pub fn sweep(&self, keep: &HashSet<String>) -> usize {
        let entries = match std::fs::read_dir(&self.0) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return 0,
            Err(e) => {
                warn!(error = %e, "listing the avatar directory failed");
                return 0;
            }
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let kept = name.to_str().is_some_and(|n| keep.contains(n));
            if !kept && entry.file_type().is_ok_and(|t| t.is_file()) {
                match std::fs::remove_file(entry.path()) {
                    Ok(()) => removed += 1,
                    Err(e) => warn!(error = %e, file = ?name, "removing an avatar file failed"),
                }
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_are_told_by_their_magic_numbers() {
        let cases: [(&[u8], Option<&str>); 12] = [
            (b"\x89PNG\r\n\x1a\n....", Some("png")),
            (b"\xff\xd8\xff\xe0..JFIF", Some("jpg")),
            (b"GIF89a....", Some("gif")),
            (b"RIFF\x10\0\0\0WEBPVP8 ", Some("webp")),
            (b"\0\0\x01\0\x01\0", Some("ico")),
            (b"BM\x10\0\0\0", Some("bmp")),
            (b"II*\0\x08\0", Some("tiff")),
            (b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>", Some("svg")),
            (
                b"\xef\xbb\xbf<?xml version=\"1.0\"?>\n<!-- logo -->\n<svg/>",
                Some("svg"),
            ),
            (b"<html><body>Sign in</body></html>", None),
            (b"RIFF\x10\0\0\0WAVEfmt ", None),
            (b"", None),
        ];
        for (bytes, ext) in cases {
            assert_eq!(
                extension(bytes),
                ext,
                "{:?}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    /// Names are stored in the rows and handed to the launchers: a hash
    /// that changed would move every file once more at its next download.
    #[test]
    fn the_file_name_follows_the_image_and_nothing_else() {
        let png = b"\x89PNG\r\n\x1a\n";
        assert_eq!(file_name(7, png).as_deref(), Some("7-95654b8f73afb947.png"));
        assert_ne!(file_name(7, png), file_name(8, png), "one per project");
        let other = b"\x89PNG\r\n\x1a\n\0";
        assert_ne!(file_name(7, png), file_name(7, other));
        assert_eq!(
            file_name(7, b"GIF89a").unwrap().rsplit_once('.').unwrap().1,
            "gif"
        );

        let mut huge = png.to_vec();
        huge.resize(MAX_BYTES + 1, 0);
        assert_eq!(file_name(7, &huge), None);
        assert_eq!(file_name(7, b"<html>Sign in</html>"), None);
        assert_eq!(file_name(7, b""), None);
    }

    /// Downloading the same image again must not touch its file: a
    /// launcher watching it would load it again.
    #[test]
    fn writing_the_bytes_a_file_holds_leaves_it_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = AvatarDir::new(tmp.path().join("avatars"));
        dir.write("7-1.png", b"one").unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let set_old = || {
            let f = std::fs::File::options()
                .write(true)
                .open(dir.path_of("7-1.png"))
                .unwrap();
            f.set_modified(old).unwrap();
        };
        let modified = || {
            std::fs::metadata(dir.path_of("7-1.png"))
                .unwrap()
                .modified()
                .unwrap()
        };
        set_old();

        dir.write("7-1.png", b"one").unwrap();
        assert_eq!(modified(), old, "the same bytes: untouched");

        dir.write("7-1.png", b"two").unwrap();
        assert_eq!(std::fs::read(dir.path_of("7-1.png")).unwrap(), b"two");
        assert_ne!(modified(), old, "same size, other bytes: replaced");

        set_old();
        dir.write("7-1.png", b"three").unwrap();
        assert_eq!(std::fs::read(dir.path_of("7-1.png")).unwrap(), b"three");
        assert_ne!(modified(), old, "another size: replaced");
    }

    #[test]
    fn writes_replace_whole_files_and_sweeps_drop_the_unlisted() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = AvatarDir::new(tmp.path().join("avatars"));
        assert_eq!(dir.sweep(&HashSet::new()), 0, "no directory yet");

        dir.write("7-1.png", b"one").unwrap();
        dir.write("7-1.png", b"two").unwrap();
        dir.write("8-1.png", b"x").unwrap();
        std::fs::write(dir.path_of(".9-1.png.tmp"), b"torn").unwrap();
        assert_eq!(std::fs::read(dir.path_of("7-1.png")).unwrap(), b"two");

        let keep = HashSet::from(["7-1.png".to_string()]);
        assert_eq!(dir.sweep(&keep), 2);
        assert!(dir.exists("7-1.png") && !dir.exists("8-1.png"));
        assert!(!dir.exists(".9-1.png.tmp"), "a torn write goes too");

        dir.remove("7-1.png");
        dir.remove("7-1.png");
        assert!(!dir.exists("7-1.png"));
    }
}
