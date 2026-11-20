//! The library's index: one entry per document, merged per entry across the Macs that share the
//! folder.
//!
//! Merging is a join: for each id the entry with the newest `modified_ms` wins whole (ties go to
//! the deleted one, then to the greater hash, so every Mac picks the same winner), `opened_ms`
//! takes the later of the two and `added_ms` the earlier. Opening a document changes only
//! `opened_ms`, so reading on one Mac never overrides an edit made on another. A removed document
//! stays as a tombstone (`deleted`) so the removal reaches the other Macs; tombstones older than
//! [`TOMBSTONE_DAYS`] are dropped.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::util::cut_bytes;

/// The file format version written to `library.json`.
pub const FORMAT: u32 = 1;

/// The most entries read from an index file; a larger file is damaged or hostile.
pub const MAX_ENTRIES: usize = 100_000;

/// How long a removal is remembered.
pub const TOMBSTONE_DAYS: i64 = 90;

const MAX_TITLE_BYTES: usize = 512;
const MAX_TAGS: usize = 32;
const MAX_TAG_BYTES: usize = 64;
const MAX_PATH_BYTES: usize = 4096;

/// Where a document is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "path", rename_all = "snake_case")]
pub enum Location {
    /// A path below the library folder, relative so that it is right on every Mac.
    Library(String),
    /// A file elsewhere, by absolute path: marked missing on a Mac where it is not.
    External(String),
}

impl Location {
    /// The path as stored.
    pub fn path(&self) -> &str {
        match self {
            Location::Library(p) | Location::External(p) => p,
        }
    }

    /// The file on this Mac, for a library folder at `root`.
    pub fn resolve(&self, root: &Path) -> std::path::PathBuf {
        match self {
            Location::Library(rel) => rel.split('/').fold(root.to_path_buf(), |p, part| p.join(part)),
            Location::External(abs) => std::path::PathBuf::from(abs),
        }
    }
}

/// One document of the library.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub pages: u32,
    #[serde(default)]
    pub tags: Vec<String>,
    /// When it was added, ms since 1970 (UTC).
    #[serde(default)]
    pub added_ms: i64,
    /// When it was last opened from the library.
    #[serde(default)]
    pub opened_ms: i64,
    /// When the entry was last changed (not opened): the clock the merge goes by.
    #[serde(default)]
    pub modified_ms: i64,
    /// Size of the file in bytes when it was indexed.
    #[serde(default)]
    pub size: u64,
    /// SHA-256 of the file when it was indexed, in hex.
    #[serde(default)]
    pub hash: String,
    pub location: Location,
    /// A removal, kept so that it reaches the other Macs.
    #[serde(default)]
    pub deleted: bool,
}

impl Entry {
    /// A removal of this entry at `now`: everything but the id is cleared.
    pub fn tombstone(&self, now: i64) -> Entry {
        Entry {
            id: self.id.clone(),
            title: String::new(),
            pages: 0,
            tags: Vec::new(),
            added_ms: self.added_ms,
            opened_ms: self.opened_ms,
            modified_ms: now.max(self.modified_ms.saturating_add(1)),
            size: 0,
            hash: String::new(),
            location: self.location.clone(),
            deleted: true,
        }
    }

    /// The value the merge compares: the greater wins.
    fn rank(&self) -> (i64, bool, &str, &str) {
        (self.modified_ms, self.deleted, &self.hash, &self.title)
    }
}

/// The merge of two versions of one entry. Commutative, associative and idempotent.
pub fn merge_entry(a: &Entry, b: &Entry) -> Entry {
    let (win, lose) = if a.rank() >= b.rank() { (a, b) } else { (b, a) };
    let mut out = win.clone();
    out.opened_ms = a.opened_ms.max(b.opened_ms);
    out.added_ms = match (a.added_ms, b.added_ms) {
        (0, x) | (x, 0) => x,
        (x, y) => x.min(y),
    };
    let _ = lose;
    out
}

/// All entries by id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Index {
    pub entries: BTreeMap<String, Entry>,
}

impl Index {
    /// Joins `other` into this index entry by entry.
    pub fn merge(&mut self, other: &Index) {
        for (id, theirs) in &other.entries {
            match self.entries.get(id) {
                Some(mine) => {
                    let merged = merge_entry(mine, theirs);
                    self.entries.insert(id.clone(), merged);
                }
                None => {
                    self.entries.insert(id.clone(), theirs.clone());
                }
            }
        }
    }

    /// Drops tombstones that are older than [`TOMBSTONE_DAYS`] at `now`.
    pub fn prune(&mut self, now: i64) {
        let horizon = now.saturating_sub(TOMBSTONE_DAYS * 86_400_000);
        self.entries.retain(|_, e| !(e.deleted && e.modified_ms < horizon));
    }

    /// The entries that are not removals.
    pub fn live(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values().filter(|e| !e.deleted)
    }

    /// The live entry that is the file at `location`.
    pub fn live_at(&self, location: &Location) -> Option<&Entry> {
        self.live().find(|e| &e.location == location)
    }

    /// The bytes of the index file.
    pub fn to_json(&self) -> Vec<u8> {
        let entries: Vec<&Entry> = self.entries.values().collect();
        let value = serde_json::json!({ "format": FORMAT, "entries": entries });
        let mut out = serde_json::to_vec_pretty(&value).unwrap_or_default();
        out.push(b'\n');
        out
    }

    /// Reads an index file. Entries that do not parse or fail the checks are skipped and said in
    /// the returned warnings; a file that is not an index at all is an error.
    pub fn from_json(bytes: &[u8]) -> Result<(Index, Vec<String>), String> {
        let value: Value = serde_json::from_slice(bytes).map_err(|e| format!("not a library index: {e}"))?;
        let format = value.get("format").and_then(Value::as_u64).unwrap_or(0);
        if format == 0 || format > u64::from(FORMAT) {
            return Err(format!("library index format {format} was written by a newer PdfCraft"));
        }
        let list = value.get("entries").and_then(Value::as_array).ok_or("not a library index: no entries")?;
        if list.len() > MAX_ENTRIES {
            return Err(format!("the library index lists {} documents; more than {MAX_ENTRIES} is not accepted", list.len()));
        }
        let mut index = Index::default();
        let mut warnings = Vec::new();
        for raw in list {
            match serde_json::from_value::<Entry>(raw.clone()).map_err(|e| e.to_string()).and_then(clean) {
                Ok(entry) => {
                    let merged = match index.entries.get(&entry.id) {
                        Some(existing) => merge_entry(existing, &entry),
                        None => entry,
                    };
                    index.entries.insert(merged.id.clone(), merged);
                }
                Err(why) => warnings.push(format!("skipped an entry of the library index: {why}")),
            }
        }
        Ok((index, warnings))
    }
}

/// Whether `id` is a plain identifier (letters, digits, `-` and `_`, 1 to 64 of them).
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Whether `rel` is a relative path of plain names with `/` between them: no `..`, no root, no
/// empty or dotted-away parts, no control characters.
pub fn valid_relative(rel: &str) -> bool {
    if rel.is_empty() || rel.len() > MAX_PATH_BYTES || rel.contains('\\') || rel.chars().any(char::is_control) {
        return false;
    }
    rel.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
        && Path::new(rel).components().all(|c| matches!(c, Component::Normal(_)))
}

/// A tag as kept: trimmed, one line, at most [`MAX_TAG_BYTES`], lowercase-insensitive duplicates
/// removed by the caller. `None` when nothing usable is left.
pub fn clean_tag(tag: &str) -> Option<String> {
    let one_line: String = tag.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let t = cut_bytes(one_line.trim(), MAX_TAG_BYTES).trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// The tags of `tags` cleaned, without duplicates (compared ignoring case) and at most [`MAX_TAGS`].
pub fn clean_tags<'a>(tags: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in tags {
        let Some(tag) = clean_tag(tag) else { continue };
        if out.len() < MAX_TAGS && !out.iter().any(|t| t.to_lowercase() == tag.to_lowercase()) {
            out.push(tag);
        }
    }
    out
}

/// Checks and tidies an entry read from a file that another Mac (or anything) may have written.
fn clean(mut e: Entry) -> Result<Entry, String> {
    if !valid_id(&e.id) {
        return Err(format!("{:?} is not a valid id", cut_bytes(&e.id, 40)));
    }
    match &e.location {
        Location::Library(rel) if !valid_relative(rel) => return Err(format!("{}: the path leaves the library folder", e.id)),
        Location::External(abs) if !Path::new(abs).is_absolute() || abs.len() > MAX_PATH_BYTES || abs.chars().any(char::is_control) => {
            return Err(format!("{}: the path is not absolute", e.id));
        }
        _ => {}
    }
    e.title = cut_bytes(&e.title.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>(), MAX_TITLE_BYTES).trim().to_string();
    e.tags = clean_tags(e.tags.iter().map(String::as_str));
    if e.hash.len() > 128 || !e.hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        e.hash.clear();
    }
    Ok(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn entry(id: &str, modified: i64) -> Entry {
        Entry {
            id: id.to_string(),
            title: format!("title {id}"),
            pages: 1,
            tags: vec![],
            added_ms: 1,
            opened_ms: 0,
            modified_ms: modified,
            size: 10,
            hash: "ab".to_string(),
            location: Location::Library(format!("Documents/{id}.pdf")),
            deleted: false,
        }
    }

    #[test]
    fn the_newest_edit_of_an_entry_wins_and_opening_never_overrides_it() {
        let mut a = entry("d1", 100);
        a.title = "from Mac A".into();
        let mut b = entry("d1", 200);
        b.title = "from Mac B".into();
        b.opened_ms = 0;
        a.opened_ms = 5_000;
        let merged = merge_entry(&a, &b);
        assert_eq!(merged.title, "from Mac B");
        assert_eq!(merged.opened_ms, 5_000, "the later open is kept");
        assert_eq!(merge_entry(&a, &b), merge_entry(&b, &a), "commutative");
    }

    #[test]
    fn a_removal_beats_an_older_edit_and_loses_to_a_newer_one() {
        let live = entry("d1", 100);
        let gone = live.tombstone(150);
        assert!(merge_entry(&live, &gone).deleted);
        let mut edited = entry("d1", 300);
        edited.title = "edited later".into();
        assert!(!merge_entry(&edited, &gone).deleted, "an edit made after the removal brings it back");
    }

    #[test]
    fn merging_is_idempotent_and_associative() {
        let (a, b, c) = (entry("d1", 1), entry("d1", 2), entry("d1", 3).tombstone(3));
        assert_eq!(merge_entry(&a, &a), a);
        assert_eq!(merge_entry(&merge_entry(&a, &b), &c), merge_entry(&a, &merge_entry(&b, &c)));
    }

    #[test]
    fn a_tie_picks_the_same_winner_on_every_mac() {
        let mut a = entry("d1", 100);
        a.hash = "aa".into();
        let mut b = entry("d1", 100);
        b.hash = "bb".into();
        assert_eq!(merge_entry(&a, &b).hash, merge_entry(&b, &a).hash);
    }

    #[test]
    fn indexes_merge_per_entry() {
        let mut one = Index::default();
        one.entries.insert("d1".into(), entry("d1", 10));
        one.entries.insert("d2".into(), entry("d2", 10));
        let mut two = Index::default();
        two.entries.insert("d2".into(), entry("d2", 20).tombstone(20));
        two.entries.insert("d3".into(), entry("d3", 5));
        one.merge(&two);
        assert_eq!(one.entries.len(), 3);
        assert!(one.entries["d2"].deleted && !one.entries["d1"].deleted && !one.entries["d3"].deleted);
    }

    #[test]
    fn old_tombstones_are_pruned_and_live_entries_never() {
        let mut index = Index::default();
        let old = entry("d1", 1).tombstone(1);
        index.entries.insert("d1".into(), old);
        index.entries.insert("d2".into(), entry("d2", 1));
        index.prune(TOMBSTONE_DAYS * 86_400_000 + 10);
        assert!(!index.entries.contains_key("d1") && index.entries.contains_key("d2"));
    }

    #[test]
    fn a_hostile_index_loses_only_the_bad_entries() {
        let json = serde_json::json!({
            "format": 1,
            "entries": [
                { "id": "ok", "location": { "kind": "library", "path": "Documents/a.pdf" } },
                { "id": "../evil", "location": { "kind": "library", "path": "Documents/a.pdf" } },
                { "id": "climb", "location": { "kind": "library", "path": "../../etc/passwd" } },
                { "id": "abs", "location": { "kind": "library", "path": "/etc/passwd" } },
                { "id": "rel", "location": { "kind": "external", "path": "relative/path.pdf" } },
                { "id": "nolocation" },
                42,
                { "id": "ext", "location": { "kind": "external", "path": "/Users/me/a.pdf" }, "title": "t\u{0}\nx", "tags": ["a", "A", " ", "b"] }
            ]
        });
        let (index, warnings) = Index::from_json(json.to_string().as_bytes()).unwrap();
        let ids: Vec<_> = index.entries.keys().cloned().collect();
        assert_eq!(ids, ["ext", "ok"]);
        assert_eq!(warnings.len(), 6, "{warnings:?}");
        assert_eq!(index.entries["ext"].tags, ["a", "b"]);
        assert_eq!(index.entries["ext"].title, "t  x");
    }

    #[test]
    fn a_file_that_is_not_an_index_is_an_error_and_a_newer_format_is_refused() {
        assert!(Index::from_json(b"not json").is_err());
        assert!(Index::from_json(br#"{"format":1}"#).is_err());
        assert!(Index::from_json(br#"{"format":99,"entries":[]}"#).unwrap_err().contains("newer"));
    }

    #[test]
    fn relative_paths_cannot_leave_the_folder() {
        for bad in ["", "..", "../a", "a/../b", "/a", "a//b", "a\\b", "./a", "a/\u{1}"] {
            assert!(!valid_relative(bad), "{bad:?}");
        }
        for good in ["a.pdf", "Documents/a b.pdf", "Documents/Sub/é.pdf"] {
            assert!(valid_relative(good), "{good:?}");
        }
    }

    #[test]
    fn the_index_round_trips_through_its_file() {
        let mut index = Index::default();
        index.entries.insert("d1".into(), entry("d1", 7));
        index.entries.insert("d2".into(), entry("d2", 8).tombstone(9));
        let (back, warnings) = Index::from_json(&index.to_json()).unwrap();
        assert_eq!(back, index);
        assert!(warnings.is_empty());
    }
}
