//! The text of the library's documents, in a file of its own (`library-text.json`) so that the
//! index stays small. One bounded text per document; merged per document like the index.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::extract::MAX_TEXT_BYTES;
use crate::index::{FORMAT, MAX_ENTRIES, valid_id};
use crate::util::cut_bytes;

/// The text of one document and the version of the entry it was read for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEntry {
    /// `modified_ms` of the entry when the text was read: the merge keeps the newer.
    #[serde(default)]
    pub modified_ms: i64,
    /// Hash of the file the text came from.
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub text: String,
}

/// Document id to its text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextIndex {
    pub texts: BTreeMap<String, TextEntry>,
}

impl TextIndex {
    /// Joins `other`: per document, the text for the newer entry (the longer one on a tie).
    pub fn merge(&mut self, other: &TextIndex) {
        for (id, theirs) in &other.texts {
            let keep_mine = self
                .texts
                .get(id)
                .is_some_and(|mine| (mine.modified_ms, mine.text.len(), &mine.hash) >= (theirs.modified_ms, theirs.text.len(), &theirs.hash));
            if !keep_mine {
                self.texts.insert(id.clone(), theirs.clone());
            }
        }
    }

    /// Keeps only the documents whose ids `keep` accepts.
    pub fn retain_ids(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.texts.retain(|id, _| keep(id));
    }

    pub fn to_json(&self) -> Vec<u8> {
        let value = serde_json::json!({ "format": FORMAT, "texts": self.texts });
        let mut out = serde_json::to_vec(&value).unwrap_or_default();
        out.push(b'\n');
        out
    }

    /// Reads a text index file; bad documents are skipped, each text cut to [`MAX_TEXT_BYTES`].
    pub fn from_json(bytes: &[u8]) -> Result<(TextIndex, Vec<String>), String> {
        let value: Value = serde_json::from_slice(bytes).map_err(|e| format!("not a library text index: {e}"))?;
        let format = value.get("format").and_then(Value::as_u64).unwrap_or(0);
        if format == 0 || format > u64::from(FORMAT) {
            return Err(format!("library text index format {format} was written by a newer PdfCraft"));
        }
        let map = value.get("texts").and_then(Value::as_object).ok_or("not a library text index: no texts")?;
        if map.len() > MAX_ENTRIES {
            return Err(format!("the library text index lists {} documents; more than {MAX_ENTRIES} is not accepted", map.len()));
        }
        let mut out = TextIndex::default();
        let mut warnings = Vec::new();
        for (id, raw) in map {
            match (valid_id(id), serde_json::from_value::<TextEntry>(raw.clone())) {
                (true, Ok(mut entry)) => {
                    entry.text = cut_bytes(&entry.text, MAX_TEXT_BYTES).to_string();
                    out.texts.insert(id.clone(), entry);
                }
                _ => warnings.push(format!("skipped the text of {:?} in the library text index", cut_bytes(id, 40))),
            }
        }
        Ok((out, warnings))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(modified: i64, text: &str) -> TextEntry {
        TextEntry { modified_ms: modified, hash: "ab".into(), text: text.into() }
    }

    #[test]
    fn the_text_of_the_newer_entry_wins() {
        let mut a = TextIndex::default();
        a.texts.insert("d1".into(), t(1, "old"));
        let mut b = TextIndex::default();
        b.texts.insert("d1".into(), t(2, "new"));
        b.texts.insert("d2".into(), t(1, "other"));
        a.merge(&b);
        assert_eq!(a.texts["d1"].text, "new");
        assert_eq!(a.texts["d2"].text, "other");
        let mut c = b.clone();
        c.merge(&a);
        assert_eq!(c, a, "the same result from either side");
    }

    #[test]
    fn a_long_text_is_cut_when_read_and_bad_ids_are_skipped() {
        let long = "x".repeat(MAX_TEXT_BYTES + 100);
        let json = serde_json::json!({ "format": 1, "texts": { "ok": { "text": long }, "../bad": { "text": "x" }, "worse": 5 } });
        let (index, warnings) = TextIndex::from_json(json.to_string().as_bytes()).unwrap();
        assert_eq!(index.texts.len(), 1);
        assert_eq!(index.texts["ok"].text.len(), MAX_TEXT_BYTES);
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn the_text_index_round_trips() {
        let mut a = TextIndex::default();
        a.texts.insert("d1".into(), t(5, "héllo wörld"));
        assert_eq!(TextIndex::from_json(&a.to_json()).unwrap().0, a);
    }
}
