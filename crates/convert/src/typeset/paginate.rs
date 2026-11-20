//! Pages: blocks placed from the top, pages added as they fill, lines and rows drawn.

use craft_wordmodel::props::VAlign;

use super::block::{Body, LBlock};
use super::font::{FontId, Fonts, SLANT};
use super::inline::{Color, Line, Run};
use super::table::{CellItem, CellLayout, PAD_X, PAD_Y, RowLayout, TableJob};
use super::{Ctx, PageSetup};

/// Most pages one document has: the bound `pdfcraft-create` puts on a page tree.
pub(crate) const MAX_PAGES: usize = 10_000;

/// Most bookmarks one document has.
const MAX_OUTLINE: usize = 2000;

/// A link on a page: where it is and where it goes.
pub(crate) struct Annot {
    pub rect: [f64; 4],
    pub uri: String,
}

/// What is drawn on one page.
#[derive(Default)]
pub(crate) struct PageBuf {
    pub ops: Vec<u8>,
    pub annots: Vec<Annot>,
    /// The pictures drawn, by index into the document's pictures.
    pub pictures: Vec<usize>,
}

pub(crate) struct OutlineItem {
    pub level: u8,
    pub title: String,
    pub page: usize,
    /// Distance from the page's bottom edge to the top of the heading.
    pub y: f64,
}

/// The pages, and where the next block goes.
pub(crate) struct Layout {
    pub setup: PageSetup,
    pub pages: Vec<PageBuf>,
    pub outline: Vec<OutlineItem>,
    /// Pages past [`MAX_PAGES`] were left out.
    pub truncated: bool,
    /// How far down the text area the next block starts.
    y: f64,
    /// Nothing has been placed on the current page yet.
    fresh: bool,
    prev_after: f64,
}

fn n(v: f64) -> String {
    if v.is_finite() { format!("{:.2}", v.clamp(-1e6, 1e6)) } else { "0".to_string() }
}

fn rgb_op(c: Color, op: &str) -> String {
    format!("{:.3} {:.3} {:.3} {op}\n", c[0], c[1], c[2])
}

fn rect_fill(buf: &mut PageBuf, c: Color, x: f64, y: f64, w: f64, h: f64) {
    buf.ops.extend_from_slice(format!("{}{} {} {} {} re f\n", rgb_op(c, "rg"), n(x), n(y), n(w), n(h)).as_bytes());
}

fn stroke(buf: &mut PageBuf, c: Color, width: f64, from: (f64, f64), to: (f64, f64)) {
    buf.ops.extend_from_slice(format!("{}{} w {} {} m {} {} l S\n", rgb_op(c, "RG"), n(width), n(from.0), n(from.1), n(to.0), n(to.1)).as_bytes());
}

/// Whether a link may be written into the PDF: a web or mail address, nothing that runs code or
/// reaches into the reader's files.
pub(crate) fn safe_uri(uri: &str) -> bool {
    let lower = uri.trim().to_ascii_lowercase();
    ["http://", "https://", "mailto:"].iter().any(|p| lower.starts_with(p)) && uri.len() <= 2048 && !uri.chars().any(char::is_control)
}

impl Layout {
    pub(crate) fn new(setup: PageSetup) -> Layout {
        Layout { setup, pages: Vec::new(), outline: Vec::new(), truncated: false, y: 0.0, fresh: true, prev_after: 0.0 }
    }

    fn body_h(&self) -> f64 {
        self.setup.height - self.setup.margins[0] - self.setup.margins[2]
    }

    fn left(&self) -> f64 {
        self.setup.margins[3]
    }

    /// The page's top edge of text, in PDF coordinates.
    fn top(&self) -> f64 {
        self.setup.height - self.setup.margins[0]
    }

    fn remaining(&self) -> f64 {
        self.body_h() - self.y
    }

    fn new_page(&mut self) -> bool {
        if self.pages.len() >= MAX_PAGES {
            self.truncated = true;
            return false;
        }
        self.pages.push(PageBuf::default());
        self.y = 0.0;
        self.fresh = true;
        self.prev_after = 0.0;
        true
    }

    fn buf(&mut self) -> Option<&mut PageBuf> {
        if self.pages.is_empty() && !self.new_page() {
            return None;
        }
        self.pages.last_mut()
    }

    /// Lays out `blocks` one after another.
    pub(crate) fn run(&mut self, ctx: &mut Ctx, blocks: Vec<LBlock<'_>>) {
        if self.pages.is_empty() {
            self.new_page();
        }
        let mut iter = blocks.into_iter().peekable();
        while let Some(b) = iter.next() {
            if self.truncated {
                return;
            }
            if b.page_break && !self.fresh {
                self.new_page();
            }
            let gap = if self.fresh { 0.0 } else { self.prev_after.max(b.before) };
            if b.keep_next && !self.fresh {
                let next = iter.peek().map_or(0.0, |nb| first_height(nb));
                if gap + block_height(&b) + b.after + next > self.remaining() {
                    self.new_page();
                }
            }
            let gap = if self.fresh { 0.0 } else { gap };
            self.place(ctx, b, gap);
        }
    }

    fn place(&mut self, ctx: &mut Ctx, b: LBlock<'_>, gap: f64) {
        let after = b.after;
        match b.body {
            Body::Lines(lines) => {
                let first = self.place_lines(ctx, &lines, gap);
                if let (Some((level, title)), Some((page, y))) = (b.outline, first) {
                    self.note_outline(level, title, page, y);
                }
            }
            Body::Rule { x, width } => {
                if gap + 2.0 > self.remaining() && !self.fresh {
                    self.new_page();
                }
                let g = if self.fresh { 0.0 } else { gap };
                let (left, top) = (self.left(), self.top());
                let y = self.y + g + 1.0;
                if let Some(buf) = self.buf() {
                    stroke(buf, [0.6, 0.6, 0.63], 0.75, (left + x, top - y), (left + x + width, top - y));
                }
                self.y += g + 2.0;
                self.fresh = false;
            }
            Body::Table(job) => self.place_table(ctx, &job, gap),
        }
        self.prev_after = after;
    }

    fn note_outline(&mut self, level: u8, title: String, page: usize, y_from_top: f64) {
        if self.outline.len() >= MAX_OUTLINE {
            return;
        }
        let y = self.top() - y_from_top;
        self.outline.push(OutlineItem { level, title, page, y });
    }

    fn place_lines(&mut self, ctx: &mut Ctx, lines: &[Line], gap: f64) -> Option<(usize, f64)> {
        let mut start = 0usize;
        let mut gap = gap;
        let mut first = None;
        while start < lines.len() {
            if self.truncated {
                return first;
            }
            let rest = lines.get(start..).unwrap_or_default();
            let avail = self.remaining() - if self.fresh { 0.0 } else { gap };
            let mut fit = 0usize;
            let mut used = 0.0;
            for l in rest {
                if used + l.height > avail + 0.01 {
                    break;
                }
                used += l.height;
                fit += 1;
            }
            if fit < rest.len() {
                // Keep two lines together at a break, when the paragraph is long enough.
                if !self.fresh && (fit == 0 || (fit == 1 && rest.len() >= 4)) {
                    self.new_page();
                    gap = 0.0;
                    continue;
                }
                if fit == 0 {
                    // A line taller than a page: drawn anyway, past the margin.
                    fit = 1;
                } else if rest.len() - fit == 1 && fit >= 3 {
                    fit -= 1;
                }
            }
            let g = if self.fresh { 0.0 } else { gap };
            let (left, top, y0) = (self.left(), self.top(), self.y + g);
            if first.is_none() {
                first = Some((self.pages.len().saturating_sub(1), y0));
            }
            let placed = rest.get(..fit).unwrap_or_default();
            let mut y = y0;
            if let Some(buf) = self.pages.last_mut() {
                for l in placed {
                    draw_line(&ctx.fonts, buf, l, left, top - y);
                    y += l.height;
                }
            }
            self.y = y;
            self.fresh = false;
            start += fit;
            if start < lines.len() {
                self.new_page();
                gap = 0.0;
            }
        }
        first
    }

    fn place_table(&mut self, ctx: &mut Ctx, job: &TableJob<'_>, gap: f64) {
        let rows = job.table.rows.len();
        let headers: Vec<RowLayout> = (0..job.header_rows).map(|r| ctx.row(job, r)).collect();
        let header_h: f64 = headers.iter().map(|h| h.height).sum();
        let mut gap = gap;
        let mut r = 0usize;
        while r < rows {
            if self.truncated {
                return;
            }
            let laid;
            let row = match headers.get(r) {
                Some(h) => h,
                None => {
                    laid = ctx.row(job, r);
                    &laid
                }
            };
            let g = if self.fresh { 0.0 } else { gap };
            if g + row.height <= self.remaining() + 0.01 {
                self.draw_row(&ctx.fonts, job, row, self.y + g);
                self.y += g + row.height;
                self.fresh = false;
                gap = 0.0;
                r += 1;
                continue;
            }
            if !self.fresh {
                // Start a page, repeating the headers when there is room for them and a row.
                self.new_page();
                if r >= job.header_rows && header_h + row.height <= self.body_h() {
                    for h in &headers {
                        self.draw_row(&ctx.fonts, job, h, self.y);
                        self.y += h.height;
                        self.fresh = false;
                    }
                }
                gap = 0.0;
                continue;
            }
            // A row taller than a page, on a page of its own: split between its lines.
            self.split_row(ctx, job, row);
            gap = 0.0;
            r += 1;
        }
    }

    /// Draws a row that is taller than the page, a page's worth of each cell's lines at a time.
    fn split_row(&mut self, ctx: &mut Ctx, job: &TableJob<'_>, row: &RowLayout) {
        let mut starts = vec![0usize; row.cells.len()];
        let (left, top) = (self.left(), self.top());
        loop {
            let budget = (self.remaining() - 2.0 * PAD_Y).max(1.0);
            let mut taken = Vec::with_capacity(row.cells.len());
            let mut used = 0.0_f64;
            for (cell, start) in row.cells.iter().zip(&starts) {
                let (count, h) = fit_items(cell, *start, budget);
                taken.push(count);
                used = used.max(h);
            }
            let height = used + 2.0 * PAD_Y;
            if let Some(buf) = self.pages.last_mut() {
                for ((cell, start), count) in row.cells.iter().zip(&starts).zip(&taken) {
                    draw_cell(&ctx.fonts, buf, cell, (left + job.left, top - self.y), height, *start..*start + *count);
                }
            }
            self.y += height;
            self.fresh = false;
            for (s, c) in starts.iter_mut().zip(&taken) {
                *s += c;
            }
            let done = row.cells.iter().zip(&starts).all(|(cell, s)| *s >= cell.items.len());
            if done || self.truncated {
                return;
            }
            if !self.new_page() {
                return;
            }
        }
    }

    fn draw_row(&mut self, fonts: &Fonts, job: &TableJob<'_>, row: &RowLayout, y: f64) {
        let (left, top) = (self.left(), self.top());
        if let Some(buf) = self.buf() {
            for cell in &row.cells {
                draw_cell(fonts, buf, cell, (left + job.left, top - y), row.height, 0..cell.items.len());
            }
        }
    }
}

/// How many of a cell's items from `start` fit in `budget` points, and their height.
fn fit_items(cell: &CellLayout, start: usize, budget: f64) -> (usize, f64) {
    let mut used = 0.0;
    let mut count = 0;
    for (gap, item) in cell.items.iter().skip(start) {
        let h = gap_of(count, *gap) + item_height(item);
        if used + h > budget && count > 0 {
            break;
        }
        used += h;
        count += 1;
    }
    (count, used)
}

fn gap_of(count: usize, gap: f64) -> f64 {
    if count == 0 { 0.0 } else { gap }
}

fn item_height(item: &CellItem) -> f64 {
    match item {
        CellItem::Line(l) => l.height,
        CellItem::Rule { .. } => 2.0,
    }
}

fn block_height(b: &LBlock<'_>) -> f64 {
    match &b.body {
        Body::Lines(lines) => lines.iter().map(|l| l.height).sum(),
        Body::Rule { .. } => 2.0,
        Body::Table(_) => 0.0,
    }
}

/// The height of the part of a block that must stay with what precedes it: its first lines.
fn first_height(b: &LBlock<'_>) -> f64 {
    match &b.body {
        Body::Lines(lines) => b.before + lines.iter().take(2).map(|l| l.height).sum::<f64>(),
        Body::Rule { .. } => 2.0,
        Body::Table(job) => b.before + job.size * super::inline::LEADING * 2.0 + 4.0 * PAD_Y,
    }
}

/// Draws one cell: its fill, its lines (from item `start`, `count` of them) and its borders.
fn draw_cell(fonts: &Fonts, buf: &mut PageBuf, cell: &CellLayout, origin: (f64, f64), height: f64, items: std::ops::Range<usize>) {
    let (left, top) = origin;
    let (start, count) = (items.start, items.len());
    let (x, w) = (left + cell.x, cell.w);
    if let Some(c) = cell.fill {
        rect_fill(buf, c, x, top - height, w, height);
    }
    let slack = (height - 2.0 * PAD_Y - cell.content_h).max(0.0);
    let shift = if start == 0 && count == cell.items.len() {
        match cell.valign {
            VAlign::Top => 0.0,
            VAlign::Center => slack / 2.0,
            VAlign::Bottom => slack,
        }
    } else {
        0.0
    };
    // Each cell's text is a table-data marked-content sequence of its own, so that PdfCraft's text
    // reading (and so its exports) does not run neighbouring cells together into one line.
    let marked = count > 0;
    if marked {
        buf.ops.extend_from_slice(b"/TD BMC\n");
    }
    let mut y = top - PAD_Y - shift;
    for (k, (gap, item)) in cell.items.iter().skip(start).take(count).enumerate() {
        y -= gap_of(k, *gap);
        match item {
            CellItem::Line(line) => {
                draw_line(fonts, buf, line, x + PAD_X, y);
                y -= line.height;
            }
            CellItem::Rule { x: rx, width } => {
                stroke(buf, [0.6, 0.6, 0.63], 0.75, (x + PAD_X + rx, y - 1.0), (x + PAD_X + rx + width, y - 1.0));
                y -= 2.0;
            }
        }
    }
    if marked {
        buf.ops.extend_from_slice(b"EMC\n");
    }
    let [t, l, b, r] = cell.edges;
    if let Some((wd, c)) = t {
        stroke(buf, c, wd, (x, top), (x + w, top));
    }
    if let Some((wd, c)) = l {
        stroke(buf, c, wd, (x, top), (x, top - height));
    }
    if let Some((wd, c)) = b {
        stroke(buf, c, wd, (x, top - height), (x + w, top - height));
    }
    if let Some((wd, c)) = r {
        stroke(buf, c, wd, (x + w, top), (x + w, top - height));
    }
}

/// Draws a line whose top is at `y_top`, from column edge `left`.
pub(crate) fn draw_line(fonts: &Fonts, buf: &mut PageBuf, line: &Line, left: f64, y_top: f64) {
    if let Some((c, bx, bw)) = line.bg {
        rect_fill(buf, c, left + bx, y_top - line.height, bw, line.height);
    }
    if let Some(bx) = line.bar {
        rect_fill(buf, [0.75, 0.75, 0.78], left + bx - 3.0, y_top - line.height, 2.0, line.height);
    }
    let base_y = y_top - line.baseline;
    if let Some((lx, run)) = &line.label {
        draw_run(fonts, buf, run, left + lx, base_y, 0.0);
    }
    let mut extra = 0.0_f64;
    let mut link: Option<(String, f64, f64, f64, f64)> = None;
    for run in &line.runs {
        let x = left + line.x + run.x + extra;
        let spaces = if matches!(run.font, FontId::Std(_)) { run.text.matches(' ').count() } else { 0 };
        draw_run(fonts, buf, run, x, base_y, line.justify);
        let w = run.width + spaces as f64 * line.justify;
        extra += spaces as f64 * line.justify;
        let (lo, hi) = (base_y - 0.22 * run.style.size, base_y + 0.8 * run.style.size);
        match (&run.style.link, &mut link) {
            (Some(uri), Some((cur, _, _, x1, top))) if **uri == **cur => {
                *x1 = x + w;
                *top = top.max(hi);
            }
            (Some(uri), _) => {
                flush_link(buf, link.take());
                link = Some((uri.to_string(), x, lo, x + w, hi));
            }
            (None, _) => flush_link(buf, link.take()),
        }
    }
    flush_link(buf, link.take());
    for img in &line.images {
        let (x, y) = (left + line.x + img.x, y_top - line.baseline);
        buf.ops.extend_from_slice(format!("q {} 0 0 {} {} {} cm /Im{} Do Q\n", n(img.w), n(img.h), n(x), n(y), img.picture.index + 1).as_bytes());
        if !buf.pictures.contains(&img.picture.index) {
            buf.pictures.push(img.picture.index);
        }
    }
}

fn flush_link(buf: &mut PageBuf, link: Option<(String, f64, f64, f64, f64)>) {
    if let Some((uri, x0, y0, x1, y1)) = link
        && safe_uri(&uri)
        && x1 > x0
    {
        buf.annots.push(Annot { rect: [x0, y0, x1, y1], uri });
    }
}

fn draw_run(fonts: &Fonts, buf: &mut PageBuf, run: &Run, x: f64, base_y: f64, justify: f64) {
    if run.text.is_empty() {
        return;
    }
    let st = &run.style;
    let y = base_y + st.rise;
    let spaces = if matches!(run.font, FontId::Std(_)) { run.text.matches(' ').count() } else { 0 };
    let w = run.width + spaces as f64 * justify;
    if let Some(bg) = st.bg {
        rect_fill(buf, bg, x, y - 0.22 * st.size, w, 1.15 * st.size);
    }
    buf.ops.extend_from_slice(rgb_op(st.color, "rg").as_bytes());
    buf.ops.extend_from_slice(format!("BT /{} {} Tf ", fonts.resource(run.font), n(st.size)).as_bytes());
    if spaces > 0 && justify > 0.0 {
        buf.ops.extend_from_slice(format!("{} Tw ", n(justify)).as_bytes());
    }
    if run.font.slanted(st.italic) {
        buf.ops.extend_from_slice(format!("1 0 {SLANT} 1 {} {} Tm ", n(x), n(y)).as_bytes());
    } else {
        buf.ops.extend_from_slice(format!("{} {} Td ", n(x), n(y)).as_bytes());
    }
    buf.ops.extend(fonts.show(run.font, &run.text));
    buf.ops.extend_from_slice(b" Tj ET\n");
    let thickness = (st.size * 0.06).max(0.4);
    if st.underline {
        stroke(buf, st.color, thickness, (x, y - st.size * 0.12), (x + w, y - st.size * 0.12));
    }
    if st.strike {
        stroke(buf, st.color, thickness, (x, y + st.size * 0.28), (x + w, y + st.size * 0.28));
    }
}
