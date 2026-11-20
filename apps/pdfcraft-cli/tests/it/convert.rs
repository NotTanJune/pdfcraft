//! Documents typeset into PDF in process and tables exported back out, through `pdfcraft-cli run`.

use std::process::Command;

#[test]
fn a_script_typesets_a_csv_and_exports_its_table_as_xlsx_csv_and_latex() {
    let dir = std::env::temp_dir().join(format!("pdfcraft-cli-convert-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("data.csv"), "Name,Amount\nwidget,10\ngadget,25\n").unwrap();
    let steps = serde_json::json!([
        { "tool": "doc_create_from_document", "args": { "path": "data.csv", "paper": "a4" } },
        { "tool": "doc_save", "args": { "doc": 1, "path": "data.pdf" } },
        { "tool": "doc_export_office", "args": { "doc": 1, "path": "data.xlsx" } },
        { "tool": "doc_export_office", "args": { "doc": 1, "path": "back.csv" } },
        { "tool": "doc_export_office", "args": { "doc": 1, "path": "data.tex" } },
    ]);
    let script = dir.join("steps.json");
    std::fs::write(&script, steps.to_string()).unwrap();
    let o =
        Command::new(env!("CARGO_BIN_EXE_pdfcraft-cli")).current_dir(&dir).arg("run").arg("--script").arg(&script).arg("--report").output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let report: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(report["ok"], true, "{report}");
    let first = &report["steps"][0]["result"][0];
    assert_eq!((first["format"].as_str(), first["pages"].as_u64()), (Some("CSV"), Some(1)), "{first}");
    assert!(std::fs::read(dir.join("data.pdf")).unwrap().starts_with(b"%PDF-"));
    assert!(std::fs::read(dir.join("data.xlsx")).unwrap().starts_with(b"PK"));
    assert_eq!(std::fs::read_to_string(dir.join("back.csv")).unwrap(), "Name,Amount\r\nwidget,10\r\ngadget,25\r\n");
    assert!(std::fs::read_to_string(dir.join("data.tex")).unwrap().contains("\\begin{tabular}"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_tools_listing_names_the_conversion_tool() {
    let o = Command::new(env!("CARGO_BIN_EXE_pdfcraft-cli")).arg("tools").output().unwrap();
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("doc_create_from_document"));
}
