//! Inline content: styled words and spaces, broken into lines.

use std::sync::Arc;

use craft_flow::{Fmt, Img, Inline};
use pdfcraft_cos::ObjRef;

use super::Ctx;
use super::font::{Family, FontId, Seg};

/// An sRGB colour, components in 0..=1.
pub(crate) type Color = [f32; 3];

pub(crate) const BLACK: Color = [0.0, 0.0, 0.0];

/// Hyperlink blue, for a link whose document names no colour.
const LINK_BLUE: Color = [0.02, 0.39, 0.76];

/// The advance of a tab stop, in points.
const TAB_STOP: f64 = 36.0;

/// The height of a line of text, as a multiple of its font size.
pub(crate) const LEADING: f64 = 1.2;

/// A line of text with this baseline offset from its bottom, as a multiple of the font size.
const DESCENT: f64 = 0.25;

/// The most pieces one document may make: past it, conversion stops with an error rather than
/// run for minutes or fill memory (a file can hold far more words than any reader will page
/// through).
pub(crate) const MAX_PIECES: usize = 16_000_000;

/// How text looks: the font family and style, size, colour and decorations.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Style {
    pub family: Family,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub size: f64,
    pub color: Color,
    /// How far the baseline is raised (superscript) or lowered (subscript), in points.
    pub rise: f64,
    pub bg: Option<Color>,
    pub link: Option<Arc<str>>,
}

impl Style {
    pub(crate) fn plain(size: f64) -> Style {
        Style {
            family: Family::Sans,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            size,
            color: BLACK,
            rise: 0.0,
            bg: None,
            link: None,
        }
    }

    /// `base` with a run's own formatting on top.
    pub(crate) fn with(&self, f: &Fmt) -> Style {
        let mut s = self.clone();
        s.bold |= f.bold;
        s.italic |= f.italic;
        s.underline |= f.underline;
        s.strike |= f.strike;
        if f.code {
            s.family = Family::Mono;
        } else if let Some(font) = f.font.as_deref() {
            s.family = Family::of_name(font);
        }
        if let Some(size) = f.size.filter(|v| v.is_finite() && *v >= 1.0) {
            s.size = f64::from(size).min(200.0);
        }
        if let Some(c) = f.color {
            s.color = rgb(c);
        }
        if let Some(c) = f.background {
            s.bg = Some(rgb(c));
        }
        if f.sup {
            s.rise = s.size * 0.35;
            s.size *= 0.65;
        } else if f.sub {
            s.rise = -s.size * 0.15;
            s.size *= 0.65;
        }
        if let Some(link) = f.link.as_ref().filter(|l| !l.is_empty()) {
            s.link = Some(link.clone());
            if f.color.is_none() {
                s.color = LINK_BLUE;
            }
            s.underline = true;
        }
        s
    }
}

fn rgb(c: craft_wordmodel::Rgb) -> Color {
    [f32::from(c.0) / 255.0, f32::from(c.1) / 255.0, f32::from(c.2) / 255.0]
}

/// A picture ready to draw: the XObject and its index among the document's pictures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Picture {
    pub index: usize,
    pub object: ObjRef,
}

#[derive(Clone, Debug)]
pub(crate) enum Piece {
    Word { segs: Vec<Seg>, style: Style, width: f64 },
    Space { style: Style, width: f64 },
    Tab,
    Break,
    Image { picture: Picture, w: f64, h: f64 },
}

/// A stretch of text drawn in one font and style, at `x` from the line's left edge.
#[derive(Clone, Debug)]
pub(crate) struct Run {
    pub x: f64,
    pub text: String,
    pub font: FontId,
    pub style: Style,
    pub width: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PlacedImage {
    pub x: f64,
    pub picture: Picture,
    pub w: f64,
    pub h: f64,
}

/// One line of a paragraph, ready to draw.
#[derive(Clone, Debug, Default)]
pub(crate) struct Line {
    pub runs: Vec<Run>,
    pub images: Vec<PlacedImage>,
    /// Where the line starts, from the left edge of the text area (indent and alignment applied).
    pub x: f64,
    pub width: f64,
    pub height: f64,
    /// The baseline's distance below the top of the line.
    pub baseline: f64,
    /// Extra space for each space drawn in a standard font, to justify the line.
    pub justify: f64,
    /// A list label, drawn at this `x` from the text area's left edge.
    pub label: Option<(f64, Run)>,
    /// A background drawn across the whole column (code), from `bg_x` for `bg_w`.
    pub bg: Option<(Color, f64, f64)>,
    /// A vertical bar drawn at the left of the column (a quote), at this x.
    pub bar: Option<f64>,
}

/// How a paragraph is set: its column and how its lines sit in it.
#[derive(Clone, Debug)]
pub(crate) struct Geometry {
    /// The left edge of the column and its width.
    pub left: f64,
    pub width: f64,
    /// The first line starts this far further in (negative: hangs out).
    pub first: f64,
    pub align: Align,
    pub multiple: f64,
    pub exact: Option<f64>,
    pub at_least: Option<f64>,
    /// The tallest a picture may be.
    pub max_picture: f64,
    pub bg: Option<(Color, f64, f64)>,
    pub bar: Option<f64>,
    /// Lines never wrap between words (code): a line breaks only at its end or at the column.
    pub verbatim: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Align {
    Left,
    Center,
    Right,
    Justify,
}

impl Geometry {
    pub(crate) fn new(left: f64, width: f64) -> Geometry {
        Geometry {
            left,
            width: width.max(1.0),
            first: 0.0,
            align: Align::Left,
            multiple: 1.0,
            exact: None,
            at_least: None,
            max_picture: f64::INFINITY,
            bg: None,
            bar: None,
            verbatim: false,
        }
    }
}

/// What a line holds before its runs are made.
enum Atom {
    Word { segs: Vec<Seg>, style: Style, width: f64 },
    Space { style: Style, width: f64 },
    Gap(f64),
    Image { picture: Picture, w: f64, h: f64, width: f64 },
}

impl Atom {
    fn width(&self) -> f64 {
        match self {
            Atom::Word { width, .. } | Atom::Space { width, .. } | Atom::Image { width, .. } => *width,
            Atom::Gap(w) => *w,
        }
    }
}

struct Builder {
    atoms: Vec<Atom>,
    width: f64,
    /// A line that continues a paragraph's: its leading spaces are dropped.
    wrapped: bool,
}

impl Builder {
    fn new(wrapped: bool) -> Builder {
        Builder { atoms: Vec::new(), width: 0.0, wrapped }
    }

    fn has_content(&self) -> bool {
        self.atoms.iter().any(|a| !matches!(a, Atom::Space { .. } | Atom::Gap(_)))
    }

    fn push(&mut self, a: Atom) {
        self.width += a.width();
        self.atoms.push(a);
    }

    /// The trailing spaces are not part of the line.
    fn trim_end(&mut self) {
        while matches!(self.atoms.last(), Some(Atom::Space { .. })) {
            if let Some(a) = self.atoms.pop() {
                self.width -= a.width();
            }
        }
    }
}

impl Ctx {
    /// The pieces of a run of inlines in `base`, appended to `out`.
    pub(crate) fn pieces(&mut self, inlines: &[Inline], base: &Style, out: &mut Vec<Piece>, verbatim: bool) {
        for inline in inlines {
            if self.work > MAX_PIECES {
                return;
            }
            self.piece(inline, base, out, verbatim);
        }
    }

    fn piece(&mut self, inline: &Inline, base: &Style, out: &mut Vec<Piece>, verbatim: bool) {
        match inline {
            Inline::Text(t, f) => {
                let style = base.with(f);
                self.text_pieces(t, &style, out, verbatim);
            }
            Inline::Equation { linear, .. } => {
                let mut style = base.clone();
                style.family = Family::Serif;
                self.text_pieces(linear, &style, out, false);
            }
            Inline::Field { result: t, fmt, .. } | Inline::Control { text: t, fmt, .. } => {
                let style = base.with(fmt);
                self.text_pieces(t, &style, out, verbatim);
            }
            Inline::Ruby { base: b, fmt, .. } => {
                let style = base.with(fmt);
                self.text_pieces(b, &style, out, verbatim);
            }
            Inline::Image(img) => self.image_piece(img, base, out),
            Inline::Note { endnote, custom, blocks, fmt } => {
                let marker = self.note(*endnote, custom, blocks);
                let mut style = base.with(fmt);
                style.rise = style.size * 0.35;
                style.size *= 0.65;
                self.text_pieces(&marker, &style, out, false);
            }
            Inline::Chart(_) | Inline::Shape { .. } | Inline::Group { .. } => self.warn_once("charts, shapes and drawings are not drawn"),
            Inline::Anchor(_)
            | Inline::AnchorEnd(_)
            | Inline::Figure(_)
            | Inline::Comment { .. }
            | Inline::CommentEnd(_)
            | Inline::FieldStart(_)
            | Inline::FieldEnd => {}
        }
    }

    fn image_piece(&mut self, img: &Img, base: &Style, out: &mut Vec<Piece>) {
        match self.picture(&img.data, &img.ext) {
            Some(picture) if img.w.is_finite() && img.h.is_finite() && img.w > 0.0 && img.h > 0.0 => {
                out.push(Piece::Image { picture, w: f64::from(img.w), h: f64::from(img.h) });
            }
            _ => {
                let label = if img.alt.trim().is_empty() { "[picture]".to_string() } else { format!("[{}]", img.alt.trim()) };
                let mut style = base.clone();
                style.italic = true;
                self.text_pieces(&label, &style, out, false);
                self.warn_once("a picture that could not be embedded is shown as its description");
            }
        }
    }

    /// Words, spaces, tabs and line breaks of `text`.
    pub(crate) fn text_pieces(&mut self, text: &str, style: &Style, out: &mut Vec<Piece>, verbatim: bool) {
        let mut word = String::new();
        for c in text.chars() {
            match c {
                '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{2028}' | '\u{2029}' => {
                    self.word_piece(&mut word, style, out);
                    if c != '\r' || !word.is_empty() {
                        out.push(Piece::Break);
                    }
                }
                '\t' if verbatim => word.push_str("    "),
                '\t' => {
                    self.word_piece(&mut word, style, out);
                    out.push(Piece::Tab);
                }
                ' ' if verbatim => word.push(' '),
                c if c.is_whitespace() && c != '\u{a0}' => {
                    self.word_piece(&mut word, style, out);
                    self.space_piece(style, out);
                }
                '\u{ad}' | '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' | '\u{2060}' => {}
                c if c.is_control() => {}
                c => word.push(c),
            }
        }
        self.word_piece(&mut word, style, out);
    }

    fn word_piece(&mut self, word: &mut String, style: &Style, out: &mut Vec<Piece>) {
        if word.is_empty() {
            return;
        }
        let text = std::mem::take(word);
        let segs = self.fonts.segments(&mut self.doc, &text, style.family, style.bold, style.italic, style.size);
        let width = segs.iter().map(|s| s.width).sum();
        self.work = self.work.saturating_add(1);
        out.push(Piece::Word { segs, style: style.clone(), width });
    }

    fn space_piece(&mut self, style: &Style, out: &mut Vec<Piece>) {
        let segs = self.fonts.segments(&mut self.doc, " ", style.family, style.bold, style.italic, style.size);
        let width = segs.iter().map(|s| s.width).sum();
        self.work = self.work.saturating_add(1);
        out.push(Piece::Space { style: style.clone(), width });
    }

    /// The pieces broken into lines in the column `g`.
    pub(crate) fn lines(&mut self, pieces: Vec<Piece>, g: &Geometry, empty: &Style) -> Vec<Line> {
        let mut done: Vec<(Builder, bool)> = Vec::new();
        let mut cur = Builder::new(false);
        let first_room = (g.width - g.first).max(1.0);
        for piece in pieces {
            let room = if done.is_empty() { first_room } else { g.width };
            match piece {
                Piece::Break => {
                    cur.trim_end();
                    done.push((std::mem::replace(&mut cur, Builder::new(false)), true));
                }
                Piece::Space { style, width } => {
                    if !(cur.wrapped && !cur.has_content()) {
                        cur.push(Atom::Space { style, width });
                    }
                }
                Piece::Tab => {
                    let stop = ((cur.width / TAB_STOP).floor() + 1.0) * TAB_STOP;
                    cur.push(Atom::Gap((stop - cur.width).max(1.0)));
                }
                Piece::Word { segs, style, width } => self.place_word(&mut done, &mut cur, g, segs, style, width),
                Piece::Image { picture, w, h } => {
                    let scale = (room / w).min(g.max_picture / h).min(1.0);
                    let (w, h) = (w * scale, h * scale);
                    if cur.width + w > room && cur.has_content() {
                        cur.trim_end();
                        done.push((std::mem::replace(&mut cur, Builder::new(true)), false));
                    }
                    cur.push(Atom::Image { picture, w, h, width: w });
                }
            }
        }
        cur.trim_end();
        done.push((cur, true));
        done.into_iter().enumerate().map(|(i, (b, hard))| self.finish_line(b, g, i == 0, hard, empty)).collect()
    }

    fn place_word(&mut self, done: &mut Vec<(Builder, bool)>, cur: &mut Builder, g: &Geometry, segs: Vec<Seg>, style: Style, width: f64) {
        let room = if done.is_empty() { (g.width - g.first).max(1.0) } else { g.width };
        if cur.width + width <= room + 0.01 {
            cur.push(Atom::Word { segs, style, width });
            return;
        }
        if cur.has_content() {
            cur.trim_end();
            done.push((std::mem::replace(cur, Builder::new(true)), false));
        }
        let room = if done.is_empty() { (g.width - g.first).max(1.0) } else { g.width };
        if width <= room + 0.01 {
            cur.push(Atom::Word { segs, style, width });
            return;
        }
        // Wider than a whole line: broken between characters.
        for seg in segs {
            for c in seg.text.chars() {
                let mut buf = [0u8; 4];
                let text: &str = c.encode_utf8(&mut buf);
                let w = self.fonts.width(seg.font, text, style.size);
                let room = if done.is_empty() { (g.width - g.first).max(1.0) } else { g.width };
                if cur.width + w > room + 0.01 && cur.has_content() {
                    done.push((std::mem::replace(cur, Builder::new(true)), false));
                }
                let piece = Seg { text: text.to_string(), font: seg.font, width: w };
                match cur.atoms.last_mut() {
                    Some(Atom::Word { segs, width, .. }) if segs.last().is_some_and(|s| s.font == piece.font) => {
                        if let Some(last) = segs.last_mut() {
                            last.text.push(c);
                            last.width += w;
                        }
                        *width += w;
                        cur.width += w;
                    }
                    _ => cur.push(Atom::Word { segs: vec![piece], style: style.clone(), width: w }),
                }
            }
        }
    }

    /// A line from its atoms: runs merged, positions and heights worked out.
    fn finish_line(&mut self, b: Builder, g: &Geometry, first: bool, ends_paragraph: bool, empty: &Style) -> Line {
        let indent = if first { g.first } else { 0.0 };
        let room = (g.width - indent).max(1.0);
        let mut line = Line { bg: g.bg, bar: g.bar, ..Line::default() };
        let (mut x, mut max_size, mut image_h) = (0.0_f64, 0.0_f64, 0.0_f64);
        let mut spaces = 0usize;
        for atom in b.atoms {
            let width = atom.width();
            match atom {
                Atom::Word { segs, style, .. } => {
                    max_size = max_size.max(style.size);
                    let mut sx = x;
                    for seg in segs {
                        push_run(&mut line.runs, sx, seg.text, seg.font, &style, seg.width);
                        sx += seg.width;
                    }
                }
                Atom::Space { style, .. } => {
                    max_size = max_size.max(style.size);
                    let font = FontId::Std(super::font::std_face(style.family, style.bold, style.italic));
                    spaces += 1;
                    push_run(&mut line.runs, x, " ".to_string(), font, &style, width);
                }
                Atom::Gap(_) => {}
                Atom::Image { picture, w, h, .. } => {
                    image_h = image_h.max(h);
                    line.images.push(PlacedImage { x, picture, w, h });
                }
            }
            x += width;
        }
        line.width = x;
        if max_size == 0.0 {
            max_size = if image_h > 0.0 { 0.0 } else { empty.size };
        }
        let natural = max_size * LEADING * g.multiple;
        let text_h = match (g.exact, g.at_least) {
            (Some(e), _) => e,
            (None, Some(m)) => natural.max(m),
            (None, None) => natural,
        };
        line.height = text_h.max(image_h);
        line.baseline = if image_h > text_h { line.height } else { line.height - max_size * DESCENT };
        let shift = match g.align {
            Align::Left | Align::Justify => 0.0,
            Align::Center => ((room - line.width) / 2.0).max(0.0),
            Align::Right => (room - line.width).max(0.0),
        };
        line.x = g.left + indent + shift;
        if g.align == Align::Justify && !ends_paragraph && spaces > 0 && line.width < room {
            line.justify = (room - line.width) / spaces as f64;
        }
        line
    }
}

/// Adds a run, joining it to the one before when they continue each other.
fn push_run(runs: &mut Vec<Run>, x: f64, text: String, font: FontId, style: &Style, width: f64) {
    if let Some(last) = runs.last_mut()
        && last.font == font
        && last.style == *style
        && ((last.x + last.width) - x).abs() < 0.01
    {
        last.text.push_str(&text);
        last.width += width;
        return;
    }
    runs.push(Run { x, text, font, style: style.clone(), width });
}
