//! Delimited text and Excel workbooks read as tables in the flow model.

use craft_flow::{Cell, FBlock, FTable, Flow, Fmt, Inline, Kind, Para};
use craft_sheet::Value;
use craft_xlsx::{CellValue, Limits, Package, SheetKind, Styles, Visibility};

use crate::ConvertError;
use crate::numfmt;

/// The most table cells one conversion makes: each costs memory, and a page holds a few hundred
/// at most, so past this the pages would outnumber what a document may have anyway.
pub(crate) const MAX_CELLS: usize = 400_000;

/// The most columns of a table.
const MAX_COLS: usize = 200;

/// The longest cell text kept, in characters.
const MAX_CELL_CHARS: usize = 4000;

/// One cell on its way to the flow.
struct Item {
    text: String,
    numeric: bool,
    colspan: u32,
    rowspan: u32,
}

impl Item {
    fn text(text: String) -> Item {
        Item { text, numeric: false, colspan: 1, rowspan: 1 }
    }
}

fn clipped(text: &str) -> String {
    match text.char_indices().nth(MAX_CELL_CHARS) {
        Some((i, _)) => format!("{}…", text.get(..i).unwrap_or_default()),
        None => text.to_string(),
    }
}

fn cell(item: Item, header: bool) -> Cell {
    let inlines = if item.text.is_empty() { Vec::new() } else { vec![Inline::Text(item.text, Fmt::default())] };
    // Numbers stay left aligned like the text beside them: PdfCraft's own table detection reads a
    // column by the left edge of its cells, so a converted table exports back out as one.
    let para = Para { kind: Kind::Normal, inlines, ..Para::default() };
    Cell { blocks: vec![FBlock::Para(para)], header, colspan: item.colspan, rowspan: item.rowspan, ..Cell::default() }
}

fn table(rows: Vec<Vec<Item>>, header: bool) -> FTable {
    let rows = rows.into_iter().enumerate().map(|(r, row)| row.into_iter().map(|item| cell(item, header && r == 0)).collect()).collect();
    FTable { rows, widths: Vec::new(), borderless: false, extra: None, kept: None, wrap: None }
}

fn heading(text: &str) -> FBlock {
    FBlock::Para(Para { kind: Kind::Heading(1), inlines: vec![Inline::Text(text.to_string(), Fmt::default())], ..Para::default() })
}

fn note(text: &str) -> FBlock {
    FBlock::Para(Para { inlines: vec![Inline::Text(text.to_string(), Fmt { italic: true, ..Fmt::default() })], ..Para::default() })
}

fn is_number(text: &str) -> bool {
    let t = text.trim();
    !t.is_empty() && t.len() <= 40 && t.parse::<f64>().is_ok_and(f64::is_finite)
}

/// Delimited text (CSV, TSV) as one table, its first record the header. The warnings say what was
/// cut to stay within bounds.
pub(crate) fn csv(bytes: &[u8]) -> Result<(Flow, Vec<String>), ConvertError> {
    let text = craft_csv::decode_text(bytes);
    let delim = char::from(craft_csv::sniff_delimiter(&text, '"'));
    let max_records = MAX_CELLS / 2;
    let mut records = craft_csv::records(&text, delim, '"', max_records.saturating_add(1), MAX_COLS.saturating_add(1));
    let mut warnings = Vec::new();
    if records.len() > max_records {
        records.truncate(max_records);
        warnings.push(format!("only the first {max_records} rows were converted"));
    }
    let mut cells = 0usize;
    let mut cut_columns = false;
    let mut rows: Vec<Vec<Item>> = Vec::with_capacity(records.len());
    for mut record in records {
        if record.len() > MAX_COLS {
            record.truncate(MAX_COLS);
            cut_columns = true;
        }
        cells = cells.saturating_add(record.len());
        if cells > MAX_CELLS {
            warnings.push(format!("only the first {MAX_CELLS} cells were converted"));
            break;
        }
        rows.push(record.into_iter().map(|f| Item { numeric: is_number(&f), ..Item::text(clipped(&f)) }).collect());
    }
    if cut_columns {
        warnings.push(format!("only the first {MAX_COLS} columns were converted"));
    }
    // Rows shorter than the widest are padded, so every column lines up.
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    for row in &mut rows {
        while row.len() < width {
            row.push(Item::text(String::new()));
        }
    }
    let blocks = if rows.is_empty() { vec![note("(no data)")] } else { vec![FBlock::Table(table(rows, true))] };
    Ok((Flow { blocks, meta: Default::default() }, warnings))
}

/// What a sheet holds: its cells by row, sparse.
struct Grid {
    rows: Vec<(u32, Vec<(u32, Item)>)>,
}

/// A workbook: each visible sheet as a heading and a table of its shown values.
pub(crate) fn xlsx(bytes: &[u8]) -> Result<(Flow, Vec<String>), ConvertError> {
    let read = |e: craft_xlsx::Error| ConvertError::Read(format!("the workbook could not be read: {e}"));
    let mut pkg = Package::open(bytes, &Limits::default()).map_err(read)?;
    let book = pkg.workbook().map_err(read)?;
    let strings = pkg.shared_strings().map_err(read)?;
    let styles = pkg.styles().map_err(read)?;
    let mut blocks = Vec::new();
    let mut warnings = Vec::new();
    let mut budget = MAX_CELLS;
    let mut unsaved_formulas = 0usize;
    for entry in &book.sheets {
        if entry.kind != SheetKind::Worksheet || entry.visibility != Visibility::Visible {
            continue;
        }
        let mut grid = Grid { rows: Vec::new() };
        let part = pkg
            .sheet(entry, |row| {
                if row.hidden || budget == 0 {
                    return Ok(());
                }
                let mut items = Vec::new();
                for c in row.cells {
                    if c.formula.is_some() && matches!(c.value, CellValue::Empty) {
                        unsaved_formulas += 1;
                    }
                    let text = shown(&c.value, c.style, &strings, styles.as_ref(), book.date_1904);
                    let Some((text, right)) = text else { continue };
                    if budget == 0 {
                        break;
                    }
                    budget -= 1;
                    items.push((c.at.col, Item { numeric: right, ..Item::text(clipped(&text)) }));
                }
                if !items.is_empty() {
                    grid.rows.push((row.index, items));
                }
                Ok(())
            })
            .map_err(read)?;
        if budget == 0 && !warnings.iter().any(|w: &String| w.contains("cells")) {
            warnings.push(format!("only the first {MAX_CELLS} cells were converted"));
        }
        blocks.push(heading(&entry.name));
        let hidden: Vec<(u32, u32)> = part.columns.iter().filter(|c| c.hidden).map(|c| (c.first, c.last)).collect();
        match sheet_table(grid, &part.merges, &hidden) {
            Some((t, cut)) => {
                if cut {
                    warnings.push(format!("sheet \"{}\": only the first {MAX_COLS} columns were converted", entry.name));
                }
                blocks.push(FBlock::Table(t));
            }
            None => blocks.push(note("(empty sheet)")),
        }
    }
    if blocks.is_empty() {
        blocks.push(note("(the workbook has no visible sheets)"));
    }
    if unsaved_formulas > 0 {
        warnings.push(format!("{unsaved_formulas} formula cells have no saved result (formulas are not calculated) and are shown empty"));
    }
    Ok((Flow { blocks, meta: Default::default() }, warnings))
}

/// The text a cell shows and whether it is a number. `None` for an
/// empty cell.
fn shown(value: &CellValue, style: u32, strings: &[craft_xlsx::StringItem], styles: Option<&Styles>, date_1904: bool) -> Option<(String, bool)> {
    match value.resolve(strings, date_1904) {
        Value::Empty => None,
        Value::Text(t) => Some((t.to_string(), false)),
        Value::Bool(b) => Some((if b { "TRUE" } else { "FALSE" }.to_string(), false)),
        Value::Error(e) => Some((e.to_string(), false)),
        Value::Number(n) => {
            let code = styles.and_then(|s| format_code(s, style));
            Some((numfmt::format(n, code.as_deref().unwrap_or("General"), date_1904), true))
        }
    }
}

/// The number format code of a cell format, custom or built in.
fn format_code(styles: &Styles, style: u32) -> Option<String> {
    let xf = styles.cell_xfs.get(usize::try_from(style).ok()?)?;
    match styles.num_fmts.iter().find(|(id, _)| *id == xf.num_fmt_id) {
        Some((_, code)) => Some(code.clone()),
        None => craft_sheet::builtin_format(xf.num_fmt_id).map(str::to_string),
    }
}

/// A sheet's cells as a table over the columns it uses, merged cells spanning. `None` for a sheet
/// with nothing in it; the flag says whether columns were cut.
fn sheet_table(grid: Grid, merges: &[craft_sheet::Range], hidden: &[(u32, u32)]) -> Option<(FTable, bool)> {
    let (min, max) = grid.rows.iter().flat_map(|(_, r)| r.iter().map(|(c, _)| *c)).fold((u32::MAX, 0u32), |(lo, hi), c| (lo.min(c), hi.max(c)));
    if grid.rows.is_empty() || min > max {
        return None;
    }
    let is_hidden = |c: u32| hidden.iter().any(|(a, b)| (*a..=*b).contains(&c));
    let cols: Vec<u32> = (min..=max).filter(|c| !is_hidden(*c)).collect();
    let cut = cols.len() > MAX_COLS;
    let cols: Vec<u32> = cols.into_iter().take(MAX_COLS).collect();
    let mut rows: Vec<Vec<Item>> = Vec::new();
    let mut last_row: Option<u32> = None;
    let mut header = false;
    for (r, cells) in grid.rows {
        // Blank rows between used ones keep their place, a few of them.
        if let Some(prev) = last_row {
            for _ in 0..r.saturating_sub(prev).saturating_sub(1).min(2) {
                rows.push(cols.iter().map(|_| Item::text(String::new())).collect());
            }
        }
        last_row = Some(r);
        if rows.is_empty() {
            header = looks_like_header(&cells);
        }
        rows.push(grid_row(r, cells, &cols, merges));
    }
    Some((table(rows, header), cut))
}

/// A first row of two or more text cells is a header, when more rows follow.
fn looks_like_header(cells: &[(u32, Item)]) -> bool {
    cells.len() >= 2 && cells.iter().all(|(_, i)| !i.numeric && !i.text.is_empty())
}

/// One row over `cols`: empty cells where the row has none, and merged cells as one that spans.
fn grid_row(r: u32, cells: Vec<(u32, Item)>, cols: &[u32], merges: &[craft_sheet::Range]) -> Vec<Item> {
    let mut by_col: std::collections::HashMap<u32, Item> = cells.into_iter().collect();
    let mut out = Vec::with_capacity(cols.len());
    let mut skip = 0usize;
    for (k, &c) in cols.iter().enumerate() {
        if skip > 0 {
            skip -= 1;
            continue;
        }
        let at = craft_sheet::CellRef::new(r, c);
        let merge = merges.iter().find(|m| m.contains(at));
        let mut item = by_col.remove(&c).unwrap_or_else(|| Item::text(String::new()));
        if let Some(m) = merge {
            if m.first == at {
                let across = cols.iter().skip(k).take_while(|x| **x <= m.last.col).count().max(1);
                item.colspan = u32::try_from(across).unwrap_or(1);
                item.rowspan = (m.last.row - m.first.row + 1).min(1000);
                skip = across - 1;
            } else if m.first.row < r {
                // Under the first row of a merge: the spanning cell covers it.
                skip = cols.iter().skip(k).take_while(|x| **x <= m.last.col).count().saturating_sub(1);
                continue;
            }
        }
        out.push(item);
    }
    out
}
