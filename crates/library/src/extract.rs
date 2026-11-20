//! What the library learns from a PDF: its page count and its text.
//!
//! The text is the page text layer the viewer and Find already use, so a scanned document that
//! went through Scan & OCR is searchable through its recognised text, and one that did not is
//! listed with no text. Reading is bounded: at most [`MAX_PAGES`] pages are read and the text is
//! cut at [`MAX_TEXT_BYTES`].

use std::sync::Arc;

use pdfcraft_render::{PageRenderer, RenderConfig, RenderRequest, RequestKind};

use crate::error::{LibraryError, Result};
use crate::util::cut_bytes;

/// The most text kept for one document (200 KB); the rest is not searched.
pub const MAX_TEXT_BYTES: usize = 200_000;

/// The most pages read for their text.
pub const MAX_PAGES: usize = 5_000;

/// What was read from a document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// Pages the document has.
    pub pages: usize,
    /// Its text, pages one after another, cut at [`MAX_TEXT_BYTES`].
    pub text: String,
    /// The text was cut, or pages past [`MAX_PAGES`] were not read.
    pub truncated: bool,
    /// The document could not be opened (encrypted with a password not given, or damaged): it has
    /// no page count and no text.
    pub unreadable: bool,
}

/// Whether `bytes` start like a PDF (the header may follow up to 1 KiB of junk, as readers allow).
pub fn looks_like_pdf(bytes: &[u8]) -> bool {
    bytes.get(..bytes.len().min(1024)).is_some_and(|head| head.windows(5).any(|w| w == b"%PDF-"))
}

/// Reads the page count and text of `bytes`. A document that is not a PDF is an error; one that
/// is but cannot be opened (needs a password, or is damaged) gives [`Facts::unreadable`].
pub fn read_facts(bytes: Arc<Vec<u8>>, password: Option<&str>) -> Result<Facts> {
    if !looks_like_pdf(&bytes) {
        return Err(LibraryError::Invalid("that file is not a PDF".to_string()));
    }
    let config = RenderConfig { password: password.map(Arc::from), ..Default::default() };
    let mut renderer = PageRenderer::new(bytes, config);
    let pages = renderer.page_count();
    if pages == 0 {
        return Ok(Facts { unreadable: true, ..Facts::default() });
    }
    let mut text = String::new();
    let mut truncated = pages > MAX_PAGES;
    for page in 0..pages.min(MAX_PAGES) {
        let out = renderer.render(RenderRequest { page, kind: RequestKind::Text, tile: None, scale: 1.0, tag: 0, preview: false });
        let Some(page_text) = out.text.filter(|_| out.error.is_none()) else { continue };
        if page_text.is_empty() {
            continue;
        }
        let body = page_text.text_of(0..page_text.glyphs.len());
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&body);
        if text.len() > MAX_TEXT_BYTES {
            truncated = true;
            break;
        }
    }
    if text.len() > MAX_TEXT_BYTES {
        text = cut_bytes(&text, MAX_TEXT_BYTES).to_string();
    }
    Ok(Facts { pages, text, truncated, unreadable: false })
}

#[cfg(test)]
pub(crate) mod fixtures {
    /// A PDF of one page per entry of `pages`, each drawing its text in Helvetica.
    pub fn pdf(pages: &[&str]) -> Vec<u8> {
        build(pages, "")
    }

    /// The same, with the text invisible (render mode 3), as Scan & OCR lays recognised text over
    /// a scanned page.
    pub fn pdf_invisible(pages: &[&str]) -> Vec<u8> {
        build(pages, "3 Tr ")
    }

    fn build(pages: &[&str], mode: &str) -> Vec<u8> {
        let n = pages.len();
        let mut objs: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!(
                "<< /Type /Pages /Kids [{}] /Count {n} /MediaBox [0 0 300 400] /Resources << /Font << /F1 3 0 R >> >> >>",
                (0..n).map(|i| format!("{} 0 R", 4 + 2 * i)).collect::<Vec<_>>().join(" ")
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        for (i, text) in pages.iter().enumerate() {
            let content = format!("BT {mode}/F1 18 Tf 40 340 Td ({text}) Tj ET");
            objs.push(format!("<< /Type /Page /Parent 2 0 R /Contents {} 0 R >>", 5 + 2 * i));
            objs.push(format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()));
        }
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_and_text_are_read() {
        let bytes = fixtures::pdf(&["Force majeure clause", "Definitions of terms"]);
        let facts = read_facts(Arc::new(bytes), None).unwrap();
        assert_eq!(facts.pages, 2);
        assert!(facts.text.contains("Force majeure clause") && facts.text.contains("Definitions of terms"), "{:?}", facts.text);
        assert!(!facts.truncated && !facts.unreadable);
    }

    #[test]
    fn a_recognised_text_layer_is_read_like_any_other_text() {
        let bytes = fixtures::pdf_invisible(&["Invoice 4711 surveyor"]);
        let facts = read_facts(Arc::new(bytes), None).unwrap();
        assert!(facts.text.contains("surveyor"), "{:?}", facts.text);
    }

    #[test]
    fn something_that_is_not_a_pdf_is_refused() {
        assert!(read_facts(Arc::new(b"hello".to_vec()), None).is_err());
        assert!(read_facts(Arc::new(Vec::new()), None).is_err());
    }

    #[test]
    fn a_pdf_that_cannot_be_opened_is_unreadable_not_an_error() {
        let facts = read_facts(Arc::new(b"%PDF-1.7\ngarbage".to_vec()), None).unwrap();
        assert!(facts.unreadable && facts.pages == 0 && facts.text.is_empty());
    }
}
