//! Tables: column widths, and rows laid out one at a time as the pages fill.

use std::borrow::Cow;

use craft_flow::{Cell, FBlock, FTable};
use craft_wordmodel::props::{Border, BorderStyle, Borders, VAlign};
use pdfcraft_fonts::Std14;

use super::block::{Body, Column, LBlock};
use super::inline::{BLACK, Color, Line};
use super::{Ctx, MAX_DEPTH};

/// Space between a cell's border and its text, left and right, and above and below.
pub(crate) const PAD_X: f64 = 4.0;
pub(crate) const PAD_Y: f64 = 3.0;

/// The most grid columns one table has; more are left out.
pub(crate) const MAX_COLS: usize = 200;

/// The rows whose text sizes the columns: the rest of a long table is set in those widths.
const MEASURE_ROWS: usize = 3000;

/// The widest a column is made for one line of text, so one long cell cannot starve the rest.
const MAX_CELL_LINE: f64 = 1200.0;

const GRID: Color = [0.55, 0.55, 0.58];
const HEADER_FILL: Color = [0.92, 0.93, 0.95];

/// A table ready to be laid out row by row.
pub(crate) struct TableJob<'f> {
    pub table: Cow<'f, FTable>,
    /// The left edge of each grid column from the table's left edge, and the right edge of the last.
    pub col_x: Vec<f64>,
    pub left: f64,
    pub size: f64,
    pub depth: usize,
    /// How many rows at the top are headers, repeated on every page.
    pub header_rows: usize,
}

pub(crate) enum CellItem {
    Line(Box<Line>),
    Rule { x: f64, width: f64 },
}

pub(crate) struct CellLayout {
    pub x: f64,
    pub w: f64,
    /// Items with the space above each, from the top of the cell's text area.
    pub items: Vec<(f64, CellItem)>,
    pub content_h: f64,
    pub fill: Option<Color>,
    /// Top, left, bottom and right edges: width and colour.
    pub edges: [Option<(f64, Color)>; 4],
    pub valign: VAlign,
}

pub(crate) struct RowLayout {
    pub cells: Vec<CellLayout>,
    pub height: f64,
}

impl RowLayout {
    pub(crate) fn blank(height: f64) -> RowLayout {
        RowLayout { cells: Vec::new(), height }
    }
}

fn edge(b: &Option<Border>) -> Option<(f64, Color)> {
    let b = b.as_ref().filter(|b| b.style != BorderStyle::None && b.width.is_finite() && b.width > 0.0)?;
    let color = b.color.map_or(BLACK, |c| [f32::from(c.0) / 255.0, f32::from(c.1) / 255.0, f32::from(c.2) / 255.0]);
    Some((f64::from(b.width).clamp(0.25, 6.0), color))
}

fn edges(cell: &Cell, borderless: bool) -> [Option<(f64, Color)>; 4] {
    match &cell.borders {
        Some(Borders { top, left, bottom, right, .. }) => [edge(top), edge(left), edge(bottom), edge(right)],
        None if borderless => [None; 4],
        None => [Some((0.5, GRID)); 4],
    }
}

/// The width one line of `text` takes in Helvetica at `size`: close enough to size columns.
fn measure(text: &str, size: f64, bold: bool) -> f64 {
    let face = if bold { Std14::HelveticaBold } else { Std14::Helvetica };
    // Characters Helvetica has no code for count as an average one.
    let known: String = text.chars().filter(|c| pdfcraft_fonts::win_ansi_encodable(*c) && !c.is_control()).collect();
    let unknown = text.chars().filter(|c| !pdfcraft_fonts::win_ansi_encodable(*c) && !c.is_control()).count();
    face.text_width(&known, size) + unknown as f64 * size * 0.7
}

impl Ctx {
    /// A table as a block of rows to lay out as pages fill.
    pub(crate) fn table<'f>(&mut self, t: &'f FTable, col: Column, size: f64, depth: usize) -> LBlock<'f> {
        let col_x = self.column_edges(t, col.width, size);
        let header_rows = header_rows(t);
        // Cells that span rows leave placeholders in the rows below, so every row's cells sit
        // on the grid in order.
        let table = if t.rows.iter().flatten().any(|c| c.rowspan > 1) && !t.rows.iter().flatten().any(|c| c.covered) {
            let mut spanned = t.clone();
            spanned.insert_covered();
            Cow::Owned(spanned)
        } else {
            Cow::Borrowed(t)
        };
        let job = TableJob { table, col_x, left: col.left, size, depth, header_rows };
        LBlock { before: 4.0, after: 8.0, keep_next: false, page_break: false, body: Body::Table(Box::new(job)), outline: None }
    }

    /// Where each grid column starts, from the table's left edge; one more for its right edge.
    fn column_edges(&mut self, t: &FTable, avail: f64, size: f64) -> Vec<f64> {
        let cols = t.cols().clamp(1, MAX_COLS);
        let given: Vec<f64> = t.widths.iter().map(|w| if w.is_finite() { f64::from(*w).max(6.0) } else { 6.0 }).collect();
        let widths = if given.len() == cols && !t.widths.is_empty() { fit_given(given, avail) } else { self.auto_widths(t, cols, avail, size) };
        let mut edges = vec![0.0];
        let mut x = 0.0;
        for w in widths {
            x += w;
            edges.push(x);
        }
        edges
    }

    fn auto_widths(&mut self, t: &FTable, cols: usize, avail: f64, size: f64) -> Vec<f64> {
        let (min, max) = extents(t, cols, size);
        allocate(&min, &max, avail)
    }

    /// Row `r` of `job` laid out.
    pub(crate) fn row(&mut self, job: &TableJob<'_>, r: usize) -> RowLayout {
        let Some(row) = job.table.rows.get(r) else { return RowLayout::blank(0.0) };
        let cols = job.col_x.len().saturating_sub(1);
        let mut cells = Vec::with_capacity(row.len().min(cols));
        let mut g = 0usize;
        let mut height = job.size * super::inline::LEADING + 2.0 * PAD_Y;
        for cell in row {
            if g >= cols {
                break;
            }
            let span = (cell.colspan.clamp(1, 63) as usize).min(cols - g).max(1);
            if !cell.covered {
                let laid = self.cell(job, cell, g, span);
                height = height.max(laid.content_h + 2.0 * PAD_Y);
                cells.push(laid);
            }
            g += span;
        }
        RowLayout { cells, height }
    }

    fn cell(&mut self, job: &TableJob<'_>, cell: &Cell, g: usize, span: usize) -> CellLayout {
        let x0 = job.col_x.get(g).copied().unwrap_or(0.0);
        let x1 = job.col_x.get(g + span).copied().unwrap_or(x0 + 24.0);
        let w = (x1 - x0).max(8.0);
        let inner = (w - 2.0 * PAD_X).max(8.0);
        let flat;
        let blocks: &[FBlock] = if cell.blocks.iter().any(|b| matches!(b, FBlock::Table(_))) {
            flat = craft_flow::flatten_tables(&cell.blocks);
            &flat
        } else {
            &cell.blocks
        };
        let before = std::mem::replace(&mut self.bold_base, cell.header);
        let laid = self.blocks(blocks, Column { left: 0.0, width: inner }, job.size, true, job.depth.min(MAX_DEPTH));
        self.bold_base = before;
        let (items, content_h) = cell_items(laid);
        let fill =
            cell.shading.map(|c| [f32::from(c.0) / 255.0, f32::from(c.1) / 255.0, f32::from(c.2) / 255.0]).or(cell.header.then_some(HEADER_FILL));
        CellLayout { x: x0, w, items, content_h, fill, edges: edges(cell, job.table.borderless), valign: cell.valign.unwrap_or(VAlign::Top) }
    }
}

/// The lines of a cell's blocks with the space above each.
fn cell_items(blocks: Vec<LBlock<'_>>) -> (Vec<(f64, CellItem)>, f64) {
    let mut items = Vec::new();
    let mut y = 0.0;
    let mut prev_after = 0.0_f64;
    for (k, b) in blocks.into_iter().enumerate() {
        let mut gap = if k == 0 { 0.0 } else { prev_after.max(b.before) };
        match b.body {
            Body::Lines(lines) => {
                for line in lines {
                    y += gap + line.height;
                    items.push((gap, CellItem::Line(Box::new(line))));
                    gap = 0.0;
                }
            }
            Body::Rule { x, width } => {
                y += gap + 2.0;
                items.push((gap, CellItem::Rule { x, width }));
            }
            // Nested tables were flattened to paragraphs before laying out.
            Body::Table(_) => {}
        }
        prev_after = b.after;
    }
    (items, y)
}

/// The leading rows that are all header cells, or the table's own header row count.
fn header_rows(t: &FTable) -> usize {
    let declared = t.extra.as_ref().map_or(0, |e| e.header_rows);
    let leading = t.rows.iter().take_while(|r| !r.is_empty() && r.iter().all(|c| c.header)).count();
    declared.max(leading).min(3).min(t.rows.len().saturating_sub(1))
}

/// The shortest and longest each of `cols` columns can be set, padding included.
fn extents(t: &FTable, cols: usize, size: f64) -> (Vec<f64>, Vec<f64>) {
    let mut min = vec![0.0_f64; cols];
    let mut max = vec![0.0_f64; cols];
    for row in t.rows.iter().take(MEASURE_ROWS) {
        let mut g = 0usize;
        for cell in row {
            let span = (cell.colspan.clamp(1, 63) as usize).min(cols.saturating_sub(g).max(1));
            if span == 1 && !cell.covered {
                let (lo, hi) = text_extent(&cell.text(), size, cell.header);
                if let (Some(a), Some(b)) = (min.get_mut(g), max.get_mut(g)) {
                    *a = a.max(lo);
                    *b = b.max(hi);
                }
            }
            g += span;
        }
    }
    let pad = 2.0 * PAD_X + 1.0;
    for (lo, hi) in min.iter_mut().zip(max.iter_mut()) {
        *lo += pad;
        *hi = ((*hi).max(*lo - pad) + pad).min(MAX_CELL_LINE);
        *lo = (*lo).min(*hi);
    }
    (min, max)
}

/// How wide `t` is when no column wraps.
pub(crate) fn natural_width(t: &FTable, size: f64) -> f64 {
    let cols = t.cols().clamp(1, MAX_COLS);
    extents(t, cols, size).1.iter().sum()
}

/// The shortest and longest a cell's text may be set: its longest word and its longest line.
fn text_extent(text: &str, size: f64, bold: bool) -> (f64, f64) {
    let mut lo = 0.0_f64;
    let mut hi = 0.0_f64;
    for line in text.lines().take(50) {
        hi = hi.max(measure(line, size, bold));
        for word in line.split_whitespace() {
            lo = lo.max(measure(word, size, bold));
        }
    }
    (lo.min(MAX_CELL_LINE), hi)
}

/// Widths a document gives its grid, shrunk to the column when they overflow it.
fn fit_given(mut widths: Vec<f64>, avail: f64) -> Vec<f64> {
    let total: f64 = widths.iter().sum();
    if total > avail && total > 0.0 {
        let k = avail / total;
        for w in &mut widths {
            *w = (*w * k).max(6.0);
        }
    }
    widths
}

/// Column widths from their shortest and longest content in `avail` points: as wide as the
/// content when that fits; otherwise every column gets its shortest and the rest is shared by how
/// much more each wants.
fn allocate(min: &[f64], max: &[f64], avail: f64) -> Vec<f64> {
    let total_max: f64 = max.iter().sum();
    if total_max <= avail {
        return max.to_vec();
    }
    let total_min: f64 = min.iter().sum();
    if total_min >= avail {
        let k = if total_min > 0.0 { avail / total_min } else { 1.0 };
        return min.iter().map(|m| (m * k).max(6.0)).collect();
    }
    let spare = avail - total_min;
    let want: f64 = min.iter().zip(max).map(|(lo, hi)| hi - lo).sum();
    min.iter().zip(max).map(|(lo, hi)| lo + if want > 0.0 { spare * (hi - lo) / want } else { 0.0 }).collect()
}
