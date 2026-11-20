//! LaTeX through craft-latex: the pages become the same `craft_wordmodel::Document` the RTF export
//! writes ([`super::as_word`], every page flowing), and craft-latex writes it as a standalone
//! `article`: headings as unnumbered sections, lists, `tabular` tables, bold, italic and links.
//! Pictures become their alternative text, as craft-latex writes them.

use super::as_word::{Meta, flow_doc};
use super::layout::Page;

/// The body text's font, as the RTF export names it.
const FONT: &str = "Helvetica";

/// A LaTeX document (.tex) of the pages, ready for pdfLaTeX, XeLaTeX or LuaLaTeX.
pub fn latex(pages: &[Page], title: &str) -> String {
    let (doc, _omitted) = flow_doc(pages, &Meta { title, language: None, font: FONT });
    craft_latex::export(&doc)
}
