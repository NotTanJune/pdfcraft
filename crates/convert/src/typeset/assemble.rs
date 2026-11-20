//! The laid-out pages as a PDF document: page tree, fonts, pictures, links, bookmarks.

use pdfcraft_cos::{Dict, Document, ObjRef, Object, PdfString, Stream};
use pdfcraft_fonts::Std14;

use super::font::FontId;
use super::paginate::{Layout, OutlineItem, PageBuf};
use super::{Ctx, PageSetup, Typeset};
use crate::ConvertError;

/// Writes the footer numbers and builds the document.
pub(super) fn finish(mut ctx: Ctx, mut layout: Layout, title: &str, page_numbers: bool) -> Result<Typeset, ConvertError> {
    if layout.pages.is_empty() {
        // A document with no text at all is one blank page.
        layout.pages.push(PageBuf::default());
    }
    if page_numbers {
        number_pages(&mut ctx, &mut layout);
    }
    let fonts = ctx.fonts.resources(&mut ctx.doc);
    let mut font_dict = Dict::new();
    for (name, object) in fonts {
        font_dict.set(name.into_bytes(), Object::Ref(object));
    }
    let tree = page_tree(&ctx.doc)?;
    let mut kids = Vec::with_capacity(layout.pages.len());
    for buf in &layout.pages {
        kids.push(add_page(&mut ctx, &layout.setup, buf, &font_dict, tree));
    }
    let count = i64::try_from(kids.len()).map_err(|_| ConvertError::TooLarge("too many pages".into()))?;
    let page_objects: Vec<ObjRef> = kids.clone();
    ctx.doc
        .update_dict(tree, |d| {
            d.set(b"Count".to_vec(), Object::Int(count));
            d.set(b"Kids".to_vec(), Object::Array(kids.into_iter().map(Object::Ref).collect()));
        })
        .map_err(|e| ConvertError::Write(e.to_string()))?;
    add_outline(&mut ctx.doc, &layout.outline, &page_objects, layout.setup)?;
    set_info(&mut ctx.doc, title);
    let pages = page_objects.len();
    Ok(Typeset { document: ctx.doc, pages, warnings: ctx.warnings })
}

fn page_tree(doc: &Document) -> Result<ObjRef, ConvertError> {
    doc.root()
        .and_then(|r| doc.get(r).as_dict().and_then(|d| d.reference(b"Pages")))
        .ok_or_else(|| ConvertError::Write("the new document has no page tree".into()))
}

fn add_page(ctx: &mut Ctx, setup: &PageSetup, buf: &PageBuf, font_dict: &Dict, tree: ObjRef) -> ObjRef {
    let mut resources = Dict::new();
    resources.set(b"Font".to_vec(), Object::Dict(font_dict.clone()));
    let mut xobjects = Dict::new();
    for &index in &buf.pictures {
        if let Some(object) = ctx.pictures.get(index) {
            xobjects.set(format!("Im{}", index + 1).into_bytes(), Object::Ref(*object));
        }
    }
    if !xobjects.is_empty() {
        resources.set(b"XObject".to_vec(), Object::Dict(xobjects));
    }
    let mut page = Dict::new();
    page.set(b"Type".to_vec(), Object::name("Page"));
    page.set(b"Parent".to_vec(), Object::Ref(tree));
    page.set(b"MediaBox".to_vec(), Object::Array(vec![0.into(), 0.into(), Object::Real(setup.width), Object::Real(setup.height)]));
    page.set(b"Resources".to_vec(), Object::Dict(resources));
    if !buf.ops.is_empty() {
        let content = ctx.doc.add(Object::Stream(Stream::flate(Dict::new(), &buf.ops)));
        page.set(b"Contents".to_vec(), Object::Ref(content));
    }
    let mut annots = Vec::new();
    for a in &buf.annots {
        let mut action = Dict::new();
        action.set(b"S".to_vec(), Object::name("URI"));
        action.set(b"URI".to_vec(), PdfString::literal(a.uri.as_bytes().to_vec()));
        let mut d = Dict::new();
        d.set(b"Type".to_vec(), Object::name("Annot"));
        d.set(b"Subtype".to_vec(), Object::name("Link"));
        d.set(b"Rect".to_vec(), Object::Array(a.rect.iter().map(|v| Object::Real(*v)).collect()));
        d.set(b"Border".to_vec(), Object::Array(vec![0.into(), 0.into(), 0.into()]));
        d.set(b"A".to_vec(), Object::Dict(action));
        annots.push(Object::Ref(ctx.doc.add(d)));
    }
    if !annots.is_empty() {
        page.set(b"Annots".to_vec(), Object::Array(annots));
    }
    ctx.doc.add(page)
}

/// Page numbers, centred in the bottom margin.
fn number_pages(ctx: &mut Ctx, layout: &mut Layout) {
    let font = FontId::Std(Std14::Helvetica);
    ctx.fonts.use_font(font);
    let name = ctx.fonts.resource(font).to_string();
    let size = 9.0;
    let (w, bottom) = (layout.setup.width, layout.setup.margins[2]);
    for (i, page) in layout.pages.iter_mut().enumerate() {
        let text = (i + 1).to_string();
        let width = Std14::Helvetica.text_width(&text, size);
        let y = (bottom / 2.0).max(8.0);
        let op = format!("0.35 0.35 0.38 rg BT /{name} {size} Tf {:.2} {y:.2} Td ({text}) Tj ET\n", (w - width) / 2.0);
        page.ops.extend_from_slice(op.as_bytes());
    }
}

fn set_info(doc: &mut Document, title: &str) {
    let mut info = Dict::new();
    if !title.is_empty() {
        info.set(b"Title".to_vec(), PdfString::text(title));
    }
    info.set(b"Producer".to_vec(), PdfString::text("PdfCraft"));
    let r = doc.add(info);
    doc.trailer_mut().set(b"Info".to_vec(), Object::Ref(r));
}

/// The headings as bookmarks, nested by level.
fn add_outline(doc: &mut Document, items: &[OutlineItem], pages: &[ObjRef], setup: PageSetup) -> Result<(), ConvertError> {
    if items.is_empty() {
        return Ok(());
    }
    // Each item's parent (an index, or none for the top) from the heading levels.
    let mut parents: Vec<Option<usize>> = Vec::with_capacity(items.len());
    let mut stack: Vec<usize> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        while stack.last().is_some_and(|&k| items.get(k).is_some_and(|t| t.level >= item.level)) {
            stack.pop();
        }
        parents.push(stack.last().copied());
        stack.push(i);
    }
    let refs: Vec<ObjRef> = items.iter().map(|_| doc.add(Dict::new())).collect();
    let root = doc.add(Dict::new());
    let children = |parent: Option<usize>| -> Vec<usize> { parents.iter().enumerate().filter(|(_, p)| **p == parent).map(|(i, _)| i).collect() };
    let descendants = |i: usize| -> i64 {
        // Items under `i` in document order, until the next one at its level or above.
        let level = items.get(i).map_or(0, |t| t.level);
        let n = items.iter().skip(i + 1).take_while(|t| t.level > level).count();
        i64::try_from(n).unwrap_or(0)
    };
    for (i, item) in items.iter().enumerate() {
        let siblings = children(parents.get(i).copied().flatten());
        let at = siblings.iter().position(|k| *k == i).unwrap_or(0);
        let kids = children(Some(i));
        let page = pages.get(item.page).or(pages.first()).copied();
        let me = refs.get(i).copied();
        let (Some(me), Some(page)) = (me, page) else { continue };
        let parent = parents.get(i).copied().flatten().and_then(|p| refs.get(p).copied()).unwrap_or(root);
        let prev = at.checked_sub(1).and_then(|k| siblings.get(k)).and_then(|k| refs.get(*k).copied());
        let next = siblings.get(at + 1).and_then(|k| refs.get(*k).copied());
        let first = kids.first().and_then(|k| refs.get(*k).copied());
        let last = kids.last().and_then(|k| refs.get(*k).copied());
        let total = descendants(i);
        let top = item.y.clamp(0.0, setup.height);
        doc.update_dict(me, |d| {
            d.set(b"Title".to_vec(), PdfString::text(&item.title.chars().take(256).collect::<String>()));
            d.set(b"Parent".to_vec(), Object::Ref(parent));
            if let Some(p) = prev {
                d.set(b"Prev".to_vec(), Object::Ref(p));
            }
            if let Some(n) = next {
                d.set(b"Next".to_vec(), Object::Ref(n));
            }
            if let (Some(f), Some(l)) = (first, last) {
                d.set(b"First".to_vec(), Object::Ref(f));
                d.set(b"Last".to_vec(), Object::Ref(l));
                d.set(b"Count".to_vec(), Object::Int(total));
            }
            d.set(b"Dest".to_vec(), Object::Array(vec![Object::Ref(page), Object::name("XYZ"), Object::Real(0.0), Object::Real(top), Object::Null]));
        })
        .map_err(|e| ConvertError::Write(e.to_string()))?;
    }
    let tops = children(None);
    let first = tops.first().and_then(|k| refs.get(*k).copied());
    let last = tops.last().and_then(|k| refs.get(*k).copied());
    let count = i64::try_from(items.len()).unwrap_or(0);
    doc.update_dict(root, |d| {
        d.set(b"Type".to_vec(), Object::name("Outlines"));
        if let (Some(f), Some(l)) = (first, last) {
            d.set(b"First".to_vec(), Object::Ref(f));
            d.set(b"Last".to_vec(), Object::Ref(l));
        }
        d.set(b"Count".to_vec(), Object::Int(count));
    })
    .map_err(|e| ConvertError::Write(e.to_string()))?;
    if let Some(catalog) = doc.root() {
        doc.update_dict(catalog, |d| {
            d.set(b"Outlines".to_vec(), Object::Ref(root));
            d.set(b"PageMode".to_vec(), Object::name("UseOutlines"));
        })
        .map_err(|e| ConvertError::Write(e.to_string()))?;
    }
    Ok(())
}
