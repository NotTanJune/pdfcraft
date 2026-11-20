//! What a drive does for PdfCraft, whoever provides it.
//!
//! [`Drive`] is the small contract PdfCraft's own UI and tools use: paging, search, a download
//! bounded in size with progress and cancellation, and uploads that can refuse to overwrite a file
//! that changed. `craft_cloud::Files` (the shared contract in storytold/craft-libs) is path-based and
//! has no paging, search, progress, cancellation or size bound, so it cannot carry these calls;
//! [`crate::files::DriveFiles`] adapts a `Drive` to `craft_cloud::Files` and `Accounts` for code
//! that wants the shared contract.

use crate::error::CloudError;
use crate::types::{Account, CancelToken, Download, Item, ListPage, Progress, ProviderKind, Query};

/// Items in a page when [`Query::limit`] is 0.
pub const DEFAULT_PAGE_SIZE: usize = 50;
/// The most items one page may ask for.
pub const MAX_PAGE_SIZE: usize = 200;
/// The most pages [`list_all`] reads.
pub const MAX_LIST_PAGES: usize = 20;
/// The largest download accepted unless the caller sets another limit: 256 MiB.
pub const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;
/// The largest upload accepted: 256 MiB (the whole file is held in memory).
pub const MAX_UPLOAD_BYTES: u64 = 256 * 1024 * 1024;
/// Files up to this size go up in one request; larger ones in chunks that can be cancelled and
/// show progress. Both providers' simple upload limits are 4 MB or more.
pub const SIMPLE_UPLOAD_BYTES: usize = 4_000_000;

pub trait Drive: Send + Sync {
    fn kind(&self) -> ProviderKind;

    /// Who is connected, as the provider says.
    fn account(&self) -> Result<Account, CloudError>;

    /// One page of folders and openable files, or of the search's matches.
    fn list(&self, query: &Query) -> Result<ListPage, CloudError>;

    /// One file or folder's details.
    fn stat(&self, id: &str) -> Result<Item, CloudError>;

    /// A file's bytes (a Google Docs or Sheets file as its PDF export). `max_bytes` bounds the
    /// size: a larger file is [`CloudError::TooLarge`] before or while it is read.
    fn download(&self, id: &str, max_bytes: u64, cancel: &CancelToken, progress: &mut dyn FnMut(Progress)) -> Result<Download, CloudError>;

    /// Creates a file in `folder` (`None` is the root) and returns it. A name that exists is not
    /// overwritten: Google makes a second file of that name, Microsoft renames the new one.
    fn upload_new(
        &self,
        folder: Option<&str>,
        name: &str,
        bytes: &[u8],
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Item, CloudError>;

    /// Replaces a file's content, whatever it is now.
    fn replace(&self, id: &str, bytes: &[u8], cancel: &CancelToken, progress: &mut dyn FnMut(Progress)) -> Result<Item, CloudError>;

    /// Replaces a file's content, unless it changed since `expected_version` (an [`Item::version`]
    /// from when it was opened): then [`CloudError::Conflict`] and nothing is written. `None`
    /// writes regardless. Neither provider's content upload takes a precondition, so another
    /// writer can still slip in between the check and the write; the window is one round trip.
    fn update(
        &self,
        id: &str,
        bytes: &[u8],
        expected_version: Option<&str>,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Item, CloudError> {
        if let Some(expected) = expected_version {
            let current = self.stat(id)?;
            if current.version.as_deref() != Some(expected) {
                return Err(CloudError::Conflict { name: current.name, current_version: current.version, modified: current.modified });
            }
        }
        self.replace(id, bytes, cancel, progress)
    }
}

/// Every item of a listing, up to [`MAX_LIST_PAGES`] pages. The flag says there were more.
pub fn list_all(drive: &dyn Drive, query: &Query) -> Result<(Vec<Item>, bool), CloudError> {
    let mut query = query.clone();
    let mut items = Vec::new();
    for _ in 0..MAX_LIST_PAGES {
        let page = drive.list(&query)?;
        items.extend(page.items);
        match page.next {
            Some(next) => query.page = Some(next),
            None => return Ok((items, false)),
        }
    }
    Ok((items, true))
}

/// Checks a name for a new file: not empty, no path separators, a reasonable length.
pub fn check_new_name(name: &str) -> Result<&str, CloudError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 255 || name.chars().any(|c| c.is_control() || matches!(c, '/' | '\\')) {
        return Err(CloudError::Invalid("a file name cannot be empty, contain / or \\, or be longer than 255 bytes".into()));
    }
    Ok(name)
}

/// Checks `bytes` is a size PdfCraft uploads.
pub fn check_upload_size(bytes: &[u8]) -> Result<(), CloudError> {
    if bytes.len() as u64 > MAX_UPLOAD_BYTES {
        return Err(CloudError::TooLarge { what: "the file".into(), limit: MAX_UPLOAD_BYTES });
    }
    Ok(())
}

/// The media type to upload a file of this name as.
pub fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "csv" => "text/csv",
        "md" | "markdown" => "text/markdown",
        "tex" => "application/x-tex",
        _ => "application/octet-stream",
    }
}

/// `name` with the extension its media type implies when it has none the browser lists, so the
/// local copy opens as the right format.
pub fn local_name(name: &str, mime: Option<&str>) -> String {
    if crate::types::is_openable(name) {
        return name.to_string();
    }
    let ext = match mime.unwrap_or_default() {
        "application/pdf" => "pdf",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "text/csv" => "csv",
        "text/markdown" => "md",
        _ => return name.to_string(),
    };
    format!("{name}.{ext}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_types_follow_the_extension() {
        assert_eq!(mime_for("Report.PDF"), "application/pdf");
        assert_eq!(mime_for("a.tex"), "application/x-tex");
        assert_eq!(mime_for("noext"), "application/octet-stream");
    }

    #[test]
    fn a_local_copy_gets_the_extension_its_type_implies() {
        assert_eq!(local_name("Report", Some("application/pdf")), "Report.pdf");
        assert_eq!(local_name("Report.pdf", Some("application/pdf")), "Report.pdf");
        assert_eq!(local_name("Notes", Some("text/plain")), "Notes");
    }

    #[test]
    fn new_names_are_checked() {
        assert_eq!(check_new_name("  a.pdf ").unwrap(), "a.pdf");
        for bad in ["", "  ", "a/b.pdf", "a\\b", "x\0y"] {
            assert!(check_new_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_new_name(&"x".repeat(256)).is_err());
    }
}
