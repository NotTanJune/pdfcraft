//! The flow typesetter: a `craft_flow::Flow` (headings, paragraphs with styled runs, lists,
//! tables, pictures, links, equations as linear text) laid out onto PDF pages.
//!
//! Text is drawn with the standard fonts, and the bundled Inter face for what those cannot draw,
//! so it is selectable and searchable. Lines are broken greedily, paragraphs keep two lines
//! together at a page break, headings stay with the text after them, and table rows move whole to
//! the next page with their header rows repeated. Nothing here reads a font file or the system.

mod assemble;
mod block;
mod font;
mod inline;
mod paginate;
mod table;

use std::collections::HashMap;
use std::sync::Arc;

use craft_flow::{FBlock, Flow, Fmt, Inline, ListCounter};
use pdfcraft_cos::{Document, ObjRef};

use crate::ConvertError;
use block::Column;
use font::Fonts;
use inline::Picture;
use paginate::Layout;

pub(crate) use craft_flow::MAX_DEPTH;

/// A page's size and margins, in points. Margins are top, right, bottom, left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageSetup {
    pub width: f64,
    pub height: f64,
    pub margins: [f64; 4],
}

impl PageSetup {
    /// ISO A4, 210 by 297 mm, with 1-inch margins.
    pub const A4: PageSetup = PageSetup { width: 595.276, height: 841.89, margins: [72.0; 4] };
    /// US Letter, with 1-inch margins.
    pub const LETTER: PageSetup = PageSetup { width: 612.0, height: 792.0, margins: [72.0; 4] };

    /// The same page with every margin `m` points wide.
    pub fn with_margin(self, m: f64) -> PageSetup {
        PageSetup { margins: [m; 4], ..self }
    }

    /// The same page turned on its side.
    pub fn landscape(self) -> PageSetup {
        PageSetup { width: self.height, height: self.width, margins: [self.margins[3], self.margins[0], self.margins[1], self.margins[2]] }
    }

    /// Whether the page is usable: a finite size PDF allows, with room for text.
    pub(crate) fn check(&self) -> Result<(), ConvertError> {
        let sane = |v: f64| v.is_finite() && (3.0..=14_400.0).contains(&v);
        let room_w = self.width - self.margins[1] - self.margins[3];
        let room_h = self.height - self.margins[0] - self.margins[2];
        if sane(self.width) && sane(self.height) && self.margins.iter().all(|m| m.is_finite() && *m >= 0.0) && room_w >= 36.0 && room_h >= 36.0 {
            Ok(())
        } else {
            Err(ConvertError::Invalid("the page size and margins leave no room for text".into()))
        }
    }

    pub(crate) fn text_width(&self) -> f64 {
        self.width - self.margins[1] - self.margins[3]
    }
}

/// How a document is set.
#[derive(Clone, Debug)]
pub struct Options {
    pub page: PageSetup,
    /// The body text size in points (headings scale from it).
    pub font_size: f64,
    /// Number the pages at the foot of each.
    pub page_numbers: bool,
    /// The title for the document's information; the flow's own title when empty.
    pub title: String,
    /// Turn a portrait page on its side when a table is wider than its text area.
    pub fit_wide: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { page: PageSetup::LETTER, font_size: 11.0, page_numbers: false, title: String::new(), fit_wide: false }
    }
}

/// A typeset document.
pub struct Typeset {
    pub document: Document,
    pub pages: usize,
    /// What the conversion could not do faithfully, each said once.
    pub warnings: Vec<String>,
}

/// Everything layout needs while it runs.
pub(crate) struct Ctx {
    pub doc: Document,
    pub fonts: Fonts,
    pub work: usize,
    pub warnings: Vec<String>,
    pub lists: HashMap<u32, ListCounter>,
    /// The tallest a picture may be: the text area's height.
    pub picture_room: f64,
    /// Text in cells of a header row is bold.
    pub bold_base: bool,
    pictures: Vec<ObjRef>,
    picture_cache: HashMap<usize, Option<Picture>>,
    picture_bytes: usize,
    notes: Vec<(String, Vec<FBlock>)>,
    note_count: usize,
    in_notes: bool,
}

/// The largest picture file embedded, and all of them together.
const MAX_PICTURE_BYTES: usize = 64 << 20;
const MAX_PICTURE_TOTAL: usize = 512 << 20;
const MAX_NOTES: usize = 2000;
const MAX_WARNINGS: usize = 12;

impl Ctx {
    fn new(page: PageSetup) -> Ctx {
        Ctx {
            doc: Document::new_empty(),
            fonts: Fonts::default(),
            work: 0,
            warnings: Vec::new(),
            lists: HashMap::new(),
            picture_room: (page.height - page.margins[0] - page.margins[2]).max(36.0),
            bold_base: false,
            pictures: Vec::new(),
            picture_cache: HashMap::new(),
            picture_bytes: 0,
            notes: Vec::new(),
            note_count: 0,
            in_notes: false,
        }
    }

    /// Records something the conversion could not do, once.
    pub(crate) fn warn_once(&mut self, message: &str) {
        if self.warnings.len() < MAX_WARNINGS && !self.warnings.iter().any(|w| w == message) {
            self.warnings.push(message.to_string());
        }
    }

    /// A picture embedded in the document, shared by every place that draws it.
    pub(crate) fn picture(&mut self, data: &Arc<Vec<u8>>, ext: &str) -> Option<Picture> {
        let key = Arc::as_ptr(data) as usize;
        if let Some(known) = self.picture_cache.get(&key) {
            return *known;
        }
        let within = data.len() <= MAX_PICTURE_BYTES && self.picture_bytes.saturating_add(data.len()) <= MAX_PICTURE_TOTAL;
        let made = if within {
            self.picture_bytes = self.picture_bytes.saturating_add(data.len());
            pdfcraft_create::image_xobject(&mut self.doc, &format!("picture.{ext}"), data).ok().map(|(object, _)| {
                let index = self.pictures.len();
                self.pictures.push(object);
                Picture { index, object }
            })
        } else {
            None
        };
        self.picture_cache.insert(key, made);
        made
    }

    /// A note (footnote or endnote) met in text: its marker, and the note kept for the end.
    pub(crate) fn note(&mut self, _endnote: bool, custom: &str, blocks: &[FBlock]) -> String {
        self.note_count = self.note_count.saturating_add(1);
        let marker = if custom.trim().is_empty() { self.note_count.to_string() } else { custom.trim().chars().take(8).collect() };
        if !self.in_notes && self.notes.len() < MAX_NOTES {
            self.notes.push((marker.clone(), blocks.to_vec()));
        }
        marker
    }
}

/// Typesets `flow` into a PDF document.
pub fn typeset(flow: &Flow, opts: &Options) -> Result<Typeset, ConvertError> {
    opts.page.check()?;
    let size = if opts.font_size.is_finite() { opts.font_size.clamp(4.0, 48.0) } else { 11.0 };
    let mut page = opts.page;
    if opts.fit_wide && page.width < page.height {
        let widest = flow
            .blocks
            .iter()
            .filter_map(|b| if let FBlock::Table(t) = b { Some(table::natural_width(t, size)) } else { None })
            .fold(0.0_f64, f64::max);
        if widest > page.text_width() {
            page = page.landscape();
        }
    }
    let mut ctx = Ctx::new(page);
    let col = Column { left: 0.0, width: page.text_width() };
    let blocks = ctx.blocks(&flow.blocks, col, size, false, 0);
    let mut layout = Layout::new(page);
    layout.run(&mut ctx, blocks);
    set_notes(&mut ctx, &mut layout, col, size);
    if ctx.work > inline::MAX_PIECES {
        return Err(ConvertError::TooLarge("the document has more text than one conversion handles".into()));
    }
    if layout.truncated {
        ctx.warn_once("only the first 10,000 pages were made");
    }
    let undrawn: String = ctx.fonts.undrawn.iter().map(|c| format!("U+{:04X} ", u32::from(*c))).collect();
    if !undrawn.is_empty() {
        let message = format!("characters no available font draws are shown as ?: {}", undrawn.trim_end());
        ctx.warn_once(&message);
    }
    let title = if opts.title.trim().is_empty() { flow.meta.title.trim() } else { opts.title.trim() };
    assemble::finish(ctx, layout, title, opts.page_numbers)
}

/// The notes collected from the text, after it.
fn set_notes(ctx: &mut Ctx, layout: &mut Layout, col: Column, size: f64) {
    let notes = std::mem::take(&mut ctx.notes);
    if notes.is_empty() {
        return;
    }
    ctx.in_notes = true;
    let mut owned: Vec<FBlock> = Vec::new();
    let rule = craft_flow::Para { kind: craft_flow::Kind::Rule, ..Default::default() };
    owned.push(FBlock::Para(rule));
    for (marker, mut blocks) in notes {
        let label = Inline::Text(format!("{marker} "), Fmt { sup: true, ..Fmt::default() });
        if let Some(FBlock::Para(p)) = blocks.first_mut() {
            p.inlines.insert(0, label);
        }
        owned.extend(blocks);
    }
    let laid = ctx.blocks(&owned, col, size * 0.85, false, 0);
    layout.run(ctx, laid);
}
