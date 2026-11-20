//! `ai_ask` and `link_preview` through the tool table, against a server on 127.0.0.1 (no real
//! network).

use pdfcraft_assist::mock::{MockServer, Reply};
use pdfcraft_assist::preview::FetchPolicy;
use pdfcraft_automation::{Automation, Content};
use serde_json::{Value, json};

fn call(a: &mut Automation, name: &str, args: Value) -> Vec<Content> {
    a.call(name, &args).unwrap_or_else(|e| panic!("{name} failed: {e}"))
}

fn tool(a: &mut Automation, name: &str, args: Value) -> Value {
    match call(a, name, args).remove(0) {
        Content::Json(value) => value,
        other => panic!("expected JSON: {other:?}"),
    }
}

/// Two 300 x 400 pages with text. Page 1 has a link to a web address (`url`) and a link to page 2
/// at (100, 300).
fn pdf(url: &str) -> Vec<u8> {
    let content = |text: &str| format!("BT /F1 18 Tf 40 340 Td ({text}) Tj ET");
    let (c1, c2) = (content("Force majeure clause"), content("Definitions of terms"));
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 300 400] /Resources << /Font << /F1 5 0 R >> >> >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /Contents 6 0 R /Annots [8 0 R 9 0 R] >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /Contents 7 0 R >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!("<< /Length {} >>\nstream\n{c1}\nendstream", c1.len()),
        format!("<< /Length {} >>\nstream\n{c2}\nendstream", c2.len()),
        format!("<< /Type /Annot /Subtype /Link /Rect [20 20 180 60] /Border [0 0 0] /A << /S /URI /URI ({url}) >> >>"),
        "<< /Type /Annot /Subtype /Link /Rect [20 80 180 120] /Border [0 0 0] /Dest [4 0 R /XYZ 20 380 0] >>".to_string(),
    ];
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
    out
}

fn open(a: &mut Automation, dir: &std::path::Path, url: &str) -> u64 {
    std::fs::write(dir.join("t.pdf"), pdf(url)).unwrap();
    tool(a, "doc_open", json!({ "path": "t.pdf" }))["doc"].as_u64().unwrap()
}

fn session(dir: &std::path::Path) -> Automation {
    Automation::new().with_root(dir).unwrap().with_link_fetch_policy(FetchPolicy { allow_private_hosts: true })
}

const ANSWER: &str =
    r#"{"model":"mock-model","choices":[{"message":{"role":"assistant","content":"It excuses delay [p. 1] and see [p. 2] (not [p. 9])."}}]}"#;

#[test]
fn ai_ask_sends_the_selection_and_page_text_and_reads_the_citations() {
    let server = MockServer::start(|_| Reply::json(ANSWER)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut a = session(dir.path());
    let doc = open(&mut a, dir.path(), "https://example.org/");
    let out = tool(
        &mut a,
        "ai_ask",
        json!({ "doc": doc, "page": 1, "text": "Force majeure", "question": "What does it mean?", "context": true,
                "base_url": format!("{}/v1", server.url()), "model": "mock-model", "api_key": "sk-mock-key" }),
    );
    assert_eq!(out["answer"], "It excuses delay [p. 1] and see [p. 2] (not [p. 9]).");
    assert_eq!(out["model"], "mock-model");
    assert_eq!(out["sent"], true);
    // Pages the document lacks are not citations.
    assert_eq!(out["citations"], json!([{ "text": "[p. 1]", "pages": [1] }, { "text": "[p. 2]", "pages": [2] }]));
    let seen = server.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/v1/chat/completions");
    assert_eq!(seen[0].header("authorization"), Some("Bearer sk-mock-key"));
    let body: Value = serde_json::from_slice(&seen[0].body).unwrap();
    let user = body["messages"][1]["content"].as_str().unwrap();
    assert!(user.contains("Selected text (page 1)") && user.contains("Force majeure"), "{user}");
    assert!(user.contains("Force majeure clause"), "the page's text went along with context: {user}");
    assert!(user.contains("What does it mean?"));
    assert!(!out.to_string().contains("sk-mock-key"), "the key is never echoed");
}

#[test]
fn ai_ask_sends_no_page_text_unless_asked() {
    let server = MockServer::start(|_| Reply::json(ANSWER)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut a = session(dir.path());
    let doc = open(&mut a, dir.path(), "https://example.org/");
    tool(&mut a, "ai_ask", json!({ "doc": doc, "page": 2, "text": "Definitions", "base_url": server.url(), "api_key": "sk-k" }));
    let body: Value = serde_json::from_slice(&server.requests()[0].body).unwrap();
    let user = body["messages"][1]["content"].as_str().unwrap();
    assert!(user.contains("page 2") && !user.contains("Text around it"), "{user}");
}

#[test]
fn ai_ask_dry_run_shows_the_request_and_contacts_nobody() {
    let server = MockServer::start(|_| Reply::json(ANSWER)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut a = session(dir.path());
    let doc = open(&mut a, dir.path(), "https://example.org/");
    let out = tool(&mut a, "ai_ask", json!({ "doc": doc, "page": 1, "text": "Force", "dry_run": true, "base_url": server.url(), "api_key": "sk-k" }));
    assert_eq!(out["sent"], false);
    assert_eq!(out["request"]["messages"][1]["role"], "user");
    assert!(server.requests().is_empty());
}

#[test]
fn another_endpoint_needs_its_own_key_so_a_stored_key_cannot_be_redirected() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = session(dir.path());
    let doc = open(&mut a, dir.path(), "https://example.org/");
    let error = a.call("ai_ask", &json!({ "doc": doc, "page": 1, "text": "x", "base_url": "https://attacker.example/v1" })).unwrap_err();
    assert!(error.to_string().contains("api_key"), "{error}");
}

#[test]
fn ai_ask_refuses_bad_arguments_and_cleartext_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = session(dir.path());
    let doc = open(&mut a, dir.path(), "https://example.org/");
    for args in [
        json!({ "doc": doc, "page": 3, "text": "x", "api_key": "k" }),
        json!({ "doc": doc, "page": 1, "text": "  ", "api_key": "k" }),
        json!({ "doc": doc, "page": 1, "text": "x", "base_url": "http://api.example.com/v1", "api_key": "k" }),
        json!({ "doc": doc, "page": 1, "text": "x", "api_key": "has space" }),
    ] {
        assert!(a.call("ai_ask", &args).is_err(), "{args}");
    }
}

#[test]
fn ai_ask_reports_what_the_endpoint_said_when_it_refuses() {
    let server = MockServer::start(|_| Reply::new(429, "application/json", r#"{"error":{"message":"You exceeded your quota."}}"#)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut a = session(dir.path());
    let doc = open(&mut a, dir.path(), "https://example.org/");
    let error =
        a.call("ai_ask", &json!({ "doc": doc, "page": 1, "text": "x", "base_url": server.url(), "api_key": "sk-k" })).unwrap_err().to_string();
    assert!(error.contains("429") && error.contains("quota"), "{error}");
}

#[test]
fn a_link_to_a_page_previews_the_destination_region_as_a_picture() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = session(dir.path());
    let doc = open(&mut a, dir.path(), "https://example.org/");
    let links = tool(&mut a, "link_list", json!({ "doc": doc }));
    let to_page = links["links"].as_array().unwrap().iter().find(|l| l["to_page"] == 2).expect("the page link");
    let out = call(&mut a, "link_preview", json!({ "doc": doc, "page": 1, "index": to_page["index"] }));
    let Content::Json(info) = &out[0] else { panic!("{out:?}") };
    assert_eq!((info["kind"].as_str(), info["page"].as_u64()), (Some("page"), Some(2)));
    assert!(matches!(&out[1], Content::Png { width, height, data } if *width > 100 && *height > 50 && data.starts_with(b"\x89PNG")), "{out:?}");
}

#[test]
fn a_web_link_previews_its_host_and_fetches_only_when_asked() {
    let server = MockServer::start(|_| {
        Reply::html(r#"<head><title>T</title><meta property="og:title" content="Card Title"><meta name="description" content="Card text"></head>"#)
    })
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut a = session(dir.path());
    let doc = open(&mut a, dir.path(), &format!("{}/article", server.url()));
    let links = tool(&mut a, "link_list", json!({ "doc": doc }));
    let web = links["links"].as_array().unwrap().iter().find(|l| l["url"].is_string()).expect("the web link");
    let index = web["index"].clone();

    let plain = tool(&mut a, "link_preview", json!({ "doc": doc, "page": 1, "index": index }));
    assert_eq!(
        (plain["kind"].as_str(), plain["host"].as_str(), plain["fetched"].as_bool()),
        (Some("web"), Some("127.0.0.1"), Some(false)),
        "{plain}"
    );
    assert!(server.requests().is_empty(), "nothing is fetched unless fetch is true");

    let card = tool(&mut a, "link_preview", json!({ "doc": doc, "page": 1, "index": index, "fetch": true }));
    assert_eq!((card["title"].as_str(), card["description"].as_str()), (Some("Card Title"), Some("Card text")), "{card}");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn the_default_policy_does_not_fetch_from_this_computer() {
    let server = MockServer::start(|_| Reply::html("<title>x</title>")).unwrap();
    let mut a = Automation::new();
    let out = tool(&mut a, "link_preview", json!({ "url": format!("{}/", server.url()), "fetch": true }));
    assert!(out["fetch_error"].as_str().is_some_and(|e| e.contains("private network")), "{out}");
    assert!(server.requests().is_empty());
}

#[test]
fn other_addresses_have_no_preview() {
    let mut a = Automation::new();
    for url in ["mailto:a@example.org", "file:///etc/passwd", "javascript:alert(1)"] {
        assert_eq!(tool(&mut a, "link_preview", json!({ "url": url }))["kind"], "other", "{url}");
    }
}
