//! `cloud_*` through the tool table, against a server on 127.0.0.1 that speaks the OAuth, Google
//! Drive and Microsoft Graph endpoints (no real network).

use pdfcraft_automation::{Automation, CloudSetup, Content, ToolError};
use pdfcraft_clouddrive::ProviderSettings;
use pdfcraft_clouddrive::mock::FakeCloud;
use serde_json::{Value, json};
use std::sync::Arc;

fn pdf(text: &str) -> Vec<u8> {
    let content = format!("BT /F1 18 Tf 40 340 Td ({text}) Tj ET");
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 400] /Resources << /Font << /F1 4 0 R >> >> >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /Contents 5 0 R >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
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

fn call(a: &mut Automation, name: &str, args: Value) -> Result<Value, ToolError> {
    match a.call(name, &args)?.remove(0) {
        Content::Json(v) => Ok(v),
        other => panic!("expected JSON: {other:?}"),
    }
}

fn tool(a: &mut Automation, name: &str, args: Value) -> Value {
    call(a, name, args).unwrap_or_else(|e| panic!("{name} failed: {e}"))
}

fn failure(a: &mut Automation, name: &str, args: Value) -> String {
    match call(a, name, args) {
        Err(e) => e.to_string(),
        Ok(v) => panic!("{name} should have failed, gave {v}"),
    }
}

/// A session confined to `dir`, with both providers pointed at `cloud`.
fn session(cloud: &FakeCloud, dir: &std::path::Path, tag: &str) -> Automation {
    pdfcraft_assist::secret::keep_in_memory_only();
    let open = cloud.browser();
    Automation::new().with_root(dir).unwrap().with_cloud(CloudSetup {
        google: Some(cloud.google_settings(&format!("auto-{tag}-g"))),
        microsoft: Some(cloud.microsoft_settings(&format!("auto-{tag}-m"))),
        open_browser: Some(Arc::new(open)),
    })
}

#[test]
fn status_reads_no_network_and_a_provider_without_a_client_is_not_set_up() {
    let cloud = FakeCloud::start().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut a = session(&cloud, dir.path(), "status");
    let out = tool(&mut a, "cloud_status", json!({}));
    assert_eq!(out["providers"][0]["provider"], "google");
    assert_eq!((out["providers"][0]["configured"].clone(), out["providers"][0]["connected"].clone()), (json!(true), json!(false)));
    assert_eq!(out["providers"][1]["provider"], "onedrive");
    assert!(cloud.requests().is_empty(), "status is the keychain only");

    let mut bare = Automation::new().with_cloud(CloudSetup {
        google: Some(ProviderSettings::default()),
        microsoft: Some(ProviderSettings::default()),
        open_browser: None,
    });
    assert_eq!(tool(&mut bare, "cloud_status", json!({ "provider": "google" }))["providers"][0]["configured"], false);
    let why = failure(&mut bare, "cloud_list", json!({ "provider": "google" }));
    assert!(why.contains("Google Drive is not set up") && why.contains("PDFCRAFT_GOOGLE_CLIENT_ID"), "{why}");
    assert!(failure(&mut bare, "cloud_connect", json!({ "provider": "onedrive" })).contains("OneDrive is not set up"));
    // Not connected: a clear next step, and still no request.
    let why = failure(&mut a, "cloud_list", json!({ "provider": "google" }));
    assert!(why.contains("not connected") && why.contains("cloud_connect"), "{why}");
    assert!(cloud.requests().is_empty());
    assert!(failure(&mut a, "cloud_list", json!({ "provider": "dropbox" })).contains("google or onedrive"));
}

/// One provider's walk through every tool, in the order a person would use them.
struct Walk {
    cloud: FakeCloud,
    dir: tempfile::TempDir,
    a: Automation,
    provider: &'static str,
    folder: String,
    lease: String,
}

fn walk(provider: &'static str, tag: &str) {
    let cloud = FakeCloud::start().unwrap();
    cloud.cap_page_size(3);
    let folder = cloud.add_folder(None, "Contracts");
    let lease = cloud.add_file(Some(&folder), "lease.pdf", "application/pdf", &pdf("Lease"));
    cloud.add_file(Some(&folder), "notes.md", "text/markdown", b"# Notes\n\nSome *markdown* text.\n");
    cloud.add_file(Some(&folder), "table.csv", "text/csv", b"a,b\n1,2\n3,4\n");
    cloud.add_file(Some(&folder), "movie.mkv", "video/x-matroska", b"v");
    let dir = tempfile::tempdir().unwrap();
    let a = session(&cloud, dir.path(), tag);
    let mut w = Walk { cloud, dir, a, provider, folder, lease };
    w.connect();
    let all = w.list();
    let doc = w.download(&all);
    w.upload(doc);
    w.guards(doc);
    w.disconnect();
}

impl Walk {
    fn call(&mut self, name: &str, mut args: Value) -> Value {
        args["provider"] = json!(self.provider);
        tool(&mut self.a, name, args)
    }

    fn fail(&mut self, name: &str, mut args: Value) -> String {
        args["provider"] = json!(self.provider);
        failure(&mut self.a, name, args)
    }

    /// Connect through the browser stand-in.
    fn connect(&mut self) {
        let out = self.call("cloud_connect", json!({ "timeout_secs": 30 }));
        assert_eq!((out["connected"].clone(), out["account"]["email"].clone()), (json!(true), json!("tester@example.com")));
        let status = self.call("cloud_status", json!({}));
        assert_eq!(status["providers"][0]["account"]["email"], "tester@example.com");
        assert_eq!(status["providers"][0]["connected"], true);
    }

    /// List: root, a paged folder, everything, a search. Returns the folder's items.
    fn list(&mut self) -> Value {
        let root = self.call("cloud_list", json!({}));
        assert_eq!((root["items"][0]["name"].clone(), root["items"][0]["folder"].clone()), (json!("Contracts"), json!(true)));
        let folder = self.folder.clone();
        let page = self.call("cloud_list", json!({ "folder": folder }));
        assert_eq!(page["items"].as_array().unwrap().len(), 3);
        // OneDrive filters by name after paging, so its second page exists and is empty; Google filters in the query.
        if let Some(next) = page["next"].as_str() {
            let rest = self.call("cloud_list", json!({ "folder": folder, "page": next }));
            assert_eq!(rest["items"].as_array().unwrap().len(), 0, "the video is not listed, so the second page is empty: {rest}");
        }
        let all = self.call("cloud_list", json!({ "folder": folder, "all": true }));
        let names: Vec<&str> = all["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(names.contains(&"lease.pdf") && names.contains(&"notes.md") && names.contains(&"table.csv"));
        assert_eq!(all["truncated"], false);
        let found = self.call("cloud_list", json!({ "search": "lease" }));
        assert_eq!(found["items"][0]["id"], self.lease);
        all
    }

    /// Download a PDF (it opens, remembers where it came from, keeps a copy inside the root), and Markdown and CSV.
    fn download(&mut self, all: &Value) -> u64 {
        let lease = self.lease.clone();
        let opened = self.call("cloud_download", json!({ "id": lease, "path": "lease-copy.pdf" }));
        let doc = opened["doc"].as_u64().unwrap();
        assert_eq!((opened["pages"].clone(), opened["converted"].clone()), (json!(1), json!(false)));
        assert_eq!(opened["remote"]["id"], lease);
        assert_eq!(std::fs::read(self.dir.path().join("lease-copy.pdf")).unwrap(), pdf("Lease"));
        // A copy never replaces a file that is there.
        let why = self.fail("cloud_download", json!({ "id": lease, "path": "lease-copy.pdf" }));
        assert!(why.contains("already exists"), "{why}");
        // Markdown and CSV open through the in-process conversion.
        for name in ["notes.md", "table.csv"] {
            let id = all["items"].as_array().unwrap().iter().find(|i| i["name"] == name).unwrap()["id"].as_str().unwrap().to_string();
            let converted = self.call("cloud_download", json!({ "id": id }));
            assert_eq!(converted["converted"], true, "{name}");
            assert!(converted["pages"].as_u64().unwrap() >= 1);
            assert!(converted["name"].as_str().unwrap().ends_with(".pdf"));
        }
        doc
    }

    /// Upload a copy, save back, then see a change made elsewhere refused.
    fn upload(&mut self, doc: u64) {
        let (folder, lease) = (self.folder.clone(), self.lease.clone());
        let copy = self.call("cloud_upload", json!({ "doc": doc, "folder": folder, "name": "lease-signed.pdf" }));
        assert_eq!(copy["created"], true);
        let stored = self.cloud.file(copy["id"].as_str().unwrap()).unwrap();
        assert_eq!((stored.name.as_str(), stored.parent.as_deref()), ("lease-signed.pdf", Some(folder.as_str())));
        assert!(stored.bytes.starts_with(b"%PDF-"));

        let before = self.cloud.file(&lease).unwrap().version;
        let saved = self.call("cloud_upload", json!({ "doc": doc, "id": lease }));
        assert_eq!(saved["created"], false);
        assert!(self.cloud.file(&lease).unwrap().version > before);
        // Saving again is based on the version just written.
        self.call("cloud_upload", json!({ "doc": doc, "id": lease }));

        // Somebody else saves: the next save back is refused, and nothing is uploaded.
        self.cloud.edit(&lease, b"%PDF-1.7 changed elsewhere");
        let why = self.fail("cloud_upload", json!({ "doc": doc, "id": lease }));
        assert!(why.contains("changed on the drive") && why.contains("overwrite"), "{why}");
        assert_eq!(self.cloud.file(&lease).unwrap().bytes, b"%PDF-1.7 changed elsewhere");
        self.call("cloud_upload", json!({ "doc": doc, "id": lease, "overwrite": true }));
        assert!(self.cloud.file(&lease).unwrap().bytes.starts_with(b"%PDF-1.7\n"));
    }

    /// No baseline, no accident; paths stay inside the root; arguments are checked.
    fn guards(&mut self, doc: u64) {
        let (folder, lease) = (self.folder.clone(), self.lease.clone());
        std::fs::write(self.dir.path().join("plain.pdf"), pdf("Plain")).unwrap();
        let why = self.fail("cloud_upload", json!({ "path": "plain.pdf", "id": lease }));
        assert!(why.contains("cannot tell whether the file changed"), "{why}");
        let up = self.call("cloud_upload", json!({ "path": "plain.pdf" }));
        assert_eq!(self.cloud.file(up["id"].as_str().unwrap()).unwrap().bytes, pdf("Plain"));

        assert!(self.fail("cloud_download", json!({ "id": lease, "path": "/tmp/outside.pdf" })).contains("outside"));
        assert!(self.fail("cloud_upload", json!({ "path": "/etc/hosts" })).contains("outside"));
        assert!(self.fail("cloud_upload", json!({})).contains("pass doc"));
        assert!(self.fail("cloud_upload", json!({ "doc": doc, "path": "plain.pdf" })).contains("not both"));
        assert!(self.fail("cloud_download", json!({ "id": folder })).contains("folder"));
        assert!(self.fail("cloud_download", json!({ "id": "missing" })).contains("not found"));
        assert!(self.fail("cloud_list", json!({ "limit": 0 })).contains("limit"));
    }

    /// Disconnect: the account is gone, and listing says to connect.
    fn disconnect(&mut self) {
        let out = self.call("cloud_disconnect", json!({}));
        assert_eq!(out["was_connected"], true);
        assert_eq!(out["revoked_at_provider"], self.provider == "google");
        assert_eq!(self.call("cloud_status", json!({}))["providers"][0]["connected"], false);
        assert!(self.fail("cloud_list", json!({})).contains("not connected"));
        // No token ever appears in an address the server saw.
        for request in self.cloud.requests() {
            assert!(!request.path.contains("at-") && !request.path.contains("rt-"), "{}", request.path);
        }
    }
}

#[test]
fn google_drive_end_to_end() {
    walk("google", "e2e-google");
}

#[test]
fn onedrive_end_to_end() {
    walk("onedrive", "e2e-onedrive");
}

#[test]
fn a_google_document_downloads_as_its_pdf_export_and_cannot_take_a_pdf_back() {
    let cloud = FakeCloud::start().unwrap();
    let doc_id = cloud.add_file(None, "Planning", "application/vnd.google-apps.document", b"");
    let dir = tempfile::tempdir().unwrap();
    let mut a = session(&cloud, dir.path(), "gdoc");
    tool(&mut a, "cloud_connect", json!({ "provider": "google" }));
    let listed = tool(&mut a, "cloud_list", json!({ "provider": "google" }));
    assert_eq!(listed["items"][0]["pdf_export"], true);
    let opened = tool(&mut a, "cloud_download", json!({ "provider": "google", "id": doc_id }));
    assert_eq!((opened["name"].clone(), opened["pages"].clone(), opened["converted"].clone()), (json!("Planning.pdf"), json!(1), json!(false)));
    assert_eq!(cloud.requests_to("/drive/v3/files/").iter().filter(|r| r.path.contains("/export")).count(), 1);
    assert!(cloud.requests().iter().all(|r| !r.path.contains("alt=media")), "a Google document has no bytes of its own");
    let doc = opened["doc"].as_u64().unwrap();
    let why = failure(&mut a, "cloud_upload", json!({ "provider": "google", "doc": doc, "id": doc_id }));
    assert!(why.contains("save a copy"), "{why}");
    assert!(cloud.file(&doc_id).unwrap().bytes.is_empty(), "the Google document was not touched");
    let copy = tool(&mut a, "cloud_upload", json!({ "provider": "google", "doc": doc }));
    assert_eq!(cloud.file(copy["id"].as_str().unwrap()).unwrap().name, "Planning.pdf");
}

#[test]
fn the_cloud_tools_are_in_the_table_with_honest_annotations() {
    let all = pdfcraft_automation::tools();
    let find = |n: &str| all.iter().find(|t| t.name == n).unwrap_or_else(|| panic!("{n} missing"));
    for n in ["cloud_status", "cloud_list"] {
        assert!(find(n).read_only, "{n}");
    }
    for n in ["cloud_connect", "cloud_disconnect", "cloud_download", "cloud_upload"] {
        assert!(!find(n).read_only, "{n} changes local state or the drive");
    }
    assert!(find("cloud_upload").destructive && find("cloud_disconnect").destructive);
}
