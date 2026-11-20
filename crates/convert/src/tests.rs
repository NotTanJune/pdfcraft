use std::sync::Arc;

use pdfcraft_cos::{Dict, Document};
use pdfcraft_render::{PageRenderer, RenderConfig, text::TextFilter};

use super::*;

/// The text each page of `pdf` shows, read back through the renderer as a reader would select it.
fn page_texts(pdf: &[u8]) -> Vec<String> {
    let mut r = PageRenderer::new(Arc::new(pdf.to_vec()), RenderConfig::default());
    (0..r.page_count())
        .map(|p| {
            let t = r.filtered_text(p, TextFilter::default()).unwrap();
            t.text_of(0..t.glyphs.len())
        })
        .collect()
}

fn page_dicts(doc: &Document) -> Vec<Dict> {
    let tree = doc.get(doc.root().unwrap()).as_dict().unwrap().reference(b"Pages").unwrap();
    let kids = doc.get(tree).as_dict().unwrap().get(b"Kids").unwrap().as_array().unwrap().clone();
    kids.iter().map(|k| doc.resolve(k).as_dict().cloned().unwrap()).collect()
}

fn open(pdf: &[u8]) -> Document {
    Document::open(Arc::new(pdf.to_vec())).unwrap()
}

fn convert_ok(name: &str, bytes: &[u8]) -> Converted {
    convert(name, bytes, &Settings::default()).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn media_box(d: &Dict) -> Vec<f64> {
    d.get(b"MediaBox").unwrap().as_array().unwrap().iter().map(|o| o.as_f64().unwrap()).collect()
}

/// Titles of the document's bookmarks, in the order they were written.
fn bookmarks(doc: &Document) -> Vec<String> {
    let mut found: Vec<(u32, String)> = doc
        .scan_objects()
        .filter_map(|(r, o)| {
            let d = o.as_dict()?;
            if d.get(b"Parent").is_none() || d.get(b"Dest").is_none() {
                return None;
            }
            Some((r.num, d.get(b"Title")?.as_string()?.to_text()))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, t)| t).collect()
}

fn link_uris(doc: &Document) -> Vec<String> {
    doc.scan_objects()
        .filter_map(|(_, o)| {
            let d = o.as_dict()?;
            (d.name(b"Subtype") == Some(b"Link")).then_some(())?;
            let action = doc.resolve(d.get(b"A")?).as_dict().cloned()?;
            Some(action.get(b"URI")?.as_string()?.to_text())
        })
        .collect()
}

const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

#[test]
fn markdown_becomes_selectable_text_with_bookmarks() {
    let md = "# Quarterly report\n\nSome **bold** and *italic* text with `code`.\n\n## Findings\n\n- first point\n- second point\n\n1. one\n2. two\n\n> quoted words\n\n```\nlet x = 1;\n```\n";
    let out = convert_ok("report.md", md.as_bytes());
    assert_eq!(out.pages, 1);
    let text = page_texts(&out.pdf).join("\n");
    for needle in
        ["Quarterly report", "Some bold and italic text with code.", "Findings", "first point", "second point", "quoted words", "let x = 1;"]
    {
        assert!(text.contains(needle), "{needle:?} missing from {text:?}");
    }
    assert!(text.contains("1.") && text.contains("2."), "numbers of the ordered list: {text:?}");
    let doc = open(&out.pdf);
    assert_eq!(bookmarks(&doc), ["Quarterly report", "Findings"]);
    assert_eq!(media_box(&page_dicts(&doc)[0]), [0.0, 0.0, 612.0, 792.0]);
}

#[test]
fn a_long_document_paginates_and_keeps_text_inside_the_margins() {
    let body: String = (0..160).map(|i| format!("Paragraph {i}. {}\n\n", "The quick brown fox jumps over the lazy dog. ".repeat(6))).collect();
    let out = convert_ok("long.md", format!("# Title\n\n{body}").as_bytes());
    assert!(out.pages >= 5, "{} pages", out.pages);
    let mut r = PageRenderer::new(Arc::new(out.pdf.clone()), RenderConfig::default());
    for p in 0..r.page_count() {
        let t = r.filtered_text(p, TextFilter::default()).unwrap();
        for g in &t.glyphs {
            assert!(g.rect[0] >= 71.0 && g.rect[2] <= 541.0, "page {p}: {g:?} outside the side margins");
            assert!(g.rect[1] >= 71.0 && g.rect[3] <= 721.0, "page {p}: {g:?} outside the top and bottom margins");
        }
    }
    let all = page_texts(&out.pdf).join(" ");
    assert!(all.contains("Paragraph 0.") && all.contains("Paragraph 159."), "every paragraph is on a page");
}

#[test]
fn a_heading_never_ends_a_page() {
    // Lines of a filler paragraph, then a heading that would land on the last line of the page.
    let mut md = String::new();
    for i in 0..40 {
        md.push_str(&format!("# Heading {i}\n\nOne line of text for section {i}.\n\n"));
    }
    let out = convert_ok("headings.md", md.as_bytes());
    for (p, text) in page_texts(&out.pdf).iter().enumerate() {
        let last = text.lines().last().unwrap_or_default();
        assert!(!last.starts_with("Heading"), "page {p} ends with a heading: {last:?}");
    }
}

#[test]
fn tables_have_their_header_row_on_every_page() {
    let mut csv = String::from("Name,Amount,Note\n");
    for i in 0..300 {
        csv.push_str(&format!("item {i},{},row {i}\n", i * 7));
    }
    let out = convert_ok("data.csv", csv.as_bytes());
    assert!(out.pages >= 4, "{} pages", out.pages);
    let texts = page_texts(&out.pdf);
    for (p, t) in texts.iter().enumerate() {
        assert!(t.contains("Name") && t.contains("Amount") && t.contains("Note"), "page {p} has no header row: {t:?}");
    }
    let all = texts.join("\n");
    assert!(all.contains("item 0") && all.contains("item 299") && all.contains("2093"), "all rows are there");
    // Page numbers at the foot.
    assert!(texts[0].trim_end().ends_with('1') && texts[1].trim_end().ends_with('2'), "{:?}", texts[1]);
}

#[test]
fn csv_with_quotes_semicolons_and_ragged_rows() {
    let out = convert_ok("odd.csv", b"a;b;c\n\"x;y\";2;\"say \"\"hi\"\"\"\np;q;r\n");
    let t = page_texts(&out.pdf).join("\n");
    assert!(t.contains("x;y") && t.contains("say \"hi\"") && t.contains("p") && !t.contains("a;b;c"), "{t:?}");
    // Short rows are padded to the widest.
    let ragged = convert_ok("ragged.csv", b"h1,h2,h3\nonly one\nx,y\n");
    let t = page_texts(&ragged.pdf).join("\n");
    assert!(t.contains("only one") && t.contains("h3"), "{t:?}");
    let empty = convert_ok("empty.csv", b"");
    assert!(page_texts(&empty.pdf).join("").contains("no data"));
}

#[test]
fn a_wide_csv_turns_the_page_on_its_side() {
    let header: Vec<String> = (0..14).map(|i| format!("Column heading {i}")).collect();
    let row: Vec<String> = (0..14).map(|i| format!("value {i}")).collect();
    let csv = format!("{}\n{}\n", header.join(","), row.join(","));
    let out = convert_ok("wide.csv", csv.as_bytes());
    let doc = open(&out.pdf);
    assert_eq!(media_box(&page_dicts(&doc)[0]), [0.0, 0.0, 792.0, 612.0]);
    let text = page_texts(&out.pdf)[0].split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(text.contains("13") && text.contains("value 13"), "{text:?}");
}

#[test]
fn a_workbook_makes_a_heading_and_a_table_for_each_sheet() {
    // Shared strings: 0 "Region", 1 "Sales", 2 "North", 3 "South", 4 "Costs sheet note".
    let rows_a = r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row><row r="2"><c r="A2" t="s"><v>2</v></c><c r="B2"><v>1234.5</v></c></row><row r="3"><c r="A3" t="s"><v>3</v></c><c r="B3"><v>99</v></c></row>"#;
    let rows_b = r#"<row r="1"><c r="A1" t="s"><v>4</v></c><c r="B1" t="b"><v>1</v></c></row>"#;
    let xlsx =
        craft_xlsx::testing::workbook(&[("Sales", rows_a), ("Costs", rows_b)], &["Region", "Sales", "North", "South", "Costs sheet note"]).unwrap();
    let out = convert_ok("book.xlsx", &xlsx);
    let t = page_texts(&out.pdf).join("\n");
    for needle in ["Sales", "Region", "North", "1234.5", "99", "Costs", "Costs sheet note", "TRUE"] {
        assert!(t.contains(needle), "{needle:?} missing from {t:?}");
    }
    assert_eq!(bookmarks(&open(&out.pdf)), ["Sales", "Costs"], "sheet names are bookmarks");
}

#[test]
fn a_workbook_shows_dates_and_percentages_as_their_cells_do() {
    use craft_xlsx::{Cell, CellRef, CellValue, Document as Book, Row, SheetEntry, SheetKind, Styles, Visibility, Workbook, Worksheet, Xf};
    let xf = |id| Xf { num_fmt_id: id, ..Xf::default() };
    let styles = Styles { cell_xfs: vec![xf(0), xf(14), xf(9), xf(2)], ..Styles::default() };
    let cell = |at: &str, style, n: f64| Cell { at: CellRef::parse(at).unwrap(), style, value: CellValue::Number(n), ..Cell::default() };
    let book = Book {
        workbook: Workbook {
            sheets: vec![SheetEntry {
                name: "S".into(),
                sheet_id: 1,
                visibility: Visibility::Visible,
                rel_id: String::new(),
                kind: SheetKind::Worksheet,
                part: None,
            }],
            ..Workbook::default()
        },
        styles: Some(styles),
        worksheets: vec![Some(Worksheet {
            rows: vec![Row { index: 0, cells: vec![cell("A1", 1, 45292.0), cell("B1", 2, 0.256), cell("C1", 3, 7.81234)], ..Row::default() }],
            ..Worksheet::default()
        })],
        ..Book::default()
    };
    let out = convert_ok("fmt.xlsx", &book.write().unwrap());
    let t = page_texts(&out.pdf).join(" ");
    assert!(t.contains("2024-01-01") && t.contains("26%") && t.contains("7.81"), "{t:?}");
}

#[test]
fn word_documents_keep_their_text_and_page() {
    let mut doc = craft_wordmodel::Document::from_text("First paragraph of the memo\nSecond paragraph follows");
    doc.last_section.page_w = 595.0;
    doc.last_section.page_h = 842.0;
    let bytes = craft_docx::write(&doc).unwrap();
    let out = convert_ok("memo.docx", &bytes);
    let t = page_texts(&out.pdf).join("\n");
    assert!(t.contains("First paragraph of the memo") && t.contains("Second paragraph follows"), "{t:?}");
    let pages = page_dicts(&open(&out.pdf));
    assert_eq!(media_box(&pages[0]), [0.0, 0.0, 595.0, 842.0], "the document's own page");
    // A paper setting overrides it.
    let letter = convert("memo.docx", &bytes, &Settings { paper: Paper::Letter, ..Settings::default() }).unwrap();
    assert_eq!(media_box(&page_dicts(&open(&letter.pdf))[0]), [0.0, 0.0, 612.0, 792.0]);
}

#[test]
fn latex_sections_lists_and_math_are_set() {
    let tex = r"\documentclass{article}
\title{On Squares}
\begin{document}
\section{Introduction}
Some \textbf{bold} words and inline math $x^2 + y^2$.
\begin{itemize}
\item alpha
\item beta
\end{itemize}
\[ E = mc^2 \]
\begin{tabular}{ll} Name & Value \\ one & 1 \\ \end{tabular}
\end{document}";
    let out = convert_ok("paper.tex", tex.as_bytes());
    let t = page_texts(&out.pdf).join("\n");
    for needle in ["Introduction", "Some bold words", "alpha", "beta", "Name", "Value"] {
        assert!(t.contains(needle), "{needle:?} missing from {t:?}");
    }
    assert!(t.contains("x") && t.contains("mc"), "the equations are text: {t:?}");
}

#[test]
fn characters_the_standard_fonts_lack_use_the_bundled_face() {
    let out = convert_ok("greek.md", "Привет, мир: Ελληνικά → ∑ café\n".as_bytes());
    let t = page_texts(&out.pdf).join(" ");
    assert!(t.contains("Привет, мир") && t.contains("Ελληνικά") && t.contains("café"), "{t:?}");
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    // What no font draws is said, not silently dropped.
    let odd = convert_ok("odd.md", "hello \u{10FFFD} there\n".as_bytes());
    assert!(odd.warnings.iter().any(|w| w.contains("U+10FFFD")), "{:?}", odd.warnings);
}

#[test]
fn web_links_become_link_annotations_and_script_links_do_not() {
    let md = "[site](https://example.com/a) and [mail](mailto:a@b.example) and [bad](javascript:alert(1)) and [file](file:///etc/passwd)\n";
    let out = convert_ok("links.md", md.as_bytes());
    let mut uris = link_uris(&open(&out.pdf));
    uris.sort();
    assert_eq!(uris, ["https://example.com/a", "mailto:a@b.example"]);
    assert!(page_texts(&out.pdf).join(" ").contains("bad"), "the text of a refused link stays");
}

#[test]
fn pictures_are_embedded() {
    let md = format!("Before\n\n![a dot](data:image/png;base64,{PNG_1X1})\n\nAfter\n");
    let out = convert_ok("pic.md", md.as_bytes());
    let doc = open(&out.pdf);
    let page = &page_dicts(&doc)[0];
    let resources = doc.resolve(page.get(b"Resources").unwrap()).as_dict().cloned().unwrap();
    let xobjects = doc.resolve(resources.get(b"XObject").expect("the page names its picture")).as_dict().cloned().unwrap();
    assert_eq!(xobjects.len(), 1);
    let t = page_texts(&out.pdf).join(" ");
    assert!(t.contains("Before") && t.contains("After"));
}

#[test]
fn paper_and_margin_settings_apply() {
    let out = convert("a.md", b"hello", &Settings { paper: Paper::A4, margin: Some(36.0), ..Settings::default() }).unwrap();
    let pages = page_dicts(&open(&out.pdf));
    let mb = media_box(&pages[0]);
    assert!((mb[2] - 595.276).abs() < 0.01 && (mb[3] - 841.89).abs() < 0.01, "{mb:?}");
    let mut r = PageRenderer::new(Arc::new(out.pdf), RenderConfig::default());
    let t = r.filtered_text(0, TextFilter::default()).unwrap();
    assert!(t.glyphs.iter().all(|g| g.rect[0] >= 35.0 && g.rect[1] >= 35.0));
    assert!(t.glyphs.iter().any(|g| g.rect[0] < 40.0), "text starts at the 36 pt margin");
}

#[test]
fn files_that_are_not_what_their_name_says_are_refused_without_panicking() {
    for (name, bytes) in [
        ("a.docx", &b"not a zip"[..]),
        ("a.docx", &[0x50, 0x4B, 0x03, 0x04, 0, 0, 0, 0][..]),
        ("a.xlsx", &b"PK\x03\x04garbage"[..]),
        ("a.xlsx", &b""[..]),
        ("a.txt", &b"plain"[..]),
    ] {
        let r = convert(name, bytes, &Settings::default());
        assert!(r.is_err(), "{name}: {r:?}");
    }
    // Text formats take any bytes.
    for name in ["a.md", "a.tex", "a.csv"] {
        let out = convert_ok(name, &[0xFF, 0xFE, 0x00, 0x80, b'\n', 0x01, 0x02]);
        assert!(out.pages >= 1);
    }
}

#[test]
fn hostile_nesting_and_size_stay_bounded() {
    // Deeply nested quotes and lists, and a table with a very wide row.
    let deep = format!("{}text\n", "> ".repeat(500));
    assert!(convert("deep.md", deep.as_bytes(), &Settings::default()).is_ok());
    let lists: String = (0..300).map(|i| format!("{}- item\n", "  ".repeat(i))).collect();
    assert!(convert("lists.md", lists.as_bytes(), &Settings::default()).is_ok());
    let wide = format!("{}\n", (0..5000).map(|i| i.to_string()).collect::<Vec<_>>().join(","));
    let out = convert_ok("wide.csv", wide.as_bytes());
    assert!(out.warnings.iter().any(|w| w.contains("columns")), "{:?}", out.warnings);
    let tex = format!("{}x{}", r"\textbf{".repeat(5000), "}".repeat(5000));
    assert!(convert("deep.tex", tex.as_bytes(), &Settings::default()).is_ok());
}

#[test]
fn a_word_longer_than_a_line_breaks_between_characters() {
    let word = "x".repeat(400);
    let out = convert_ok("long.md", format!("{word}\n").as_bytes());
    let t = page_texts(&out.pdf).join("");
    assert_eq!(t.replace('\n', "").matches('x').count(), 400, "no character is lost");
}

#[test]
fn formats_are_recognised_by_extension() {
    assert_eq!(Format::of_name("A.DOCX"), Some(Format::Docx));
    assert_eq!(Format::of_name("dir/notes.markdown"), Some(Format::Markdown));
    assert_eq!(Format::of_name("x.tex"), Some(Format::Latex));
    assert_eq!(Format::of_name("x.tsv"), Some(Format::Csv));
    assert_eq!(Format::of_name("x.xlsm"), Some(Format::Xlsx));
    assert_eq!(Format::of_name("x.doc"), None);
    assert_eq!(Format::of_name("x.pptx"), None);
    assert_eq!(Format::of_name("noextension"), None);
    for ext in EXTENSIONS {
        assert!(Format::of_name(&format!("f.{ext}")).is_some(), "{ext}");
    }
}

#[test]
fn a_page_with_no_room_for_text_is_refused() {
    let flow = craft_markdown::parse("hi");
    let bad = Options { page: PageSetup { width: 100.0, height: 100.0, margins: [60.0; 4] }, ..Options::default() };
    assert!(matches!(typeset(&flow, &bad), Err(ConvertError::Invalid(_))));
    let nan = Options { page: PageSetup { width: f64::NAN, ..PageSetup::LETTER }, ..Options::default() };
    assert!(matches!(typeset(&flow, &nan), Err(ConvertError::Invalid(_))));
}

#[test]
fn a_line_taller_than_the_page_does_not_loop_forever() {
    // Room for one 48 pt line of text and no more: the pages must still end.
    let flow = craft_markdown::parse(&"word ".repeat(400));
    let page = PageSetup { width: 612.0, height: 200.0, margins: [60.0, 72.0, 60.0, 72.0] };
    let out = typeset(&flow, &Options { page, font_size: 48.0, ..Options::default() }).unwrap();
    assert!(out.pages >= 2 && out.pages < 10_000, "{} pages", out.pages);
    let tiny = PageSetup { width: 612.0, height: 100.0, margins: [32.0, 72.0, 32.0, 72.0] };
    let out = typeset(&flow, &Options { page: tiny, font_size: 48.0, ..Options::default() }).unwrap();
    assert!(out.pages < 10_000, "{} pages", out.pages);
}

#[test]
fn a_document_too_long_for_a_pdf_is_cut_and_says_so() {
    let flow = craft_markdown::parse(&"line\n\n".repeat(40_000));
    let page = PageSetup { width: 612.0, height: 120.0, margins: [36.0, 72.0, 36.0, 72.0] };
    let out = typeset(&flow, &Options { page, font_size: 20.0, ..Options::default() }).unwrap();
    assert_eq!(out.pages, 10_000);
    assert!(out.warnings.iter().any(|w| w.contains("10,000")), "{:?}", out.warnings);
}

#[test]
fn formula_cells_show_their_saved_result_and_a_missing_one_is_said() {
    let rows = r#"<row r="1"><c r="A1"><f>6*7</f><v>42</v></c><c r="B1"><f>1+1</f></c></row>"#;
    let xlsx = craft_xlsx::testing::workbook(&[("S", rows)], &[]).unwrap();
    let out = convert_ok("f.xlsx", &xlsx);
    assert!(page_texts(&out.pdf).join(" ").contains("42"));
    assert!(out.warnings.iter().any(|w| w.contains("1 formula cells have no saved result")), "{:?}", out.warnings);
}
