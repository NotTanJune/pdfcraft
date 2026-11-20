//! The fonts a typeset document is set in.
//!
//! Text is drawn with the standard fonts (Helvetica, Times, Courier in their four styles) in
//! WinAnsiEncoding. A character those cannot encode is drawn with the bundled Inter face
//! (Regular, or SemiBold for bold text; italic is a slant of the text matrix), which every build
//! has and which carries the Latin, Greek and Cyrillic letters and many symbols. A character
//! neither can draw is set as `?` and reported, never dropped without a word.

use std::collections::HashMap;

use pdfcraft_cos::{Dict, Document, ObjRef, Object};
use pdfcraft_fonts::pdf::Metrics;
use pdfcraft_fonts::{Std14, literal, win_ansi, win_ansi_byte};

/// The shear of the text matrix that slants an upright face into an oblique one (12 degrees).
pub(crate) const SLANT: f64 = 0.2126;

/// A typeface family, chosen from the font name a document asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Family {
    Sans,
    Serif,
    Mono,
}

impl Family {
    /// The family a document's font name stands for: monospace names, serif names, else sans.
    pub(crate) fn of_name(name: &str) -> Family {
        let n = name.to_ascii_lowercase();
        let has = |w: &[&str]| w.iter().any(|w| n.contains(w));
        if has(&["courier", "mono", "consolas", "menlo", "typewriter", "code", "fixed"]) {
            Family::Mono
        } else if !n.contains("sans")
            && has(&["times", "serif", "georgia", "garamond", "cambria", "palatino", "book", "minion", "century", "roman", "mincho"])
        {
            Family::Serif
        } else {
            Family::Sans
        }
    }
}

/// A font as drawn: one of the standard fonts, or the bundled face.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FontId {
    Std(Std14),
    Inter { bold: bool },
}

impl FontId {
    /// Whether this font is drawn with a slant of the text matrix rather than a face of its own.
    pub(crate) fn slanted(self, italic: bool) -> bool {
        italic && matches!(self, FontId::Inter { .. })
    }
}

/// The standard font of a family in a style.
pub(crate) fn std_face(family: Family, bold: bool, italic: bool) -> Std14 {
    match (family, bold, italic) {
        (Family::Sans, false, false) => Std14::Helvetica,
        (Family::Sans, true, false) => Std14::HelveticaBold,
        (Family::Sans, false, true) => Std14::HelveticaOblique,
        (Family::Sans, true, true) => Std14::HelveticaBoldOblique,
        (Family::Serif, false, false) => Std14::TimesRoman,
        (Family::Serif, true, false) => Std14::TimesBold,
        (Family::Serif, false, true) => Std14::TimesItalic,
        (Family::Serif, true, true) => Std14::TimesBoldItalic,
        (Family::Mono, false, false) => Std14::Courier,
        (Family::Mono, true, false) => Std14::CourierBold,
        (Family::Mono, false, true) => Std14::CourierOblique,
        (Family::Mono, true, true) => Std14::CourierBoldOblique,
    }
}

/// A stretch of text in one font.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Seg {
    pub text: String,
    pub font: FontId,
    pub width: f64,
}

struct InterFace {
    object: ObjRef,
    metrics: Metrics,
    /// Whether the face draws a character, asked once.
    draws: HashMap<char, bool>,
}

/// The fonts a document uses: their PDF objects, resource names and (for Inter) metrics.
#[derive(Default)]
pub(crate) struct Fonts {
    inter: [Option<InterFace>; 2],
    inter_missing: [bool; 2],
    std: HashMap<Std14, ObjRef>,
    /// Resource names in the order fonts were first used.
    names: Vec<(FontId, String)>,
    /// The first characters nothing could draw, for the report.
    pub(crate) undrawn: Vec<char>,
}

/// How many characters nothing could draw are remembered for the report.
const MAX_UNDRAWN: usize = 8;

impl Fonts {
    /// The resource name of `font`, which is added to the page resources when the document ends.
    fn name_of(&mut self, font: FontId) -> String {
        if let Some((_, n)) = self.names.iter().find(|(f, _)| *f == font) {
            return n.clone();
        }
        let name = format!("F{}", self.names.len() + 1);
        self.names.push((font, name.clone()));
        name
    }

    /// The resource name `font` was given when it was first measured.
    pub(crate) fn resource(&self, font: FontId) -> &str {
        self.names.iter().find(|(f, _)| *f == font).map_or("F1", |(_, n)| n.as_str())
    }

    fn inter_index(bold: bool) -> usize {
        usize::from(bold)
    }

    /// The bundled face, embedded in `doc` once; `None` when this build has none.
    fn inter_face(&mut self, doc: &mut Document, bold: bool) -> Option<&mut InterFace> {
        let i = Self::inter_index(bold);
        if self.inter.get(i).is_some_and(Option::is_none) && !self.inter_missing.get(i).copied().unwrap_or(true) {
            let made = Self::embed_inter(doc, bold);
            match made {
                Some(face) => {
                    if let Some(slot) = self.inter.get_mut(i) {
                        *slot = Some(face);
                    }
                }
                None => {
                    if let Some(m) = self.inter_missing.get_mut(i) {
                        *m = true;
                    }
                }
            }
        }
        self.inter.get_mut(i).and_then(Option::as_mut)
    }

    fn embed_inter(doc: &mut Document, bold: bool) -> Option<InterFace> {
        let face = pdfcraft_fonts::document_latin_fonts(bold).into_iter().next()?;
        let mut fonts = Dict::new();
        let name = pdfcraft_fonts::embedding::embed(doc, &mut fonts, face.bytes, face.family).ok()?;
        let object = fonts.reference(name.as_bytes())?;
        let dict = doc.dict(&Object::Ref(object))?;
        Some(InterFace { object, metrics: Metrics::from_dict(doc, &dict), draws: HashMap::new() })
    }

    /// Whether the bundled face draws `c`.
    fn inter_draws(&mut self, doc: &mut Document, bold: bool, c: char) -> bool {
        let Some(face) = self.inter_face(doc, bold) else { return false };
        if let Some(known) = face.draws.get(&c) {
            return *known;
        }
        let mut buf = [0u8; 4];
        let draws = face.metrics.encode(c.encode_utf8(&mut buf)).is_some();
        face.draws.insert(c, draws);
        draws
    }

    /// The font that draws `c` in a style, and the character to draw (`?` when nothing can).
    fn choose(&mut self, doc: &mut Document, family: Family, bold: bool, italic: bool, c: char) -> (FontId, char) {
        if win_ansi_byte(c).is_some() {
            return (FontId::Std(std_face(family, bold, italic)), c);
        }
        if self.inter_draws(doc, bold, c) {
            return (FontId::Inter { bold }, c);
        }
        if self.undrawn.len() < MAX_UNDRAWN && !self.undrawn.contains(&c) {
            self.undrawn.push(c);
        }
        (FontId::Std(std_face(family, bold, italic)), '?')
    }

    /// The advance of `text` in `font` at `size`.
    pub(crate) fn width(&self, font: FontId, text: &str, size: f64) -> f64 {
        match font {
            FontId::Std(f) => f.text_width(text, size),
            FontId::Inter { bold } => {
                let Some(m) = self.inter.get(Self::inter_index(bold)).and_then(Option::as_ref).map(|f| &f.metrics) else { return 0.0 };
                m.encode(text).map_or(0.0, |codes| m.codes(&codes).iter().map(|(code, _)| m.width(*code)).sum::<f64>() * size)
            }
        }
    }

    /// `text` as consecutive stretches each in one font, with their widths at `size`.
    pub(crate) fn segments(&mut self, doc: &mut Document, text: &str, family: Family, bold: bool, italic: bool, size: f64) -> Vec<Seg> {
        let mut out: Vec<Seg> = Vec::new();
        for c in text.chars() {
            let (font, drawn) = self.choose(doc, family, bold, italic, c);
            match out.last_mut() {
                Some(last) if last.font == font => last.text.push(drawn),
                _ => out.push(Seg { text: drawn.to_string(), font, width: 0.0 }),
            }
        }
        for seg in &mut out {
            self.name_of(seg.font);
            seg.width = self.width(seg.font, &seg.text, size);
        }
        out
    }

    /// The string operand that shows `text` in `font`.
    pub(crate) fn show(&self, font: FontId, text: &str) -> Vec<u8> {
        match font {
            FontId::Std(_) => literal(&win_ansi(text)),
            FontId::Inter { bold } => {
                let codes = self.inter.get(Self::inter_index(bold)).and_then(Option::as_ref).and_then(|f| f.metrics.encode(text)).unwrap_or_default();
                let mut out = Vec::with_capacity(codes.len().saturating_mul(2).saturating_add(2));
                out.push(b'<');
                for b in codes {
                    out.extend_from_slice(format!("{b:02X}").as_bytes());
                }
                out.push(b'>');
                out
            }
        }
    }

    /// The font objects for the page resources: the resource name and object of every font used.
    pub(crate) fn resources(&mut self, doc: &mut Document) -> Vec<(String, ObjRef)> {
        let used: Vec<(FontId, String)> = self.names.clone();
        let mut out = Vec::with_capacity(used.len());
        for (font, name) in used {
            let object = match font {
                FontId::Std(f) => *self.std.entry(f).or_insert_with(|| {
                    let mut d = Dict::new();
                    d.set(b"Type".to_vec(), Object::name("Font"));
                    d.set(b"Subtype".to_vec(), Object::name("Type1"));
                    d.set(b"BaseFont".to_vec(), Object::name(f.base_font()));
                    d.set(b"Encoding".to_vec(), Object::name("WinAnsiEncoding"));
                    doc.add(d)
                }),
                FontId::Inter { bold } => match self.inter.get(Self::inter_index(bold)).and_then(Option::as_ref) {
                    Some(face) => face.object,
                    None => continue,
                },
            };
            out.push((name, object));
        }
        out
    }

    /// Registers `font` (a footer, say) so it appears in the resources.
    pub(crate) fn use_font(&mut self, font: FontId) {
        self.name_of(font);
    }
}
