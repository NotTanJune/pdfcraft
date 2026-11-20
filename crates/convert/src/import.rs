//! Documents read into the flow model: Word, Markdown and LaTeX.

use craft_flow::{Flow, FlowOpts};

use crate::ConvertError;
use crate::typeset::PageSetup;

/// The text of a file: its byte order mark or UTF-8, else the code page it most likely is.
pub(crate) fn text_of(bytes: &[u8]) -> String {
    pdfcraft_create::decode_text(bytes)
}

/// A Word document (.docx, .docm, .dotx, .dotm), and the page it asks for.
pub(crate) fn docx(bytes: &[u8]) -> Result<(Flow, Option<PageSetup>), ConvertError> {
    let doc = craft_docx::read(bytes).map_err(|e| ConvertError::Read(format!("the Word document could not be read: {e}")))?;
    let s = &doc.last_section;
    let page = PageSetup {
        width: f64::from(s.page_w),
        height: f64::from(s.page_h),
        margins: [s.margin_top, s.margin_right, s.margin_bottom, s.margin_left].map(f64::from),
    };
    let page = page.check().is_ok().then_some(page);
    // Footnotes and endnotes come with their markers; lists keep which list an item is in.
    let flow = craft_flow::from_doc_with(&doc, FlowOpts { notes: true, lists: true, ..FlowOpts::default() });
    Ok((flow, page))
}

pub(crate) fn markdown(bytes: &[u8]) -> Flow {
    craft_markdown::parse(&text_of(bytes))
}

pub(crate) fn latex(bytes: &[u8]) -> Flow {
    let cut = bytes.get(..craft_latex::MAX_SOURCE).unwrap_or(bytes);
    craft_latex::parse(&text_of(cut))
}
