//! Ranking for the library and vault searches: a title, a tag or the text contains every word of
//! the query (ignoring case). Titles count most, then tags, then the text.

use crate::util::cut_bytes;

/// One searchable document.
#[derive(Clone, Debug)]
pub struct Item<'a> {
    pub title: &'a str,
    pub tags: &'a [String],
    pub text: &'a str,
}

/// Where a query matched, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Title,
    Tag,
    Text,
}

impl Field {
    pub fn name(self) -> &'static str {
        match self {
            Field::Title => "title",
            Field::Tag => "tag",
            Field::Text => "text",
        }
    }
}

/// How a document matched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub score: u32,
    pub fields: Vec<Field>,
    /// A piece of the text around the first hit, when the text matched.
    pub snippet: Option<String>,
}

/// The longest query accepted (bytes); the rest is ignored.
pub const MAX_QUERY_BYTES: usize = 512;
/// The most words of a query used.
pub const MAX_WORDS: usize = 12;
/// Characters of context on each side of a snippet's hit.
const SNIPPET_CONTEXT: usize = 60;

/// The lowercase words of `query`.
pub fn words(query: &str) -> Vec<String> {
    cut_bytes(query, MAX_QUERY_BYTES).split_whitespace().take(MAX_WORDS).map(str::to_lowercase).collect()
}

/// How `item` matches every one of `words` (all must be found somewhere), or `None`. No words
/// matches everything with score 0.
pub fn matches(item: &Item<'_>, words: &[String]) -> Option<Match> {
    if words.is_empty() {
        return Some(Match { score: 0, fields: Vec::new(), snippet: None });
    }
    let title = item.title.to_lowercase();
    let tags: Vec<String> = item.tags.iter().map(|t| t.to_lowercase()).collect();
    let text = item.text.to_lowercase();
    let mut score = 0u32;
    let (mut in_title, mut in_tag, mut in_text) = (false, false, false);
    for word in words {
        let (t, g, x) = (title.contains(word.as_str()), tags.iter().any(|tag| tag.contains(word.as_str())), text.contains(word.as_str()));
        if !(t || g || x) {
            return None;
        }
        if t {
            score = score.saturating_add(100);
            in_title = true;
        }
        if g {
            score = score.saturating_add(50);
            in_tag = true;
        }
        if x {
            score = score.saturating_add(10 + u32::try_from(text.matches(word.as_str()).take(20).count()).unwrap_or(20));
            in_text = true;
        }
    }
    let mut fields = Vec::new();
    for (on, f) in [(in_title, Field::Title), (in_tag, Field::Tag), (in_text, Field::Text)] {
        if on {
            fields.push(f);
        }
    }
    let snippet = if in_text { words.iter().find_map(|w| snippet(item.text, &text, w)) } else { None };
    Some(Match { score, fields, snippet })
}

/// The characters of `original` around the first hit of `word`. `lower` is `original` lowercased;
/// positions are mapped by character count because lowercasing can change byte lengths.
fn snippet(original: &str, lower: &str, word: &str) -> Option<String> {
    let byte = lower.find(word)?;
    let chars_before = lower.get(..byte)?.chars().count();
    let start = chars_before.saturating_sub(SNIPPET_CONTEXT);
    let len = SNIPPET_CONTEXT * 2 + word.chars().count();
    let piece: String = original.chars().skip(start).take(len).map(|c| if c.is_whitespace() { ' ' } else { c }).collect();
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(piece.trim());
    if original.chars().count() > start + len {
        out.push('…');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item<'a>(title: &'a str, tags: &'a [String], text: &'a str) -> Item<'a> {
        Item { title, tags, text }
    }

    #[test]
    fn every_word_must_be_found_and_case_does_not_matter() {
        let tags = vec!["Contracts".to_string()];
        let i = item("Lease agreement", &tags, "The tenant pays rent monthly. Force majeure applies.");
        assert!(matches(&i, &words("LEASE rent")).is_some());
        assert!(matches(&i, &words("lease unicorn")).is_none());
        assert!(matches(&i, &words("contract")).is_some(), "a tag matches by part of a word");
        assert!(matches(&i, &words("")).is_some());
    }

    #[test]
    fn a_title_beats_a_tag_beats_the_text() {
        let none: Vec<String> = vec![];
        let tag = vec!["budget".to_string()];
        let in_title = matches(&item("Budget", &none, ""), &words("budget")).unwrap();
        let in_tag = matches(&item("x", &tag, ""), &words("budget")).unwrap();
        let in_text = matches(&item("x", &none, "the budget"), &words("budget")).unwrap();
        assert!(in_title.score > in_tag.score && in_tag.score > in_text.score);
        assert_eq!(in_title.fields, [Field::Title]);
    }

    #[test]
    fn a_snippet_shows_the_hit_with_its_surroundings() {
        let none: Vec<String> = vec![];
        let long = format!("{} needle {}", "a ".repeat(100), "b ".repeat(100));
        let m = matches(&item("t", &none, &long), &words("needle")).unwrap();
        let s = m.snippet.unwrap();
        assert!(s.contains("needle") && s.starts_with('…') && s.ends_with('…') && s.chars().count() < 160, "{s}");
    }

    #[test]
    fn unicode_case_changes_do_not_break_the_snippet() {
        let none: Vec<String> = vec![];
        let text = "İstanbul İİİ rapor Straße gelir";
        let m = matches(&item("t", &none, text), &words("rapor")).unwrap();
        assert!(m.snippet.unwrap().contains("rapor"));
    }

    #[test]
    fn a_huge_query_is_bounded() {
        assert_eq!(words(&"w ".repeat(1000)).len(), MAX_WORDS);
    }
}
