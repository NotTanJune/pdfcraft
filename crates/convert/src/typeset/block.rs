//! Blocks of a flow: paragraphs, headings, lists, quotes and code laid out as lines.

use craft_flow::{FBlock, Inline, Kind, Para};
use craft_wordmodel::Align as WAlign;
use craft_wordmodel::props::LineSpacing;

use super::inline::{Align, Color, Geometry, Line, Piece, Style};
use super::table::TableJob;
use super::{Ctx, MAX_DEPTH};

/// The column text is set in: its left edge and width, from the text area's left edge.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Column {
    pub left: f64,
    pub width: f64,
}

/// What a block draws.
pub(crate) enum Body<'f> {
    Lines(Vec<Line>),
    /// A horizontal rule across the column, from `x`, `width` long.
    Rule {
        x: f64,
        width: f64,
    },
    Table(Box<TableJob<'f>>),
}

/// A laid-out block, with the space around it.
pub(crate) struct LBlock<'f> {
    pub before: f64,
    pub after: f64,
    /// Keep this block on a page with what follows it (a heading).
    pub keep_next: bool,
    pub page_break: bool,
    pub body: Body<'f>,
    /// A bookmark: its level and title.
    pub outline: Option<(u8, String)>,
}

/// How a kind of paragraph looks when the document says nothing else.
struct Look {
    style: Style,
    before: f64,
    after: f64,
    keep_next: bool,
    indent: f64,
    bar: bool,
    code: bool,
}

const CODE_BG: Color = [0.95, 0.95, 0.96];
const QUOTE_GRAY: Color = [0.3, 0.3, 0.33];

fn look(kind: Kind, size: f64) -> Look {
    let plain = || Style::plain(size);
    let mut l = Look { style: plain(), before: 0.0, after: 6.0, keep_next: false, indent: 0.0, bar: false, code: false };
    match kind {
        Kind::Normal | Kind::Rule => {}
        Kind::Heading(n) => {
            let (scale, before) = match n {
                0 | 1 => (1.8, 16.0),
                2 => (1.45, 14.0),
                3 => (1.2, 12.0),
                4 => (1.1, 10.0),
                _ => (1.0, 8.0),
            };
            l.style.size = size * scale;
            l.style.bold = true;
            l.before = before;
            l.keep_next = true;
        }
        Kind::Title => {
            l.style.size = size * 2.3;
            l.style.bold = true;
            l.after = 12.0;
            l.keep_next = true;
        }
        Kind::Quote => {
            l.style.color = QUOTE_GRAY;
            l.indent = 20.0;
            l.bar = true;
        }
        Kind::Code => {
            l.style.family = super::font::Family::Mono;
            l.style.size = size * 0.88;
            l.after = 0.0;
            l.code = true;
        }
    }
    l
}

fn align_of(a: WAlign) -> Align {
    match a {
        WAlign::Left => Align::Left,
        WAlign::Center => Align::Center,
        WAlign::Right => Align::Right,
        WAlign::Justify | WAlign::Distribute => Align::Justify,
    }
}

/// A finite, sane length in points.
fn pts(v: f32) -> f64 {
    if v.is_finite() { f64::from(v).clamp(-2000.0, 2000.0) } else { 0.0 }
}

/// Which part of a paragraph split at display equations is laid out.
#[derive(Clone, Copy)]
struct Part {
    display: bool,
    first: bool,
}

/// Where in a run of blocks a paragraph is, for the space around code and lists.
#[derive(Clone, Copy)]
struct Around {
    prev_code: bool,
    next_code: bool,
    cell: bool,
}

impl Ctx {
    /// The blocks of a flow, laid out in `col` at body size `size`.
    pub(crate) fn blocks<'f>(&mut self, blocks: &'f [FBlock], col: Column, size: f64, cell: bool, depth: usize) -> Vec<LBlock<'f>> {
        let mut out: Vec<LBlock<'f>> = Vec::new();
        let is_code = |b: Option<&FBlock>| matches!(b, Some(FBlock::Para(p)) if p.kind == Kind::Code);
        for (i, block) in blocks.iter().enumerate() {
            if self.work > super::inline::MAX_PIECES {
                break;
            }
            match block {
                FBlock::Para(p) => {
                    let around = Around { prev_code: i > 0 && is_code(blocks.get(i - 1)), next_code: is_code(blocks.get(i + 1)), cell };
                    out.extend(self.para(p, col, size, around));
                }
                FBlock::Table(t) => {
                    if depth >= MAX_DEPTH {
                        self.warn_once("tables nested too deeply are left out");
                        continue;
                    }
                    out.push(self.table(t, col, size, depth + 1));
                }
            }
        }
        out
    }

    /// A paragraph as one block, or several where display equations split it.
    fn para<'f>(&mut self, p: &Para, col: Column, size: f64, around: Around) -> Vec<LBlock<'f>> {
        if p.kind == Kind::Rule {
            return vec![LBlock {
                before: 6.0,
                after: 6.0,
                keep_next: false,
                page_break: p.page_break,
                body: Body::Rule { x: col.left, width: col.width },
                outline: None,
            }];
        }
        let mut parts: Vec<(Vec<&Inline>, bool)> = vec![(Vec::new(), false)];
        for inline in &p.inlines {
            if matches!(inline, Inline::Equation { display: true, .. }) {
                parts.push((vec![inline], true));
                parts.push((Vec::new(), false));
            } else if let Some((v, _)) = parts.last_mut() {
                v.push(inline);
            }
        }
        let n = parts.len();
        let mut out = Vec::new();
        for (k, (inlines, display)) in parts.into_iter().enumerate() {
            if inlines.is_empty() && n > 1 {
                continue;
            }
            let owned: Vec<Inline> = inlines.into_iter().cloned().collect();
            out.push(self.para_part(p, &owned, Part { display, first: k == 0 }, col, size, around));
        }
        out
    }

    fn para_part<'f>(&mut self, p: &Para, inlines: &[Inline], part: Part, col: Column, size: f64, around: Around) -> LBlock<'f> {
        let Part { display, first: first_part } = part;
        let mut look = look(p.kind, size);
        look.style.bold |= self.bold_base;
        let mut g = self.geometry(p, &look, col, display);
        let label = self.list_label(p, &look, &mut g);
        let mut pieces: Vec<Piece> = Vec::new();
        self.pieces(inlines, &look.style, &mut pieces, look.code);
        let mut lines = self.lines(pieces, &g, &look.style);
        if let (Some((x, run)), Some(first)) = (label, lines.first_mut()) {
            first.label = Some((x, run));
        }
        let (mut before, mut after) = (look.before, look.after);
        if look.code {
            before = if around.prev_code { 0.0 } else { 4.0 };
            after = if around.next_code { 0.0 } else { 6.0 };
        }
        if p.list.is_some() && !look.code {
            after = after.min(3.0);
        }
        before = p.spacing.before.map_or(before, |v| pts(v).max(0.0));
        after = p.spacing.after.map_or(after, |v| pts(v).max(0.0));
        if around.cell {
            before = 0.0;
            after = after.min(2.0);
        }
        let title = if matches!(p.kind, Kind::Heading(_) | Kind::Title) && first_part { p.shown_text().trim().to_string() } else { String::new() };
        let level = match p.kind {
            Kind::Heading(n) => n.clamp(1, 6),
            _ => 1,
        };
        LBlock {
            before: if first_part { before } else { before.min(4.0) },
            after,
            keep_next: look.keep_next || p.keep_next,
            page_break: p.page_break && first_part,
            body: Body::Lines(lines),
            outline: (!title.is_empty() && !around.cell).then_some((level, title)),
        }
    }

    fn geometry(&self, p: &Para, look: &Look, col: Column, display: bool) -> Geometry {
        let sp = &p.spacing;
        let left = col.left + look.indent + sp.left.map_or(0.0, |v| pts(v).max(0.0)) + p.list.map_or(0.0, |l| list_indent(l.level) + LIST_HANG);
        let right = sp.right.map_or(0.0, |v| pts(v).max(0.0));
        let width = (col.left + col.width - left - right).max(1.0);
        let mut g = Geometry::new(left, width);
        g.first = sp.first.map_or(0.0, pts).clamp(-g.left + col.left, g.width / 2.0);
        g.align = if display { Align::Center } else { p.align.map_or(Align::Left, align_of) };
        if look.code {
            g.verbatim = true;
            g.bg = Some((CODE_BG, col.left, col.width));
        }
        if look.bar {
            g.bar = Some(col.left + 6.0);
        }
        match sp.line {
            Some(LineSpacing::Multiple(m)) if m.is_finite() && m > 0.0 => g.multiple = f64::from(m).clamp(0.5, 5.0),
            Some(LineSpacing::Exactly(e)) if e.is_finite() && e > 0.0 => g.exact = Some(f64::from(e).clamp(1.0, 400.0)),
            Some(LineSpacing::AtLeast(a)) if a.is_finite() && a > 0.0 => g.at_least = Some(f64::from(a).clamp(1.0, 400.0)),
            _ => {}
        }
        g.max_picture = self.picture_room;
        g
    }

    /// The label of a list item (a bullet or a number) and its place; the paragraph itself is
    /// already indented past it.
    fn list_label(&mut self, p: &Para, look: &Look, g: &mut Geometry) -> Option<(f64, super::inline::Run)> {
        let li = p.list?;
        let text = self.lists.entry(li.list).or_default().label(li);
        let text = match text.as_str() {
            "◦" | "▪" => "-".to_string(),
            _ => text,
        };
        let mut style = look.style.clone();
        style.underline = false;
        style.link = None;
        let mut pieces = Vec::new();
        self.text_pieces(&text, &style, &mut pieces, false);
        let Some(Piece::Word { segs, width, .. }) = pieces.into_iter().next() else { return None };
        let seg = segs.into_iter().next()?;
        let x = g.left - LIST_HANG;
        let run = super::inline::Run { x: 0.0, text: seg.text, font: seg.font, style, width };
        Some((x, run))
    }
}

/// How far each list level is indented, and the space a label has before its text.
fn list_indent(level: u8) -> f64 {
    f64::from(level.min(8)) * 18.0
}

const LIST_HANG: f64 = 18.0;
