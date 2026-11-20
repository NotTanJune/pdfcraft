//! The library folder: `library.json` (the index) and `library-text.json` (the text of the
//! documents) at its top, the documents it copied in `Documents/`, and the vault in `Vault/`.
//!
//! The folder is meant to be synced between Macs (iCloud Drive's `PdfCraft/` folder by default),
//! so every change is read-merge-write: the files on disk are read, joined with each other and
//! with any conflict copies the sync service made (`library 2.json`), the change is applied, and
//! the result is written back atomically. A conflict copy that was merged is then removed. The
//! window between the read and the write is not locked: two changes made at the same instant on
//! one Mac can lose one of them, which a second try repairs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{LibraryError, Result};
use crate::extract::{self, Facts};
use crate::index::{Entry, Index, Location, clean_tags, merge_entry, valid_id};
use crate::search::{self, Field, Item};
use crate::text::{TextEntry, TextIndex};
use crate::util::{self, MAX_FILE_BYTES, now_ms, read_bounded, safe_file_name, sha256_hex, unused_name, write_atomic};

/// The index file.
pub const INDEX_FILE: &str = "library.json";
/// The text index file.
pub const TEXT_FILE: &str = "library-text.json";
/// Where copied documents go, below the library folder.
pub const DOCUMENTS_DIR: &str = "Documents";

/// The largest index or text file read.
const MAX_INDEX_BYTES: u64 = 256 * 1024 * 1024;

/// How a document joins the library.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    /// Copy the file into the library folder (the default), so it syncs with the library.
    #[default]
    Copy,
    /// Keep the file where it is and record its path.
    Reference,
}

/// What to do when adding a document.
#[derive(Clone, Debug, Default)]
pub struct AddOptions {
    pub mode: Mode,
    /// The title; the file's name without `.pdf` when empty or absent.
    pub title: Option<String>,
    pub tags: Vec<String>,
    /// For a document that needs one to be read.
    pub password: Option<String>,
}

/// What adding did.
#[derive(Clone, Debug)]
pub struct Added {
    pub entry: Entry,
    /// A new entry (not an existing one updated).
    pub created: bool,
    /// The file was copied into the library folder.
    pub copied: bool,
    /// Why the document's text is missing or partial, if it is.
    pub note: Option<String>,
}

/// The library folder.
#[derive(Clone, Debug)]
pub struct Library {
    root: PathBuf,
}

/// The index and text, merged, as of one moment.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    root: PathBuf,
    pub index: Index,
    pub texts: TextIndex,
    /// Files that could not be read whole, and entries skipped.
    pub warnings: Vec<String>,
}

/// Where an added document's file will be.
enum Place {
    /// Already settled: inside the library folder, or a reference.
    At(Location),
    /// To be copied into `Documents/`.
    CopyIn,
}

/// The conflict copies that were merged, to remove once the result is written.
#[derive(Default)]
struct Merged {
    snapshot: Snapshot,
    index_conflicts: Vec<PathBuf>,
    text_conflicts: Vec<PathBuf>,
    /// A damaged main file to set aside before writing.
    damaged: Vec<PathBuf>,
    index_bytes: Option<Vec<u8>>,
    text_bytes: Option<Vec<u8>>,
}

/// Which file of the library a name is, and whether it is a sync conflict copy of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Index,
    Text,
}

fn classify(name: &str) -> Option<(Kind, bool)> {
    if name.starts_with('.') {
        return None;
    }
    let stem = name.strip_suffix(".json")?;
    match stem {
        "library" => return Some((Kind::Index, false)),
        "library-text" => return Some((Kind::Text, false)),
        _ => {}
    }
    // iCloud: `library 2.json`; Dropbox: `library (conflicted copy ...).json`; Syncthing:
    // `library.sync-conflict-....json`.
    for (base, kind) in [("library-text", Kind::Text), ("library", Kind::Index)] {
        if let Some(rest) = stem.strip_prefix(base)
            && rest.starts_with([' ', '(', '.'])
            && !rest.starts_with("-text")
        {
            return Some((kind, true));
        }
    }
    None
}

impl Library {
    /// The library in `root`. Nothing is created until something is added.
    pub fn open(root: impl Into<PathBuf>) -> Library {
        Library { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reads both index files and every conflict copy and joins them.
    fn read_merged(&self) -> Result<Merged> {
        let mut merged = Merged { snapshot: Snapshot { root: self.root.clone(), ..Snapshot::default() }, ..Merged::default() };
        for name in [INDEX_FILE, TEXT_FILE] {
            if util::is_placeholder_only(&self.root.join(name)) {
                return Err(LibraryError::NotDownloaded(name.to_string()));
            }
        }
        let Ok(dir) = std::fs::read_dir(&self.root) else { return Ok(merged) };
        let mut files: Vec<(Kind, bool, PathBuf)> = dir
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                classify(&name).map(|(kind, conflict)| (kind, conflict, e.path()))
            })
            .collect();
        // The main file first, then the copies by name, so the outcome does not depend on the
        // directory's order.
        files.sort_by(|a, b| (a.1, &a.2).cmp(&(b.1, &b.2)));
        for (kind, conflict, path) in files {
            let bytes = match read_bounded(&path, MAX_INDEX_BYTES) {
                Ok(b) => b,
                Err(e) => {
                    merged.snapshot.warnings.push(e.to_string());
                    continue;
                }
            };
            let shown = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            match kind {
                Kind::Index => match Index::from_json(&bytes) {
                    Ok((index, warnings)) => {
                        merged.snapshot.index.merge(&index);
                        merged.snapshot.warnings.extend(warnings);
                        if conflict {
                            merged.index_conflicts.push(path);
                        } else {
                            merged.index_bytes = Some(bytes);
                        }
                    }
                    Err(why) => {
                        merged.snapshot.warnings.push(format!("{shown}: {why}"));
                        if !conflict {
                            merged.damaged.push(path);
                        }
                    }
                },
                Kind::Text => match TextIndex::from_json(&bytes) {
                    Ok((texts, warnings)) => {
                        merged.snapshot.texts.merge(&texts);
                        merged.snapshot.warnings.extend(warnings);
                        if conflict {
                            merged.text_conflicts.push(path);
                        } else {
                            merged.text_bytes = Some(bytes);
                        }
                    }
                    Err(why) => {
                        merged.snapshot.warnings.push(format!("{shown}: {why}"));
                        if !conflict {
                            merged.damaged.push(path);
                        }
                    }
                },
            }
        }
        Ok(merged)
    }

    /// The library as it is on disk now, merged with any conflict copies. Writes nothing.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let mut snapshot = self.read_merged()?.snapshot;
        let live: std::collections::HashSet<String> = snapshot.index.live().map(|e| e.id.clone()).collect();
        snapshot.texts.retain_ids(|id| live.contains(id));
        Ok(snapshot)
    }

    /// Reads, lets `change` edit the merged state, and writes the result back atomically.
    fn modify<R>(&self, change: impl FnOnce(&Library, &mut Index, &mut TextIndex) -> Result<R>) -> Result<R> {
        let merged = self.read_merged()?;
        let Merged { snapshot, index_conflicts, text_conflicts, damaged, index_bytes, text_bytes } = merged;
        let (mut index, mut texts) = (snapshot.index, snapshot.texts);
        let result = change(self, &mut index, &mut texts)?;
        index.prune(now_ms());
        let live: std::collections::HashSet<String> = index.live().map(|e| e.id.clone()).collect();
        texts.retain_ids(|id| live.contains(id));
        // A damaged main file is set aside, not overwritten, so nothing is lost for good.
        for path in damaged {
            let aside = path.with_extension(format!("json.damaged-{}", now_ms()));
            std::fs::rename(&path, &aside).map_err(|e| LibraryError::Io(format!("{}: {e}", path.display())))?;
        }
        let (index_out, text_out) = (index.to_json(), texts.to_json());
        if index_bytes.as_deref() != Some(index_out.as_slice()) || !index_conflicts.is_empty() {
            write_atomic(&self.root.join(INDEX_FILE), &index_out)?;
        }
        if text_bytes.as_deref() != Some(text_out.as_slice()) || !text_conflicts.is_empty() {
            write_atomic(&self.root.join(TEXT_FILE), &text_out)?;
        }
        // Merged into the files just written; leaving them would merge them again every time.
        for path in index_conflicts.into_iter().chain(text_conflicts) {
            let _ = std::fs::remove_file(path);
        }
        Ok(result)
    }

    /// Whether `path` is inside the library folder, and where (a `/`-separated relative path).
    fn relative_in_library(&self, path: &Path) -> Option<String> {
        let root = self.root.canonicalize().ok()?;
        let path = path.canonicalize().ok()?;
        let rel = path.strip_prefix(&root).ok()?;
        let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        // The vault's files are not library documents.
        (!parts.is_empty() && parts.first().map(String::as_str) != Some(crate::vault::VAULT_DIR)).then(|| parts.join("/"))
    }

    /// Adds the PDF at `path` to the library, or updates its entry when it is already there
    /// (same file, or the same content): the title, tags, page count and text are refreshed.
    pub fn add_file(&self, path: &Path, options: &AddOptions) -> Result<Added> {
        let bytes = read_bounded(path, MAX_FILE_BYTES)?;
        let facts = extract::read_facts(Arc::new(bytes.clone()), options.password.as_deref())?;
        let hash = sha256_hex(&bytes);
        let size = bytes.len() as u64;
        let id = format!("d{}", hash.get(..16).unwrap_or(&hash));
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let title = options.title.as_deref().map(str::trim).filter(|t| !t.is_empty()).map_or_else(|| stem.clone(), str::to_string);
        let tags = clean_tags(options.tags.iter().map(String::as_str));
        let note = if facts.unreadable {
            Some("the document could not be opened (password-protected or damaged), so its pages and text are not indexed".to_string())
        } else if facts.truncated {
            Some("only the first part of the text is indexed".to_string())
        } else if facts.text.trim().is_empty() {
            Some("the document has no text layer: run Scan & OCR to make it searchable".to_string())
        } else {
            None
        };
        let inside = self.relative_in_library(path);
        let canonical = path.canonicalize().map_err(|e| LibraryError::Io(format!("{}: {e}", path.display())))?;
        self.modify(|lib, index, texts| {
            let place = match (&inside, options.mode) {
                (Some(rel), _) => Place::At(Location::Library(rel.clone())),
                (None, Mode::Reference) => Place::At(Location::External(canonical.to_string_lossy().into_owned())),
                (None, Mode::Copy) => Place::CopyIn,
            };
            let existing = index.live().find(|e| e.id == id || matches!(&place, Place::At(loc) if &e.location == loc)).cloned();
            let now = now_ms();
            let created = existing.is_none();
            let mut entry = existing.unwrap_or_else(|| Entry {
                id: id.clone(),
                title: String::new(),
                pages: 0,
                tags: Vec::new(),
                added_ms: now,
                opened_ms: 0,
                modified_ms: now,
                size,
                hash: hash.clone(),
                // Replaced just below: a new entry always gets a place.
                location: Location::External(String::new()),
                deleted: false,
            });
            // Where the entry's file is: kept when it is there, else the place chosen now.
            let keep = !created && !lib.is_missing(&entry);
            let mut copied = false;
            if !keep {
                match &place {
                    Place::At(location) => entry.location = location.clone(),
                    Place::CopyIn => {
                        let dir = lib.root.join(DOCUMENTS_DIR);
                        std::fs::create_dir_all(&dir).map_err(|e| LibraryError::Io(format!("{}: {e}", dir.display())))?;
                        let target = unused_name(&dir, &safe_file_name(&stem), "pdf");
                        write_atomic(&target, &bytes)?;
                        let file = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        entry.location = Location::Library(format!("{DOCUMENTS_DIR}/{file}"));
                        copied = true;
                    }
                }
            }
            entry.title = title.clone();
            entry.pages = u32::try_from(facts.pages).unwrap_or(u32::MAX);
            entry.size = size;
            entry.hash = hash.clone();
            entry.tags = clean_tags(entry.tags.iter().map(String::as_str).chain(tags.iter().map(String::as_str)));
            entry.modified_ms = now.max(entry.modified_ms.saturating_add(1));
            texts.texts.insert(entry.id.clone(), TextEntry { modified_ms: entry.modified_ms, hash: hash.clone(), text: facts.text.clone() });
            index.entries.insert(entry.id.clone(), entry.clone());
            Ok(Added { entry, created, copied, note: note.clone() })
        })
    }

    /// Removes a document from the library (the entry stays as a tombstone so that other Macs drop
    /// it too). A copy made in the library folder is deleted with it; a referenced file never is.
    /// Returns the removed entry and whether a copy was deleted.
    pub fn remove(&self, id: &str) -> Result<(Entry, bool)> {
        self.modify(|lib, index, _| {
            let entry =
                index.live().find(|e| e.id == id).cloned().ok_or_else(|| LibraryError::NotFound(format!("no document {id} in the library")))?;
            let mut deleted_copy = false;
            if let Location::Library(_) = &entry.location {
                let path = lib.resolve(&entry);
                // Only a file below Documents/ is the library's own copy.
                let own = path.starts_with(lib.root.join(DOCUMENTS_DIR));
                if own && path.is_file() {
                    deleted_copy = std::fs::remove_file(&path).is_ok();
                }
            }
            index.entries.insert(entry.id.clone(), entry.tombstone(now_ms()));
            Ok((entry, deleted_copy))
        })
    }

    /// Adds `add` to and removes `remove` from the tags of a document.
    pub fn retag(&self, id: &str, add: &[String], remove: &[String]) -> Result<Entry> {
        self.modify(|_, index, _| {
            let entry =
                index.entries.get_mut(id).filter(|e| !e.deleted).ok_or_else(|| LibraryError::NotFound(format!("no document {id} in the library")))?;
            let drop: Vec<String> = remove.iter().map(|t| t.trim().to_lowercase()).collect();
            let kept: Vec<String> = entry.tags.iter().filter(|t| !drop.contains(&t.to_lowercase())).cloned().collect();
            entry.tags = clean_tags(kept.iter().map(String::as_str).chain(add.iter().map(String::as_str)));
            entry.modified_ms = now_ms().max(entry.modified_ms.saturating_add(1));
            Ok(entry.clone())
        })
    }

    /// Renames a document in the library (the file keeps its name).
    pub fn retitle(&self, id: &str, title: &str) -> Result<Entry> {
        let title = title.trim();
        if title.is_empty() {
            return Err(LibraryError::Invalid("the title is empty".to_string()));
        }
        self.modify(|_, index, _| {
            let entry =
                index.entries.get_mut(id).filter(|e| !e.deleted).ok_or_else(|| LibraryError::NotFound(format!("no document {id} in the library")))?;
            entry.title = crate::util::cut_bytes(title, 512).to_string();
            entry.modified_ms = now_ms().max(entry.modified_ms.saturating_add(1));
            Ok(entry.clone())
        })
    }

    /// Notes that a document was opened now (only `opened_ms` changes, so it never overrides an
    /// edit made on another Mac).
    pub fn mark_opened(&self, id: &str) -> Result<Entry> {
        self.modify(|_, index, _| {
            let entry =
                index.entries.get_mut(id).filter(|e| !e.deleted).ok_or_else(|| LibraryError::NotFound(format!("no document {id} in the library")))?;
            entry.opened_ms = now_ms();
            Ok(entry.clone())
        })
    }

    /// The file of `entry` on this Mac.
    pub fn resolve(&self, entry: &Entry) -> PathBuf {
        entry.location.resolve(&self.root)
    }

    /// Whether the file of `entry` is not there on this Mac.
    pub fn is_missing(&self, entry: &Entry) -> bool {
        !self.resolve(entry).is_file()
    }

    /// Reads a document's pages and text again from its file and updates the entry, for a file
    /// that changed since it was added.
    pub fn reindex(&self, id: &str, password: Option<&str>) -> Result<Entry> {
        let snapshot = self.snapshot()?;
        let entry =
            snapshot.index.live().find(|e| e.id == id).cloned().ok_or_else(|| LibraryError::NotFound(format!("no document {id} in the library")))?;
        let bytes = read_bounded(&self.resolve(&entry), MAX_FILE_BYTES)?;
        let hash = sha256_hex(&bytes);
        let facts: Facts = extract::read_facts(Arc::new(bytes.clone()), password)?;
        self.modify(|_, index, texts| {
            let e =
                index.entries.get_mut(id).filter(|e| !e.deleted).ok_or_else(|| LibraryError::NotFound(format!("no document {id} in the library")))?;
            e.pages = u32::try_from(facts.pages).unwrap_or(u32::MAX);
            e.size = bytes.len() as u64;
            e.hash = hash.clone();
            e.modified_ms = now_ms().max(e.modified_ms.saturating_add(1));
            texts.texts.insert(id.to_string(), TextEntry { modified_ms: e.modified_ms, hash, text: facts.text });
            Ok(e.clone())
        })
    }
}

impl Snapshot {
    /// The folder the snapshot was read from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The live entries, most recently opened (or added) first.
    pub fn entries(&self) -> Vec<Entry> {
        let mut list: Vec<Entry> = self.index.live().cloned().collect();
        list.sort_by(|a, b| (b.opened_ms.max(b.added_ms), &a.title, &a.id).cmp(&(a.opened_ms.max(a.added_ms), &b.title, &b.id)));
        list
    }

    /// The entry with `id`, if it is live.
    pub fn get(&self, id: &str) -> Option<&Entry> {
        if !valid_id(id) {
            return None;
        }
        self.index.entries.get(id).filter(|e| !e.deleted)
    }

    /// The file of `entry` on this Mac.
    pub fn path_of(&self, entry: &Entry) -> PathBuf {
        entry.location.resolve(&self.root)
    }

    /// Every tag in use with how many documents carry it, most used first.
    pub fn tag_counts(&self) -> Vec<(String, usize)> {
        let mut counts: HashMap<String, (String, usize)> = HashMap::new();
        for e in self.index.live() {
            for t in &e.tags {
                counts.entry(t.to_lowercase()).or_insert_with(|| (t.clone(), 0)).1 += 1;
            }
        }
        let mut list: Vec<(String, usize)> = counts.into_values().collect();
        list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
        list
    }

    /// Searches titles, tags and text. An empty `query` lists everything, most recent first;
    /// `tags` keeps the documents that carry all of them.
    pub fn search(&self, query: &str, tags: &[String], limit: usize) -> Vec<Hit> {
        let words = search::words(query);
        let wanted: Vec<String> = tags.iter().map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()).collect();
        let mut hits: Vec<Hit> = Vec::new();
        for (rank, entry) in self.entries().into_iter().enumerate() {
            if !wanted.iter().all(|w| entry.tags.iter().any(|t| &t.to_lowercase() == w)) {
                continue;
            }
            let text = self.texts.texts.get(&entry.id).map_or("", |t| t.text.as_str());
            let item = Item { title: &entry.title, tags: &entry.tags, text };
            let Some(m) = search::matches(&item, &words) else { continue };
            hits.push(Hit { entry, score: m.score, fields: m.fields, snippet: m.snippet, recency: rank });
        }
        // Best match first; equal matches (and a list without a query) by recency.
        hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.recency.cmp(&b.recency)));
        hits.truncate(limit);
        hits
    }
}

/// One search result.
#[derive(Clone, Debug)]
pub struct Hit {
    pub entry: Entry,
    pub score: u32,
    pub fields: Vec<Field>,
    pub snippet: Option<String>,
    recency: usize,
}

/// Merges two versions of one entry (re-exported for the vault's use of the same rule).
pub fn merge(a: &Entry, b: &Entry) -> Entry {
    merge_entry(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::fixtures::pdf;

    fn write_pdf(dir: &Path, name: &str, pages: &[&str]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, pdf(pages)).unwrap();
        path
    }

    #[test]
    fn conflict_copies_are_recognised_and_other_files_are_not() {
        assert_eq!(classify("library.json"), Some((Kind::Index, false)));
        assert_eq!(classify("library 2.json"), Some((Kind::Index, true)));
        assert_eq!(classify("library (conflicted copy 2026-11-01).json"), Some((Kind::Index, true)));
        assert_eq!(classify("library.sync-conflict-20261101-ABC.json"), Some((Kind::Index, true)));
        assert_eq!(classify("library-text.json"), Some((Kind::Text, false)));
        assert_eq!(classify("library-text 2.json"), Some((Kind::Text, true)));
        assert_eq!(classify("libraryx.json"), None);
        assert_eq!(classify(".library.json"), None);
        assert_eq!(classify("notes.json"), None);
    }

    #[test]
    fn adding_copies_the_file_into_the_library_and_stores_a_relative_path() {
        let home = tempfile::tempdir().unwrap();
        let lib = Library::open(home.path().join("PdfCraft"));
        let src = tempfile::tempdir().unwrap();
        let file = write_pdf(src.path(), "Lease.pdf", &["Force majeure clause"]);
        let added = lib.add_file(&file, &AddOptions { tags: vec!["Legal".into()], ..Default::default() }).unwrap();
        assert!(added.created && added.copied);
        assert_eq!(added.entry.title, "Lease");
        assert_eq!(added.entry.pages, 1);
        assert_eq!(added.entry.location, Location::Library("Documents/Lease.pdf".into()));
        assert!(lib.root().join("Documents/Lease.pdf").is_file());
        // The index on disk names the file relatively, so it is right on every Mac.
        let json = std::fs::read_to_string(lib.root().join(INDEX_FILE)).unwrap();
        assert!(json.contains("Documents/Lease.pdf") && !json.contains(home.path().to_str().unwrap()), "{json}");
        let snap = lib.snapshot().unwrap();
        assert!(snap.texts.texts[&added.entry.id].text.contains("Force majeure"));
    }

    #[test]
    fn a_referenced_file_stays_where_it_is_and_is_marked_missing_when_it_goes() {
        let home = tempfile::tempdir().unwrap();
        let lib = Library::open(home.path().join("PdfCraft"));
        let src = tempfile::tempdir().unwrap();
        let file = write_pdf(src.path(), "ref.pdf", &["Referenced text"]);
        let added = lib.add_file(&file, &AddOptions { mode: Mode::Reference, ..Default::default() }).unwrap();
        assert!(!added.copied);
        assert!(matches!(&added.entry.location, Location::External(p) if Path::new(p).is_absolute()));
        assert!(!lib.root().join("Documents").exists());
        assert!(!lib.is_missing(&added.entry));
        std::fs::remove_file(&file).unwrap();
        assert!(lib.is_missing(&added.entry), "marked missing, still listed");
        assert_eq!(lib.snapshot().unwrap().entries().len(), 1);
    }

    #[test]
    fn adding_the_same_content_again_updates_the_entry() {
        let home = tempfile::tempdir().unwrap();
        let lib = Library::open(home.path().join("L"));
        let src = tempfile::tempdir().unwrap();
        let file = write_pdf(src.path(), "a.pdf", &["same"]);
        let first = lib.add_file(&file, &AddOptions { tags: vec!["one".into()], ..Default::default() }).unwrap();
        let again = lib.add_file(&file, &AddOptions { tags: vec!["two".into()], ..Default::default() }).unwrap();
        assert!(!again.created && !again.copied);
        assert_eq!(first.entry.id, again.entry.id);
        assert_eq!(again.entry.tags, ["one", "two"]);
        assert_eq!(lib.snapshot().unwrap().entries().len(), 1);
        assert_eq!(std::fs::read_dir(lib.root().join("Documents")).unwrap().count(), 1);
    }

    #[test]
    fn a_file_already_in_the_library_folder_is_indexed_in_place() {
        let home = tempfile::tempdir().unwrap();
        let lib = Library::open(home.path().join("L"));
        std::fs::create_dir_all(lib.root().join("Inbox")).unwrap();
        let file = write_pdf(&lib.root().join("Inbox"), "x.pdf", &["inside"]);
        let added = lib.add_file(&file, &AddOptions::default()).unwrap();
        assert!(!added.copied);
        assert_eq!(added.entry.location, Location::Library("Inbox/x.pdf".into()));
    }

    #[test]
    fn something_that_is_not_a_pdf_is_not_added() {
        let home = tempfile::tempdir().unwrap();
        let lib = Library::open(home.path().join("L"));
        let src = tempfile::tempdir().unwrap();
        let file = src.path().join("n.pdf");
        std::fs::write(&file, b"hello").unwrap();
        assert!(lib.add_file(&file, &AddOptions::default()).is_err());
        assert!(!lib.root().join(INDEX_FILE).exists());
    }

    #[test]
    fn removing_deletes_only_the_librarys_own_copy() {
        let home = tempfile::tempdir().unwrap();
        let lib = Library::open(home.path().join("L"));
        let src = tempfile::tempdir().unwrap();
        let copy = lib.add_file(&write_pdf(src.path(), "c.pdf", &["copy"]), &AddOptions::default()).unwrap();
        let refd_file = write_pdf(src.path(), "r.pdf", &["ref"]);
        let refd = lib.add_file(&refd_file, &AddOptions { mode: Mode::Reference, ..Default::default() }).unwrap();
        let (_, deleted) = lib.remove(&copy.entry.id).unwrap();
        assert!(deleted && !lib.root().join("Documents/c.pdf").exists());
        let (_, deleted) = lib.remove(&refd.entry.id).unwrap();
        assert!(!deleted && refd_file.exists(), "a referenced file is never deleted");
        let snap = lib.snapshot().unwrap();
        assert!(snap.entries().is_empty());
        assert!(snap.index.entries[&copy.entry.id].deleted, "the removal is remembered for the other Macs");
        assert!(lib.remove(&copy.entry.id).is_err());
    }

    #[test]
    fn two_macs_edits_merge_per_entry_and_the_conflict_copy_is_folded_in() {
        let home = tempfile::tempdir().unwrap();
        let lib = Library::open(home.path().join("L"));
        let src = tempfile::tempdir().unwrap();
        let a = lib.add_file(&write_pdf(src.path(), "a.pdf", &["alpha"]), &AddOptions::default()).unwrap().entry;
        let b = lib.add_file(&write_pdf(src.path(), "b.pdf", &["bravo"]), &AddOptions::default()).unwrap().entry;
        // The other Mac's version of the index, written beside ours as iCloud does on a conflict:
        // it retitled `a` later than we did anything, removed `b`, and knows a document of its own.
        let mut theirs = lib.snapshot().unwrap().index;
        let mut a2 = a.clone();
        a2.title = "Alpha (from the other Mac)".into();
        a2.modified_ms = a.modified_ms + 10_000;
        theirs.entries.insert(a.id.clone(), a2);
        theirs.entries.insert(b.id.clone(), b.tombstone(b.modified_ms + 5_000));
        let mut c = a.clone();
        c.id = "dother".into();
        c.title = "Only there".into();
        c.hash.clear();
        c.location = Location::Library("Documents/other.pdf".into());
        theirs.entries.insert(c.id.clone(), c);
        std::fs::write(lib.root().join("library 2.json"), theirs.to_json()).unwrap();

        let snap = lib.snapshot().unwrap();
        let titles: Vec<_> = snap.entries().into_iter().map(|e| e.title).collect();
        assert!(titles.contains(&"Alpha (from the other Mac)".to_string()) && titles.contains(&"Only there".to_string()), "{titles:?}");
        assert!(!titles.iter().any(|t| t == "b"), "the other Mac's removal arrived: {titles:?}");
        assert!(lib.root().join("library 2.json").exists(), "reading alone changes nothing");

        // The next change writes the merged result and retires the copy.
        lib.retag(&a.id, &["x".to_string()], &[]).unwrap();
        assert!(!lib.root().join("library 2.json").exists());
        let (index, _) = Index::from_json(&std::fs::read(lib.root().join(INDEX_FILE)).unwrap()).unwrap();
        assert_eq!(index.entries[&a.id].title, "Alpha (from the other Mac)");
        assert_eq!(index.entries[&a.id].tags, ["x"]);
        assert!(index.entries[&b.id].deleted && index.entries.contains_key("dother"));
    }

    #[test]
    fn a_damaged_index_is_set_aside_not_overwritten() {
        let home = tempfile::tempdir().unwrap();
        let lib = Library::open(home.path().join("L"));
        std::fs::create_dir_all(lib.root()).unwrap();
        std::fs::write(lib.root().join(INDEX_FILE), b"{ this is not json").unwrap();
        let snap = lib.snapshot().unwrap();
        assert!(snap.entries().is_empty() && !snap.warnings.is_empty());
        let src = tempfile::tempdir().unwrap();
        lib.add_file(&write_pdf(src.path(), "a.pdf", &["x"]), &AddOptions::default()).unwrap();
        let asides: Vec<_> =
            std::fs::read_dir(lib.root()).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().contains("damaged")).collect();
        assert_eq!(asides.len(), 1);
        assert_eq!(std::fs::read(asides[0].path()).unwrap(), b"{ this is not json");
    }

    #[test]
    fn an_index_still_downloading_is_not_replaced_by_an_empty_one() {
        let home = tempfile::tempdir().unwrap();
        let lib = Library::open(home.path().join("L"));
        std::fs::create_dir_all(lib.root()).unwrap();
        std::fs::write(lib.root().join(".library.json.icloud"), b"placeholder").unwrap();
        assert!(matches!(lib.snapshot(), Err(LibraryError::NotDownloaded(_))));
        let src = tempfile::tempdir().unwrap();
        assert!(lib.add_file(&write_pdf(src.path(), "a.pdf", &["x"]), &AddOptions::default()).is_err());
        assert!(!lib.root().join(INDEX_FILE).exists());
    }

    #[test]
    fn search_finds_titles_tags_and_text_and_filters_by_tag() {
        let home = tempfile::tempdir().unwrap();
        let lib = Library::open(home.path().join("L"));
        let src = tempfile::tempdir().unwrap();
        lib.add_file(
            &write_pdf(src.path(), "Lease.pdf", &["Force majeure clause"]),
            &AddOptions { tags: vec!["legal".into()], ..Default::default() },
        )
        .unwrap();
        lib.add_file(
            &write_pdf(src.path(), "Recipes.pdf", &["Banana bread and majeure"]),
            &AddOptions { tags: vec!["food".into()], ..Default::default() },
        )
        .unwrap();
        let snap = lib.snapshot().unwrap();
        assert_eq!(snap.search("", &[], 10).len(), 2);
        let hits = snap.search("majeure", &[], 10);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.fields == [Field::Text] && h.snippet.as_deref().is_some_and(|s| s.contains("majeure"))));
        assert_eq!(snap.search("lease", &[], 10)[0].fields, [Field::Title]);
        let legal = snap.search("majeure", &["Legal".to_string()], 10);
        assert_eq!(legal.len(), 1);
        assert_eq!(legal[0].entry.title, "Lease");
        assert!(snap.search("zebra", &[], 10).is_empty());
        assert_eq!(snap.tag_counts().len(), 2);
    }

    #[test]
    fn opening_marks_the_entry_recent_without_touching_its_edit_time() {
        let home = tempfile::tempdir().unwrap();
        let lib = Library::open(home.path().join("L"));
        let src = tempfile::tempdir().unwrap();
        let old = lib.add_file(&write_pdf(src.path(), "old.pdf", &["old"]), &AddOptions::default()).unwrap().entry;
        std::thread::sleep(std::time::Duration::from_millis(5));
        let newer = lib.add_file(&write_pdf(src.path(), "new.pdf", &["new"]), &AddOptions::default()).unwrap().entry;
        assert_eq!(lib.snapshot().unwrap().entries()[0].id, newer.id, "newest first");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let opened = lib.mark_opened(&old.id).unwrap();
        assert_eq!(opened.modified_ms, old.modified_ms);
        assert_eq!(lib.snapshot().unwrap().entries()[0].id, old.id, "recently opened first");
    }

    #[test]
    fn a_hostile_index_cannot_point_outside_the_folder() {
        let home = tempfile::tempdir().unwrap();
        let lib = Library::open(home.path().join("L"));
        std::fs::create_dir_all(lib.root()).unwrap();
        let json = serde_json::json!({ "format": 1, "entries": [ { "id": "x", "title": "x", "location": { "kind": "library", "path": "../../secret.pdf" } } ] });
        std::fs::write(lib.root().join(INDEX_FILE), json.to_string()).unwrap();
        let snap = lib.snapshot().unwrap();
        assert!(snap.entries().is_empty() && snap.warnings.len() == 1);
    }

    #[test]
    fn retag_and_retitle_change_the_entry() {
        let home = tempfile::tempdir().unwrap();
        let lib = Library::open(home.path().join("L"));
        let src = tempfile::tempdir().unwrap();
        let e = lib
            .add_file(&write_pdf(src.path(), "a.pdf", &["x"]), &AddOptions { tags: vec!["a".into(), "b".into()], ..Default::default() })
            .unwrap()
            .entry;
        let e2 = lib.retag(&e.id, &["c".to_string()], &["A".to_string()]).unwrap();
        assert_eq!(e2.tags, ["b", "c"]);
        assert_eq!(lib.retitle(&e.id, "  New title ").unwrap().title, "New title");
        assert!(lib.retitle(&e.id, "  ").is_err());
        assert!(lib.retag("nope", &[], &[]).is_err());
    }
}
