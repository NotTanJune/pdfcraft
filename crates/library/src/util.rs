//! Small file and clock helpers shared by the library and the vault.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{LibraryError, Result};

/// Largest file read into memory (a document, the index, a vault file): 512 MiB.
pub const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;

/// Milliseconds since 1970-01-01 UTC, from the system clock (0 when the clock is before then).
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// `n` random bytes from the system.
pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).map_err(|e| LibraryError::Random(e.to_string()))?;
    Ok(out)
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 15)]));
    }
    s
}

/// A random id of `N` bytes, as `2 * N` hex digits.
pub fn random_id<const N: usize>() -> Result<String> {
    Ok(hex(&random_bytes::<N>()?))
}

/// SHA-256 of `bytes`, in hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(bytes))
}

/// Reads `path` whole, refusing a file past `max` bytes without buffering it.
pub fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let io = |e: std::io::Error| LibraryError::Io(format!("{}: {e}", path.display()));
    let file = std::fs::File::open(path).map_err(io)?;
    let meta = file.metadata().map_err(io)?;
    if !meta.is_file() {
        return Err(LibraryError::Invalid(format!("{} is not a file", path.display())));
    }
    if meta.len() > max {
        return Err(LibraryError::Invalid(format!("{} is larger than the {} MiB this accepts", path.display(), max / (1024 * 1024))));
    }
    let mut bytes = Vec::new();
    file.take(max.saturating_add(1)).read_to_end(&mut bytes).map_err(io)?;
    if bytes.len() as u64 > max {
        return Err(LibraryError::Invalid(format!("{} is larger than the {} MiB this accepts", path.display(), max / (1024 * 1024))));
    }
    Ok(bytes)
}

/// Writes `bytes` to a temporary file beside `path`, flushes it, then renames it over `path`: a
/// reader (or the sync service) sees the old file or the new one, never half of one. A failure
/// removes the temporary file and leaves `path` as it was.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let io = |e: std::io::Error| LibraryError::Io(format!("{}: {e}", path.display()));
    if path.is_dir() {
        return Err(LibraryError::Invalid(format!("{} is a folder, not a file", path.display())));
    }
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().ok_or_else(|| LibraryError::Invalid(format!("{} is not a file path", path.display())))?;
    std::fs::create_dir_all(dir).map_err(io)?;
    let tag = random_id::<6>()?;
    // A leading dot and a `.tmp` end keep the sync service and the folder scans from taking it
    // for a document.
    let tmp = dir.join(format!(".{}.{tag}.tmp", name.to_string_lossy()));
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(e));
    }
    Ok(())
}

/// Whether the sync service has `path` only as a placeholder (iCloud keeps `.<name>.icloud`
/// where the file will be until it is downloaded).
pub fn is_placeholder_only(path: &Path) -> bool {
    if path.exists() {
        return false;
    }
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else { return false };
    dir.join(format!(".{}.icloud", name.to_string_lossy())).exists()
}

/// `name` as one plain file name: separators, colons and control characters become `_`, leading
/// dots are dropped (no hidden or `..` names), and it is cut to 120 characters. Never empty.
pub fn safe_file_name(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '\0') || c.is_control() { '_' } else { c }).collect();
    let trimmed = cleaned.trim().trim_start_matches('.').trim();
    let cut: String = trimmed.chars().take(120).collect();
    if cut.is_empty() { "document".to_string() } else { cut }
}

/// The first of `stem.ext`, `stem (2).ext`, ... that does not exist in `dir`.
pub fn unused_name(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let candidate = |n: u32| if n == 1 { format!("{stem}.{ext}") } else { format!("{stem} ({n}).{ext}") };
    for n in 1..10_000 {
        let path = dir.join(candidate(n));
        if !path.exists() {
            return path;
        }
    }
    dir.join(format!("{stem}-{}.{ext}", now_ms()))
}

/// `text` cut to at most `max` bytes at a character boundary.
pub fn cut_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_cannot_climb_or_hide() {
        assert_eq!(safe_file_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_file_name(".hidden.pdf"), "hidden.pdf");
        assert_eq!(safe_file_name(".."), "document");
        assert_eq!(safe_file_name("a\\b:c\n.pdf"), "a_b_c_.pdf");
    }

    #[test]
    fn text_is_cut_at_a_character_boundary() {
        assert_eq!(cut_bytes("héllo", 2), "h");
        assert_eq!(cut_bytes("héllo", 3), "hé");
        assert_eq!(cut_bytes("abc", 10), "abc");
    }

    #[test]
    fn an_atomic_write_replaces_the_file_and_leaves_no_staging_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn a_folder_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        assert!(write_atomic(dir.path(), b"x").is_err());
    }

    #[test]
    fn a_big_file_is_refused_without_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        std::fs::write(&path, vec![0u8; 100]).unwrap();
        assert!(read_bounded(&path, 50).is_err());
        assert_eq!(read_bounded(&path, 100).unwrap().len(), 100);
    }
}
