//! Page citations in an answer.
//!
//! The system prompt asks for `[p. 12]`. Models also write `[p.12]`, `[pp. 3-5]`, `[page 7]`,
//! `[p. 3, p. 7]` and `[pages 3 and 7]`; all of these are read. A bracket that is not a page
//! reference (`[1]`, `[sic]`, `[see below]`) is plain text. The pages are 1-based, as shown to
//! people, and only pages the document has are kept, so a made-up `[p. 4000]` is not a link.

use std::ops::Range;

/// The longest bracket considered. A citation is short; this bounds the scan of hostile text.
const MAX_BRACKET_BYTES: usize = 80;
/// The most pages one bracket can name (a range `3-500` names its two ends, not 498 pages).
const MAX_PAGES: usize = 12;
/// The widest range expanded into its pages.
const MAX_RANGE: usize = 8;

/// A citation: where it is in the answer and which pages it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Citation {
    /// The bracketed text, `[` and `]` included, as a byte range of the answer.
    pub range: Range<usize>,
    /// The pages, from 1, in order, without repeats.
    pub pages: Vec<usize>,
}

/// A piece of an answer for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment<'a> {
    Text(&'a str),
    Cite { text: &'a str, pages: Vec<usize> },
}

/// The pages named by the inside of a bracket (`p. 3, p. 7`), or `None` when it is not one.
fn pages_in(inner: &str) -> Option<Vec<usize>> {
    let lower = inner.trim().to_ascii_lowercase();
    let rest = ["pages", "page", "pp.", "pp", "p."].iter().find_map(|prefix| lower.strip_prefix(prefix)).or_else(|| lower.strip_prefix('p'))?;
    // `p` must be followed by a space, a dot or a digit: `[plan]` is not a page.
    if !rest.starts_with(|c: char| c.is_ascii_digit() || c == ' ' || c == '.') {
        return None;
    }
    let mut pages: Vec<usize> = Vec::new();
    let mut number: Option<usize> = None;
    let mut range_from: Option<usize> = None;
    let push = |pages: &mut Vec<usize>, from: Option<usize>, to: usize| {
        let named: Vec<usize> = match from {
            Some(from) if to > from && to - from <= MAX_RANGE => (from..=to).collect(),
            Some(from) => vec![from, to],
            None => vec![to],
        };
        for p in named {
            if !pages.contains(&p) && pages.len() < MAX_PAGES {
                pages.push(p);
            }
        }
    };
    let flush = |pages: &mut Vec<usize>, number: &mut Option<usize>, range_from: &mut Option<usize>| {
        if let Some(n) = number.take() {
            push(pages, range_from.take(), n);
        }
    };
    for c in rest.chars() {
        match c {
            '0'..='9' => {
                let digit = c.to_digit(10).map(|d| d as usize)?;
                let n = number.unwrap_or(0).checked_mul(10)?.checked_add(digit)?;
                if n > 9_999_999 {
                    return None;
                }
                number = Some(n);
            }
            '-' | '\u{2013}' => {
                // `3-5`: the number so far starts a range.
                range_from = Some(number.take()?);
            }
            ' ' | ',' | ';' | '.' | '&' | 'p' | 'a' | 'n' | 'd' | 'g' | 'e' | 's' => flush(&mut pages, &mut number, &mut range_from),
            _ => return None,
        }
    }
    flush(&mut pages, &mut number, &mut range_from);
    (!pages.is_empty()).then_some(pages)
}

/// The citations in `answer`, in order. With `page_count`, pages outside `1..=page_count` are
/// dropped, and a bracket left with none is not a citation.
pub fn find(answer: &str, page_count: Option<usize>) -> Vec<Citation> {
    let mut found = Vec::new();
    let bytes = answer.as_bytes();
    let mut at = 0;
    while let Some(open) = bytes.get(at..).and_then(|rest| rest.iter().position(|b| *b == b'[')).map(|i| at + i) {
        let window_end = answer.len().min(open + MAX_BRACKET_BYTES);
        // Cut at a character boundary: the window can end inside a multi-byte character.
        let window_end = (open..=window_end).rev().find(|i| answer.is_char_boundary(*i)).unwrap_or(open);
        let window = answer.get(open + 1..window_end).unwrap_or_default();
        let close = window.find([']', '[', '\n']).filter(|i| window.as_bytes().get(*i) == Some(&b']'));
        let Some(close) = close else {
            at = open + 1;
            continue;
        };
        let end = open + 1 + close + 1;
        let inner = window.get(..close).unwrap_or_default();
        if let Some(mut pages) = pages_in(inner) {
            if let Some(count) = page_count {
                pages.retain(|p| (1..=count).contains(p));
            }
            if !pages.is_empty() {
                found.push(Citation { range: open..end, pages });
            }
        }
        at = end;
    }
    found
}

/// `answer` split into plain text and citations, for display.
pub fn segments(answer: &str, page_count: Option<usize>) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut at = 0;
    for citation in find(answer, page_count) {
        if let Some(text) = answer.get(at..citation.range.start).filter(|t| !t.is_empty()) {
            out.push(Segment::Text(text));
        }
        if let Some(text) = answer.get(citation.range.clone()) {
            out.push(Segment::Cite { text, pages: citation.pages });
        }
        at = citation.range.end;
    }
    if let Some(text) = answer.get(at..).filter(|t| !t.is_empty()) {
        out.push(Segment::Text(text));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages(answer: &str) -> Vec<Vec<usize>> {
        find(answer, None).into_iter().map(|c| c.pages).collect()
    }

    #[test]
    fn the_requested_form_is_found() {
        let answer = "It says so [p. 12]. And again [p. 3] [p. 7].";
        assert_eq!(pages(answer), vec![vec![12], vec![3], vec![7]]);
        let c = &find(answer, None)[0];
        assert_eq!(&answer[c.range.clone()], "[p. 12]");
    }

    #[test]
    fn other_spellings_are_read() {
        assert_eq!(pages("[p.12]"), vec![vec![12]]);
        assert_eq!(pages("[P. 12]"), vec![vec![12]]);
        assert_eq!(pages("[page 7]"), vec![vec![7]]);
        assert_eq!(pages("[pp. 3-5]"), vec![vec![3, 4, 5]]);
        assert_eq!(pages("[pp. 3\u{2013}5]"), vec![vec![3, 4, 5]]);
        assert_eq!(pages("[p. 3, p. 7]"), vec![vec![3, 7]]);
        assert_eq!(pages("[pages 3 and 7]"), vec![vec![3, 7]]);
        assert_eq!(pages("[p. 3, 3]"), vec![vec![3]]);
    }

    #[test]
    fn brackets_that_are_not_pages_stay_text() {
        for text in ["[1]", "[sic]", "[see below]", "[plan]", "[p.]", "[p. x]", "[]", "[p. 3", "no brackets", "[p. 3\n]"] {
            assert!(find(text, None).is_empty(), "{text:?}");
        }
    }

    #[test]
    fn a_wide_range_names_its_ends_only() {
        assert_eq!(pages("[pp. 3-500]"), vec![vec![3, 500]]);
    }

    #[test]
    fn pages_the_document_lacks_are_dropped() {
        assert_eq!(find("[p. 4000]", Some(10)), vec![]);
        let kept: Vec<Vec<usize>> = find("[p. 3, p. 40]", Some(10)).into_iter().map(|c| c.pages).collect();
        assert_eq!(kept, vec![vec![3]]);
    }

    #[test]
    fn hostile_text_is_bounded_and_never_panics() {
        let long = format!("[p. {}]", "9".repeat(1000));
        assert!(find(&long, None).is_empty());
        let _ = find(&"[".repeat(10_000), None);
        let _ = find(&"é[p. é]".repeat(100), None);
        let at_edge = format!("[{}é", "a".repeat(MAX_BRACKET_BYTES - 2));
        let _ = find(&at_edge, None);
        assert!(find("[p. 99999999999999999999]", None).is_empty());
    }

    #[test]
    fn segments_cover_the_whole_answer() {
        let answer = "A [p. 2] B [p. 5] C";
        let parts = segments(answer, Some(9));
        assert_eq!(
            parts,
            vec![
                Segment::Text("A "),
                Segment::Cite { text: "[p. 2]", pages: vec![2] },
                Segment::Text(" B "),
                Segment::Cite { text: "[p. 5]", pages: vec![5] },
                Segment::Text(" C"),
            ]
        );
        let rebuilt: String = parts
            .iter()
            .map(|s| match s {
                Segment::Text(t) | Segment::Cite { text: t, .. } => *t,
            })
            .collect();
        assert_eq!(rebuilt, answer);
    }
}
