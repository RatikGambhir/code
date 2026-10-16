//! A file opened for editing.
//!
//! The editor always works with `\n` line endings; a file that used `\r\n` is
//! written back the way it came. Saves are atomic (write a sibling temp file,
//! then rename), so a crash mid-save never leaves a truncated file — which
//! matters when an agent and the user are both touching the same tree.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context as _, Result};

/// Files larger than this are refused rather than opened sluggishly.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    #[error("{} looks like a binary file", .0.display())]
    Binary(PathBuf),
    #[error("{} is too large to open ({} MB)", .0.display(), .1 / (1024 * 1024))]
    TooLarge(PathBuf, u64),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn detect(text: &str) -> Self {
        // The first line ending decides; mixed files keep LF.
        match text.find('\n') {
            Some(i) if i > 0 && text.as_bytes()[i - 1] == b'\r' => LineEnding::CrLf,
            _ => LineEnding::Lf,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
        }
    }
}

/// What we know about the file on disk, to notice changes made elsewhere.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Fingerprint {
    modified: Option<SystemTime>,
    len: u64,
}

impl Fingerprint {
    fn of(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        Some(Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        })
    }
}

#[derive(Debug)]
pub struct Document {
    path: PathBuf,
    /// Contents as last loaded or saved, with `\n` line endings.
    saved_text: String,
    line_ending: LineEnding,
    fingerprint: Option<Fingerprint>,
    language: &'static str,
}

impl Document {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let (text, line_ending) = read_text(&path)?;
        Ok(Self {
            language: crate::language_for_path(&path),
            fingerprint: Fingerprint::of(&path),
            path,
            saved_text: text,
            line_ending,
        })
    }

    /// A document for a file that does not exist yet.
    pub fn new_file(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self {
            language: crate::language_for_path(&path),
            path,
            saved_text: String::new(),
            line_ending: LineEnding::Lf,
            fingerprint: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    pub fn language(&self) -> &'static str {
        self.language
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// The text as last loaded or saved.
    pub fn saved_text(&self) -> &str {
        &self.saved_text
    }

    pub fn is_dirty(&self, current: &str) -> bool {
        current != self.saved_text
    }

    /// Writes `current` to disk atomically, in the file's original line
    /// ending, keeping its permissions.
    pub fn save(&mut self, current: &str) -> Result<()> {
        // Write through symlinks to the real file rather than replacing the link.
        let target = fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
        let dir = target
            .parent()
            .context("file has no parent directory")?
            .to_path_buf();
        fs::create_dir_all(&dir)?;

        let on_disk = match self.line_ending {
            LineEnding::Lf => current.to_string(),
            LineEnding::CrLf => current.replace('\n', "\r\n"),
        };

        let mut temp = tempfile::Builder::new()
            .prefix(".cockpit-save-")
            .tempfile_in(&dir)
            .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
        temp.write_all(on_disk.as_bytes())?;
        temp.as_file().sync_all()?;
        if let Ok(meta) = fs::metadata(&target) {
            fs::set_permissions(temp.path(), meta.permissions())?;
        }
        temp.persist(&target)
            .map_err(|err| err.error)
            .with_context(|| format!("saving {}", target.display()))?;

        self.saved_text = current.to_string();
        self.fingerprint = Fingerprint::of(&self.path);
        Ok(())
    }

    /// Whether the file on disk changed since we last read or wrote it.
    pub fn changed_on_disk(&self) -> bool {
        Fingerprint::of(&self.path) != self.fingerprint
    }

    /// Re-reads the file, returning the new text.
    pub fn reload(&mut self) -> Result<&str> {
        let (text, line_ending) = read_text(&self.path)?;
        self.saved_text = text;
        self.line_ending = line_ending;
        self.fingerprint = Fingerprint::of(&self.path);
        Ok(&self.saved_text)
    }

    /// Points the document at a new path (after a rename on disk).
    pub fn set_path(&mut self, path: PathBuf) {
        self.language = crate::language_for_path(&path);
        self.path = path;
        self.fingerprint = Fingerprint::of(&self.path);
    }
}

fn read_text(path: &Path) -> Result<(String, LineEnding)> {
    let meta = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(DocumentError::TooLarge(path.to_path_buf(), meta.len()).into());
    }
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if looks_binary(&bytes) {
        return Err(DocumentError::Binary(path.to_path_buf()).into());
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        // Not UTF-8: open it lossily rather than refusing; the user sees the
        // replacement characters and can decide.
        Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned(),
    };
    let text = text.strip_prefix('\u{feff}').map(str::to_string).unwrap_or(text);
    let line_ending = LineEnding::detect(&text);
    let text = if line_ending == LineEnding::CrLf {
        text.replace("\r\n", "\n")
    } else {
        text
    };
    Ok((text, line_ending))
}

/// A NUL byte in the first 8 KiB is what git uses to call a file binary.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_edit_save_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();

        let mut doc = Document::open(&path).unwrap();
        assert_eq!(doc.language(), "rust");
        assert!(!doc.is_dirty("fn main() {}\n"));
        assert!(doc.is_dirty("fn main() { println!(); }\n"));

        doc.save("fn main() { println!(); }\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "fn main() { println!(); }\n");
        assert!(!doc.changed_on_disk());
        // No temp files are left behind.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn crlf_files_keep_their_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("win.txt");
        fs::write(&path, "a\r\nb\r\n").unwrap();
        let mut doc = Document::open(&path).unwrap();
        assert_eq!(doc.line_ending(), LineEnding::CrLf);
        assert_eq!(doc.saved_text(), "a\nb\n");
        doc.save("a\nb\nc\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"a\r\nb\r\nc\r\n");
    }

    #[test]
    fn external_changes_are_detected_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        fs::write(&path, "one").unwrap();
        let mut doc = Document::open(&path).unwrap();
        assert!(!doc.changed_on_disk());
        fs::write(&path, "two, longer").unwrap();
        assert!(doc.changed_on_disk());
        assert_eq!(doc.reload().unwrap(), "two, longer");
        assert!(!doc.changed_on_disk());
    }

    #[test]
    fn binary_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.bin");
        fs::write(&path, [0x89, b'P', b'N', b'G', 0, 0, 1]).unwrap();
        let err = Document::open(&path).unwrap_err();
        assert!(err.downcast_ref::<DocumentError>().is_some());
    }

    #[test]
    fn saving_keeps_permissions() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("run.sh");
            fs::write(&path, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            let mut doc = Document::open(&path).unwrap();
            doc.save("#!/bin/sh\necho hi\n").unwrap();
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755);
        }
    }

    #[test]
    fn saving_through_a_symlink_keeps_the_link() {
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().unwrap();
            let real = dir.path().join("real.txt");
            let link = dir.path().join("link.txt");
            fs::write(&real, "x").unwrap();
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let mut doc = Document::open(&link).unwrap();
            doc.save("y").unwrap();
            assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
            assert_eq!(fs::read_to_string(&real).unwrap(), "y");
        }
    }

    #[test]
    fn new_files_are_created_on_first_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/new.md");
        let mut doc = Document::new_file(&path);
        assert_eq!(doc.language(), "markdown");
        doc.save("# hi\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "# hi\n");
    }
}
