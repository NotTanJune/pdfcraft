//! Documents typeset into PDF in process (`doc_create_from_document`, and the office path of
//! `doc_create_multiple`), and tables exported from a PDF as Excel, CSV and LaTeX. The files are
//! written here from synthetic text; nothing is committed as a fixture, and no LibreOffice runs.

use std::path::{Path, PathBuf};

use pdfcraft_automation::{Automation, Content, ToolError};
use serde_json::{Value, json};

fn workdir(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pdfcraft-convert-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn auto(dir: &Path) -> Automation {
    Automation::new().with_root(dir).unwrap().with_clock(|| 1_700_000_000)
}

fn ok(a: &mut Automation, tool: &str, args: Value) -> Value {
    match a.call(tool, &args) {
        Ok(mut c) => match c.remove(0) {
            Content::Json(v) => v,
            other => panic!("{tool}: expected JSON, got {other:?}"),
        },
        Err(e) => panic!("{tool} {args}: {e}"),
    }
}

/// Each page's text, with runs of whitespace made single spaces.
fn page_text(a: &mut Automation, doc: u64) -> Vec<String> {
    ok(a, "text_extract", json!({ "doc": doc }))["pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["text"].as_str().unwrap().split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

fn docx(text: &str) -> Vec<u8> {
    craft_docx::write(&craft_wordmodel::Document::from_text(text)).unwrap()
}

fn workbook() -> Vec<u8> {
    let sales = r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row><row r="2"><c r="A2" t="s"><v>2</v></c><c r="B2"><v>1234.5</v></c></row>"#;
    let costs = r#"<row r="1"><c r="A1" t="s"><v>3</v></c><c r="B1"><v>42</v></c></row>"#;
    craft_xlsx::testing::workbook(&[("Sales", sales), ("Costs", costs)], &["Region", "Total", "North", "Rent"]).unwrap()
}

fn csv_of(rows: usize) -> String {
    let mut csv = String::from("Name,Amount,Note\n");
    for i in 0..rows {
        csv.push_str(&format!("item {i},{},row {i}\n", i * 7));
    }
    csv
}

#[test]
fn word_markdown_latex_csv_and_excel_files_are_typeset_through_the_tool() {
    let dir = workdir("typeset");
    std::fs::write(dir.join("memo.docx"), docx("Memo to the board\nBudget approved")).unwrap();
    std::fs::write(dir.join("notes.md"), "# Release notes\n\nFixed **three** bugs.\n\n- speed\n- size\n").unwrap();
    std::fs::write(dir.join("paper.tex"), "\\section{Proof}\nLet $x^2$ be a square.\n\\begin{itemize}\\item base\\end{itemize}").unwrap();
    std::fs::write(dir.join("data.csv"), csv_of(3)).unwrap();
    std::fs::write(dir.join("book.xlsx"), workbook()).unwrap();
    let mut a = auto(&dir);
    for (file, format, needles) in [
        ("memo.docx", "Word", &["Memo to the board", "Budget approved"][..]),
        ("notes.md", "Markdown", &["Release notes", "Fixed three bugs.", "speed", "size"]),
        ("paper.tex", "LaTeX", &["Proof", "Let", "base"]),
        ("data.csv", "CSV", &["Name", "Amount", "item 2", "14"]),
        ("book.xlsx", "Excel", &["Sales", "Region", "North", "1234.5", "Costs", "Rent", "42"]),
    ] {
        let r = ok(&mut a, "doc_create_from_document", json!({ "path": file }));
        assert_eq!((r["format"].as_str(), r["dirty"].as_bool()), (Some(format), Some(true)), "{file}: {r}");
        assert!(r["pages"].as_u64().unwrap() >= 1 && r["warnings"].as_array().unwrap().is_empty(), "{file}: {r}");
        let doc = r["doc"].as_u64().unwrap();
        let text = page_text(&mut a, doc).join(" ");
        for needle in needles {
            assert!(text.contains(needle), "{file}: {needle:?} missing from {text:?}");
        }
        // The result saves and reopens as an ordinary PDF.
        let out = format!("{file}.pdf");
        ok(&mut a, "doc_save", json!({ "doc": doc, "path": out }));
        let again = ok(&mut a, "doc_open", json!({ "path": out }))["doc"].as_u64().unwrap();
        assert_eq!(page_text(&mut a, again).join(" "), text, "{file}");
    }
}

#[test]
fn paper_margin_and_font_size_shape_the_pages() {
    let dir = workdir("paper");
    std::fs::write(dir.join("a.md"), "# Title\n\nBody\n").unwrap();
    let mut a = auto(&dir);
    let size = |a: &mut Automation, args: Value| -> (f64, f64) {
        let d = ok(a, "doc_create_from_document", args)["doc"].as_u64().unwrap();
        let info = ok(a, "doc_info", json!({ "doc": d }));
        let p = &info["pages"][0];
        (p["width"].as_f64().unwrap(), p["height"].as_f64().unwrap())
    };
    assert_eq!(size(&mut a, json!({ "path": "a.md" })), (612.0, 792.0), "US Letter by default");
    let (w, h) = size(&mut a, json!({ "path": "a.md", "paper": "a4", "margin": 36, "font_size": 14 }));
    assert!((w - 595.276).abs() < 0.01 && (h - 841.89).abs() < 0.01, "{w} x {h}");
}

#[test]
fn the_tool_refuses_what_it_cannot_convert_and_says_why() {
    let dir = workdir("refusals");
    std::fs::write(dir.join("old.doc"), b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1").unwrap();
    std::fs::write(dir.join("deck.pptx"), b"PK\x03\x04").unwrap();
    std::fs::write(dir.join("broken.docx"), b"this is not a zip package").unwrap();
    std::fs::write(dir.join("broken.xlsx"), b"PK\x03\x04 truncated").unwrap();
    std::fs::write(dir.join("a.md"), "hi").unwrap();
    let mut a = auto(&dir);
    for file in ["old.doc", "deck.pptx"] {
        match a.call("doc_create_from_document", &json!({ "path": file })) {
            Err(ToolError::InvalidArgs(m)) => assert!(m.contains("LibreOffice") && m.contains(file), "{m}"),
            other => panic!("{file}: {other:?}"),
        }
    }
    for file in ["broken.docx", "broken.xlsx"] {
        match a.call("doc_create_from_document", &json!({ "path": file })) {
            Err(ToolError::Failed(m)) => assert!(m.contains(file), "{m}"),
            other => panic!("{file}: {other:?}"),
        }
    }
    for bad in [
        json!({ "path": "a.md", "paper": "legal" }),
        json!({ "path": "a.md", "margin": -1 }),
        json!({ "path": "a.md", "margin": 500 }),
        json!({ "path": "a.md", "font_size": 2 }),
        json!({ "path": "a.md", "font_size": 99 }),
        json!({ "path": "a.md", "unknown": true }),
    ] {
        assert!(matches!(a.call("doc_create_from_document", &bad), Err(ToolError::InvalidArgs(_))), "{bad}");
    }
    assert!(a.call("doc_create_from_document", &json!({ "path": "missing.md" })).is_err());
    // Nothing was opened by the refusals.
    assert_eq!(ok(&mut a, "doc_list", json!({}))["documents"].as_array().unwrap().len(), 0);
}

#[test]
fn a_csv_becomes_a_table_that_exports_back_to_excel_csv_and_latex() {
    let dir = workdir("round-trip");
    std::fs::write(dir.join("data.csv"), csv_of(150)).unwrap();
    let mut a = auto(&dir);
    let created = ok(&mut a, "doc_create_from_document", json!({ "path": "data.csv" }));
    let pages = created["pages"].as_u64().unwrap();
    assert!(pages >= 3, "{pages} pages");
    let doc = created["doc"].as_u64().unwrap();
    ok(&mut a, "doc_save", json!({ "doc": doc, "path": "data.pdf" }));
    let doc = ok(&mut a, "doc_open", json!({ "path": "data.pdf" }))["doc"].as_u64().unwrap();

    // Excel: a sheet per table (one per page here), values typed.
    let r = ok(&mut a, "doc_export_office", json!({ "doc": doc, "path": "out.xlsx" }));
    assert_eq!(r["format"], "xlsx", "{r}");
    let bytes = std::fs::read(dir.join("out.xlsx")).unwrap();
    let book = craft_xlsx::Document::read(&bytes, &craft_xlsx::Limits::default()).unwrap();
    assert_eq!(book.workbook.sheets.len() as u64, pages, "one sheet per page's table");
    assert_eq!(book.workbook.sheets[0].name, "Page 1 Table 1");
    let first = book.worksheets[0].as_ref().unwrap();
    let text = |c: &craft_xlsx::Cell| match c.value.resolve(&book.shared_strings, false) {
        craft_xlsx::Value::Text(t) => t.to_string(),
        craft_xlsx::Value::Number(n) => format!("#{n}"),
        other => format!("{other:?}"),
    };
    let row0: Vec<String> = first.rows[0].cells.iter().map(text).collect();
    assert_eq!(row0, ["Name", "Amount", "Note"]);
    let row2: Vec<String> = first.rows[2].cells.iter().map(text).collect();
    assert_eq!(row2, ["item 1", "#7", "row 1"], "numbers are numbers");

    // CSV: the same tables one after another; `table` picks one.
    ok(&mut a, "doc_export_office", json!({ "doc": doc, "path": "out.csv" }));
    let csv = std::fs::read_to_string(dir.join("out.csv")).unwrap();
    assert!(csv.starts_with("Name,Amount,Note\r\nitem 0,0,row 0\r\n"), "{}", &csv[..80.min(csv.len())]);
    assert!(csv.contains("item 149,1043,row 149\r\n"), "the last row is there");
    ok(&mut a, "doc_export_office", json!({ "doc": doc, "path": "first.csv", "table": 1 }));
    let first_csv = std::fs::read_to_string(dir.join("first.csv")).unwrap();
    assert!(first_csv.len() < csv.len() && first_csv.starts_with("Name,Amount,Note"), "only the first table");
    assert!(!first_csv.contains("item 149"));
    for bad in [json!({ "table": 0 }), json!({ "table": -3 })] {
        let mut args = json!({ "doc": doc, "path": "bad.csv" });
        args.as_object_mut().unwrap().extend(bad.as_object().unwrap().clone());
        assert!(matches!(a.call("doc_export_office", &args), Err(ToolError::InvalidArgs(_))), "{args}");
    }
    assert!(matches!(a.call("doc_export_office", &json!({ "doc": doc, "path": "bad.xlsx", "table": 1 })), Err(ToolError::InvalidArgs(_))));

    // LaTeX: a standalone article with a tabular.
    let r = ok(&mut a, "doc_export_office", json!({ "doc": doc, "path": "out.tex" }));
    assert_eq!(r["format"], "tex");
    let tex = std::fs::read_to_string(dir.join("out.tex")).unwrap();
    assert!(tex.contains("\\documentclass") && tex.contains("\\begin{tabular}") && tex.contains("item 0"), "{tex}");
}

#[test]
fn a_page_without_a_table_exports_its_text_lines_as_rows() {
    let dir = workdir("text-sheet");
    std::fs::write(dir.join("a.md"), "First line of prose\n\nSecond line of prose\n").unwrap();
    let mut a = auto(&dir);
    let doc = ok(&mut a, "doc_create_from_document", json!({ "path": "a.md" }))["doc"].as_u64().unwrap();
    ok(&mut a, "doc_export_office", json!({ "doc": doc, "path": "t.csv" }));
    let csv = std::fs::read_to_string(dir.join("t.csv")).unwrap();
    // A paragraph is a row (the export reads close lines as one paragraph, so the two may share one).
    assert!(csv.ends_with("\r\n") && csv.contains("First line of prose") && csv.contains("Second line of prose"), "{csv:?}");
    ok(&mut a, "doc_export_office", json!({ "doc": doc, "path": "t.xlsx" }));
    let book = craft_xlsx::Document::read(&std::fs::read(dir.join("t.xlsx")).unwrap(), &craft_xlsx::Limits::default()).unwrap();
    assert_eq!(book.workbook.sheets[0].name, "Page 1");
}

#[test]
fn text_that_would_run_as_a_formula_is_exported_as_text() {
    let dir = workdir("formula");
    std::fs::write(dir.join("a.csv"), "Name,Cmd\nx,=HYPERLINK(\"http://example.com\")\ny,+1+1\nz,-5\n").unwrap();
    let mut a = auto(&dir);
    let doc = ok(&mut a, "doc_create_from_document", json!({ "path": "a.csv" }))["doc"].as_u64().unwrap();
    ok(&mut a, "doc_export_office", json!({ "doc": doc, "path": "out.csv" }));
    let csv = std::fs::read_to_string(dir.join("out.csv")).unwrap();
    assert!(csv.contains("'=HYPERLINK") && csv.contains("'+1+1") && csv.contains(",-5\r\n"), "{csv}");
}

#[test]
fn office_files_in_a_combine_are_typeset_in_process() {
    let dir = workdir("combine");
    std::fs::write(dir.join("a.md"), "# From Markdown\n").unwrap();
    std::fs::write(dir.join("b.csv"), csv_of(2)).unwrap();
    std::fs::write(dir.join("c.docx"), docx("From Word")).unwrap();
    std::fs::write(dir.join("d.xlsx"), workbook()).unwrap();
    let mut a = auto(&dir);
    let r = ok(&mut a, "doc_create_multiple", json!({ "paths": ["a.md", "b.csv", "c.docx", "d.xlsx"], "open": true }));
    let doc = r["document"]["doc"].as_u64().unwrap_or_else(|| panic!("{r}"));
    let text = page_text(&mut a, doc).join(" ");
    for needle in ["From Markdown", "item 1", "From Word", "Region"] {
        assert!(text.contains(needle), "{needle:?} missing from {text:?}");
    }
}

#[test]
fn the_conversion_tools_are_listed_and_the_table_is_well_formed() {
    let tools = pdfcraft_automation::tools();
    let t = tools.iter().find(|t| t.name == "doc_create_from_document").expect("the import tool exists");
    assert!(!t.read_only);
    assert!(t.description.contains("in this process"), "{}", t.description);
    let export = tools.iter().find(|t| t.name == "doc_export_office").unwrap();
    for ext in [".xlsx", ".csv", ".tex"] {
        assert!(export.description.contains(ext), "{ext}");
    }
}
