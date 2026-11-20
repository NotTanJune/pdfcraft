//! Tables out of a PDF as spreadsheets: Excel (.xlsx) and CSV.
//!
//! Each table the page layout detects ([`super::layout::tables`]) is one sheet, named for its page
//! (`Page 3 Table 2`). A page without a table gives one sheet of its text, a paragraph in each row.
//! Cells that read as plain numbers are written as numbers; everything else is text, never a
//! formula. CSV is the same rows, one table after another with an empty row between, and text that
//! a spreadsheet program would run as a formula gets a leading apostrophe.

use std::collections::HashMap;

use craft_xlsx::{
    Cell, CellRef, CellValue, Document, Font, Pattern, PatternFill, Range, Row, SheetEntry, SheetKind, Styles, Visibility, Worksheet, Xf,
};

use super::layout::{Item, Page, items};

/// Most sheets, rows and cells one export writes: past them the rest is left out. Excel itself
/// stops at 1,048,576 rows, and 32,767 characters in a cell.
const MAX_SHEETS: usize = 500;
const MAX_ROWS: usize = 1_048_575;
const MAX_CELLS: usize = 4_000_000;
const MAX_CELL_CHARS: usize = 32_767;

/// One cell of a sheet.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SheetCell {
    pub text: String,
    pub bold: bool,
    /// How many columns the cell covers (a header over sub-columns).
    pub span: usize,
}

/// A detected table, or a page's text, as rows of cells.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sheet {
    pub name: String,
    pub rows: Vec<Vec<SheetCell>>,
}

/// The sheets of `pages`: a table each, and for a page with no table its text lines.
pub(crate) fn sheets(pages: &[Page]) -> Vec<Sheet> {
    let mut out: Vec<Sheet> = Vec::new();
    let (mut page, mut table, mut tables_here) = (1usize, 0usize, 0usize);
    let mut lines: Vec<Vec<SheetCell>> = Vec::new();
    let end_page = |out: &mut Vec<Sheet>, lines: &mut Vec<Vec<SheetCell>>, page: usize, tables_here: usize| {
        if tables_here == 0 && !lines.is_empty() {
            out.push(Sheet { name: format!("Page {page}"), rows: std::mem::take(lines) });
        }
        lines.clear();
    };
    for item in items(pages) {
        match item {
            Item::Table(t) => {
                table += 1;
                tables_here += 1;
                let rows = t
                    .rows
                    .iter()
                    .map(|r| r.iter().map(|c| SheetCell { text: c.text.clone(), bold: c.bold, span: c.span.max(1) }).collect())
                    .collect();
                out.push(Sheet { name: format!("Page {page} Table {table}"), rows });
            }
            Item::Para(runs, _) => {
                let text: String = runs.iter().map(|r| r.text.as_str()).collect::<String>().trim().to_string();
                if !text.is_empty() {
                    let bold = runs.iter().all(|r| r.block.bold);
                    lines.push(vec![SheetCell { text, bold, span: 1 }]);
                }
            }
            Item::Img(_) => {}
            Item::PageBreak => {
                end_page(&mut out, &mut lines, page, tables_here);
                page += 1;
                table = 0;
                tables_here = 0;
            }
        }
        if out.len() >= MAX_SHEETS {
            return out;
        }
    }
    end_page(&mut out, &mut lines, page, tables_here);
    out.truncate(MAX_SHEETS);
    out
}

/// Whether `text` is a plain number a spreadsheet should hold as one: an optional minus sign,
/// digits and an optional decimal part, no leading zero (a code such as `007` stays text), at most
/// fifteen digits (what a number keeps exactly).
fn number(text: &str) -> Option<f64> {
    let t = text.trim();
    let digits = t.strip_prefix('-').unwrap_or(t);
    let (int, frac) = digits.split_once('.').map_or((digits, ""), |(i, f)| (i, f));
    let plain = !int.is_empty()
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.bytes().all(|b| b.is_ascii_digit())
        && (digits.contains('.') == !frac.is_empty())
        && (int == "0" || !int.starts_with('0'))
        && int.len() + frac.len() <= 15;
    if plain { t.parse::<f64>().ok().filter(|n| n.is_finite()) } else { None }
}

/// A sheet name Excel accepts: at most 31 characters without `[ ] : * ? / \`, and not already used.
pub(crate) fn sheet_name(wanted: &str, used: &[String]) -> String {
    let clean: String = wanted.chars().map(|c| if matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\') { ' ' } else { c }).take(31).collect();
    let base = if clean.trim().is_empty() { "Sheet".to_string() } else { clean.trim().to_string() };
    let taken = |n: &str| used.iter().any(|u| u.eq_ignore_ascii_case(n));
    let mut name = base.clone();
    let mut k = 2usize;
    while taken(&name) {
        let tail = format!(" ({k})");
        name = format!("{}{tail}", base.chars().take(31usize.saturating_sub(tail.chars().count())).collect::<String>());
        k += 1;
    }
    name
}

fn clipped(text: &str) -> String {
    text.chars().take(MAX_CELL_CHARS).collect()
}

/// An Excel workbook (.xlsx) with a sheet for each table of the document. `title` is the
/// workbook's name for the empty case.
pub fn xlsx(pages: &[Page], title: &str) -> Vec<u8> {
    let sheets = sheets(pages);
    let mut doc = Document::default();
    let mut strings: Vec<craft_xlsx::StringItem> = Vec::new();
    let mut index: HashMap<String, u32> = HashMap::new();
    let mut names: Vec<String> = Vec::new();
    let mut budget = MAX_CELLS;
    for sheet in &sheets {
        let name = sheet_name(&sheet.name, &names);
        names.push(name.clone());
        let mut ws = Worksheet::default();
        for (r, row) in sheet.rows.iter().take(MAX_ROWS).enumerate() {
            let mut cells = Vec::new();
            let mut col = 0u32;
            for c in row {
                if budget == 0 {
                    break;
                }
                budget -= 1;
                let at = CellRef::new(u32::try_from(r).unwrap_or(0), col);
                let value = match number(&c.text) {
                    Some(n) => CellValue::Number(n),
                    None => {
                        let text = clipped(&c.text);
                        let i = *index.entry(text.clone()).or_insert_with(|| {
                            strings.push(craft_xlsx::StringItem::plain(&text));
                            u32::try_from(strings.len() - 1).unwrap_or(0)
                        });
                        CellValue::Shared(i)
                    }
                };
                cells.push(Cell { at, style: u32::from(c.bold), value, ..Cell::default() });
                let span = u32::try_from(c.span.min(64)).unwrap_or(1);
                if span > 1 {
                    ws.part.merges.push(Range::new(at, CellRef::new(at.row, at.col.saturating_add(span - 1))));
                }
                col = col.saturating_add(span);
            }
            if !cells.is_empty() {
                ws.rows.push(Row { index: u32::try_from(r).unwrap_or(0), cells, ..Row::default() });
            }
        }
        doc.workbook.sheets.push(SheetEntry {
            name,
            sheet_id: u32::try_from(doc.workbook.sheets.len() + 1).unwrap_or(1),
            visibility: Visibility::Visible,
            rel_id: String::new(),
            kind: SheetKind::Worksheet,
            part: None,
        });
        doc.worksheets.push(Some(ws));
    }
    if doc.workbook.sheets.is_empty() {
        let name = sheet_name(if title.trim().is_empty() { "Sheet1" } else { title.trim() }, &[]);
        doc.workbook.sheets.push(SheetEntry {
            name,
            sheet_id: 1,
            visibility: Visibility::Visible,
            rel_id: String::new(),
            kind: SheetKind::Worksheet,
            part: None,
        });
        doc.worksheets.push(Some(Worksheet::default()));
    }
    doc.shared_strings = strings;
    doc.styles = Some(styles());
    // Writing a workbook built here fails only on a hostile size the limits above already bound.
    doc.write().unwrap_or_default()
}

/// Regular and bold Calibri 11, as a workbook needs at least: two default fills, a border and a
/// base style.
fn styles() -> Styles {
    let font = |bold: bool| Font { name: Some("Calibri".into()), size: Some(11.0), bold: bold.then_some(true), ..Font::default() };
    let pattern = |p| craft_xlsx::Fill::Pattern(PatternFill { pattern: Some(p), fg: None, bg: None });
    let xf = |font_id| Xf { font_id, ..Xf::default() };
    Styles {
        fonts: vec![font(false), font(true)],
        fills: vec![pattern(Pattern::None), pattern(Pattern::Gray125)],
        borders: vec![craft_xlsx::Border::default()],
        cell_style_xfs: vec![xf(0)],
        cell_xfs: vec![xf(0), xf(1)],
        cell_styles: vec![craft_xlsx::CellStyle { name: "Normal".into(), xf_id: 0, builtin_id: Some(0), ..Default::default() }],
        ..Styles::default()
    }
}

/// CSV (RFC 4180, comma separated, CRLF line ends) of the document's tables, one after another with
/// an empty row between. `only` picks one table or text sheet by its 1-based number, as
/// [`xlsx`] orders its sheets. Text that starts like a formula gets a leading apostrophe, so a
/// spreadsheet program shows it instead of running it. A byte order mark leads text with
/// characters beyond ASCII, so Excel reads it as UTF-8.
pub fn csv(pages: &[Page], only: Option<usize>) -> String {
    let all = sheets(pages);
    let picked: Vec<&Sheet> = match only {
        Some(n) => n.checked_sub(1).and_then(|i| all.get(i)).into_iter().collect(),
        None => all.iter().collect(),
    };
    let mut out = String::new();
    for (k, sheet) in picked.iter().enumerate() {
        if k > 0 {
            out.push_str("\r\n");
        }
        for row in sheet.rows.iter().take(MAX_ROWS) {
            let mut first = true;
            for cell in row {
                for filler in 0..cell.span.clamp(1, 64) {
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    // A cell spanning columns holds its text in the first and leaves the rest empty.
                    if filler == 0 {
                        craft_csv::push_field(&mut out, &neutralised(&cell.text), ',');
                    }
                }
            }
            out.push_str("\r\n");
        }
    }
    if out.is_ascii() { out } else { format!("\u{feff}{out}") }
}

/// `text` with a leading apostrophe when a spreadsheet would run it as a formula.
fn neutralised(text: &str) -> String {
    let t = text.trim_start();
    let formula = t.starts_with(['=', '+', '@', '\t', '\r']) || (t.starts_with('-') && number(t).is_none());
    if formula { format!("'{text}") } else { text.to_string() }
}
