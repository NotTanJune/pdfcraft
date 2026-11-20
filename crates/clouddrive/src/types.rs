//! The data the drives pass around. Plain values; nothing here touches the network.

use serde::{Deserialize, Serialize};

/// Which service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Google,
    Microsoft,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 2] = [ProviderKind::Google, ProviderKind::Microsoft];

    /// The short name used in tool arguments, secret names and cache folders: `google` or `onedrive`.
    pub fn id(self) -> &'static str {
        match self {
            ProviderKind::Google => "google",
            ProviderKind::Microsoft => "onedrive",
        }
    }

    /// The name shown to people.
    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::Google => "Google Drive",
            ProviderKind::Microsoft => "OneDrive",
        }
    }

    /// `google` or `microsoft` (also `onedrive`, `drive`), in any letter case.
    pub fn parse(text: &str) -> Option<ProviderKind> {
        match text.trim().to_ascii_lowercase().as_str() {
            "google" | "gdrive" | "google_drive" | "google-drive" => Some(ProviderKind::Google),
            "microsoft" | "onedrive" | "one_drive" | "graph" => Some(ProviderKind::Microsoft),
            _ => None,
        }
    }
}

/// A file or a folder on a drive.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// The provider's own handle. Opaque.
    pub id: String,
    pub name: String,
    pub is_folder: bool,
    pub size: Option<u64>,
    /// RFC 3339, as the provider says it.
    pub modified: Option<String>,
    pub mime: Option<String>,
    /// What changes whenever the content does: Google's `version`, Graph's `eTag`. Compared, never
    /// parsed.
    pub version: Option<String>,
    pub parent: Option<String>,
    pub can_write: bool,
    /// A Google Docs, Sheets or Slides file: it has no bytes of its own and downloads as a PDF
    /// export.
    pub exported_as_pdf: bool,
    pub web_url: Option<String>,
}

/// Who is connected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub provider: ProviderKind,
    pub email: Option<String>,
    pub name: Option<String>,
}

impl Account {
    /// The email, else the name, else the service's name.
    pub fn display(&self) -> String {
        self.email.clone().or_else(|| self.name.clone()).unwrap_or_else(|| self.provider.label().to_string())
    }
}

/// What to list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    /// The folder's id; `None` is the drive's root. Ignored when `search` is set.
    pub folder: Option<String>,
    /// Part of a file name, searched across the whole drive.
    pub search: Option<String>,
    /// `ListPage::next` of the page before.
    pub page: Option<String>,
    /// How many to ask for; 0 is the default ([`crate::DEFAULT_PAGE_SIZE`]). Capped at [`crate::MAX_PAGE_SIZE`].
    pub limit: usize,
}

/// One page of a listing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListPage {
    pub items: Vec<Item>,
    /// Pass as [`Query::page`] for the next page; `None` at the end.
    pub next: Option<String>,
}

/// How far a transfer is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub done: u64,
    pub total: Option<u64>,
}

/// A downloaded file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Download {
    pub bytes: Vec<u8>,
    /// The file as the drive describes it just before its bytes were read.
    pub item: Item,
    /// A name for the local copy: the drive's name, with `.pdf` added for an exported Google file.
    pub file_name: String,
}

/// A request to stop. Cloning shares the flag.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn check(&self) -> Result<(), crate::CloudError> {
        if self.is_cancelled() { Err(crate::CloudError::Cancelled) } else { Ok(()) }
    }
}

/// File types the browser lists besides folders: what PdfCraft opens, directly or through the
/// in-process conversions.
pub const OPENABLE_EXTENSIONS: &[&str] =
    &["pdf", "docx", "docm", "dotx", "dotm", "md", "markdown", "tex", "csv", "tsv", "xlsx", "xlsm", "xltx", "xltm"];

/// Whether a file called `name` is one the browser lists.
pub fn is_openable(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| OPENABLE_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// A file name that is safe on every system: no path separators, no control characters, no
/// leading dots, at most 120 bytes (cut at a character boundary), never empty.
pub fn safe_file_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\u{202e}' | '\u{202d}') { '_' } else { c })
        .collect();
    out = out.trim().trim_start_matches('.').trim().to_string();
    while out.len() > 120 {
        out.pop();
    }
    if out.is_empty() { "file".to_string() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_openable_types_are_the_ones_pdfcraft_reads() {
        for name in ["a.pdf", "A.PDF", "x.docx", "n.md", "paper.tex", "t.csv", "b.xlsx", "r.markdown"] {
            assert!(is_openable(name), "{name}");
        }
        for name in ["a.exe", "pdf", "notes", "x.pdf.exe", ".pdf.zip"] {
            assert!(!is_openable(name), "{name}");
        }
    }

    #[test]
    fn a_file_name_cannot_escape_its_folder() {
        assert_eq!(safe_file_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_file_name("..hidden"), "hidden");
        assert_eq!(safe_file_name("a\0b\nc.pdf"), "a_b_c.pdf");
        assert_eq!(safe_file_name("   "), "file");
        assert_eq!(safe_file_name("...."), "file");
        assert!(safe_file_name(&"é".repeat(200)).len() <= 120);
        assert_eq!(safe_file_name("fine name (1).pdf"), "fine name (1).pdf");
    }

    #[test]
    fn providers_parse_by_their_common_names() {
        assert_eq!(ProviderKind::parse(" Google "), Some(ProviderKind::Google));
        assert_eq!(ProviderKind::parse("OneDrive"), Some(ProviderKind::Microsoft));
        assert_eq!(ProviderKind::parse("dropbox"), None);
    }
}
