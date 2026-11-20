//! The library and vault tools through the tool table, on synthetic PDFs in temporary folders.
//! The vault's key cache is in memory: no real keychain is touched.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pdfcraft_automation::{Automation, Content};
use pdfcraft_library::KdfParams;
use pdfcraft_library::vault::MemoryCache;
use serde_json::{Value, json};

const PASS: &str = "correct horse battery";

fn tool(a: &mut Automation, name: &str, args: Value) -> Value {
    match a.call(name, &args).unwrap_or_else(|e| panic!("{name} failed: {e}")).remove(0) {
        Content::Json(value) => value,
        other => panic!("expected JSON: {other:?}"),
    }
}

fn fails(a: &mut Automation, name: &str, args: Value) -> String {
    a.call(name, &args).err().unwrap_or_else(|| panic!("{name} should have failed")).to_string()
}

/// A PDF with one page per entry, each drawing its text; `invisible` draws it in text render mode
/// 3, as Scan & OCR lays recognised text over a scanned page.
fn pdf(pages: &[&str], invisible: bool) -> Vec<u8> {
    let n = pages.len();
    let mut objs: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {n} /MediaBox [0 0 300 400] /Resources << /Font << /F1 3 0 R >> >> >>",
            (0..n).map(|i| format!("{} 0 R", 4 + 2 * i)).collect::<Vec<_>>().join(" ")
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    for (i, text) in pages.iter().enumerate() {
        let mode = if invisible { "3 Tr " } else { "" };
        let content = format!("BT {mode}/F1 18 Tf 40 340 Td ({text}) Tj ET");
        objs.push(format!("<< /Type /Page /Parent 2 0 R /Contents {} 0 R >>", 5 + 2 * i));
        objs.push(format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()));
    }
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

/// `--root` at `dir`, the library in `dir/lib`, an in-memory key cache and the cheapest key
/// derivation.
fn session(dir: &Path) -> (Automation, Arc<MemoryCache>) {
    let cache = Arc::new(MemoryCache::new());
    let a = Automation::new()
        .with_root(dir)
        .unwrap()
        .with_library_dir(dir.join("lib"))
        .with_vault_key_cache(cache.clone())
        .with_vault_kdf(KdfParams::MIN_STRENGTH);
    (a, cache)
}

fn write(dir: &Path, name: &str, pages: &[&str]) {
    std::fs::write(dir.join(name), pdf(pages, false)).unwrap();
}

fn ids(list: &Value) -> Vec<String> {
    list["documents"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect()
}

#[test]
fn a_document_is_added_listed_searched_opened_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, _) = session(dir.path());
    write(dir.path(), "Lease.pdf", &["Force majeure clause", "Definitions of terms"]);
    write(dir.path(), "Recipes.pdf", &["Banana bread"]);

    let lease = tool(&mut a, "library_add", json!({ "path": "Lease.pdf", "tags": ["legal"] }));
    assert_eq!((lease["created"].as_bool(), lease["copied"].as_bool(), lease["pages"].as_u64()), (Some(true), Some(true), Some(2)));
    assert_eq!(lease["location"]["kind"], "library");
    assert_eq!(lease["location"]["path"], "Documents/Lease.pdf", "relative to the library folder, so it is right on every Mac");
    assert!(dir.path().join("lib/Documents/Lease.pdf").is_file());
    tool(&mut a, "library_add", json!({ "path": "Recipes.pdf", "title": "Family recipes" }));

    let list = tool(&mut a, "library_list", json!({}));
    assert_eq!(list["documents"].as_array().unwrap().len(), 2);
    assert_eq!(list["tags"], json!([{ "tag": "legal", "count": 1 }]));
    assert_eq!(list["documents"][0]["title"], "Family recipes", "the newest first");

    // Titles, tags and text are searched; the text hit comes with a snippet.
    let hits = tool(&mut a, "library_search", json!({ "query": "majeure" }));
    assert_eq!(hits["results"].as_array().unwrap().len(), 1);
    assert_eq!(hits["results"][0]["title"], "Lease");
    assert_eq!(hits["results"][0]["matched"], json!(["text"]));
    assert!(hits["results"][0]["snippet"].as_str().unwrap().contains("Force majeure"));
    assert_eq!(tool(&mut a, "library_search", json!({ "query": "family" }))["results"][0]["matched"], json!(["title"]));
    assert_eq!(tool(&mut a, "library_search", json!({ "query": "legal" }))["results"][0]["matched"], json!(["tag"]));
    assert!(tool(&mut a, "library_search", json!({ "query": "zebra" }))["results"].as_array().unwrap().is_empty());
    assert_eq!(tool(&mut a, "library_search", json!({ "query": "", "tag": "legal" }))["results"].as_array().unwrap().len(), 1);

    // Opening puts the file in the session and makes it the most recent.
    let id = lease["id"].as_str().unwrap().to_string();
    let opened = tool(&mut a, "library_open", json!({ "id": id }));
    assert_eq!((opened["pages"].as_u64(), opened["library_id"].as_str()), (Some(2), Some(id.as_str())));
    assert_eq!(opened["marked_opened"], true);
    assert_eq!(ids(&tool(&mut a, "library_list", json!({})))[0], id);

    // Removing deletes the library's own copy and the entry.
    let removed = tool(&mut a, "library_remove", json!({ "id": id }));
    assert_eq!(removed["deleted_copy"], true);
    assert!(!dir.path().join("lib/Documents/Lease.pdf").exists());
    assert_eq!(tool(&mut a, "library_list", json!({}))["documents"].as_array().unwrap().len(), 1);
    assert!(fails(&mut a, "library_remove", json!({ "id": id })).contains("no document"));
}

#[test]
fn recognised_text_from_scan_and_ocr_is_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, _) = session(dir.path());
    std::fs::write(dir.path().join("scan.pdf"), pdf(&["Invoice 4711 surveyor"], true)).unwrap();
    let added = tool(&mut a, "library_add", json!({ "path": "scan.pdf" }));
    assert!(added["note"].is_null(), "text was found: {added}");
    assert_eq!(tool(&mut a, "library_search", json!({ "query": "surveyor 4711" }))["results"].as_array().unwrap().len(), 1);
}

#[test]
fn a_document_with_no_text_is_added_with_a_note_that_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, _) = session(dir.path());
    std::fs::write(dir.path().join("blank.pdf"), pdf(&[""], false)).unwrap();
    let added = tool(&mut a, "library_add", json!({ "path": "blank.pdf" }));
    assert!(added["note"].as_str().unwrap().contains("OCR"), "{added}");
}

#[test]
fn a_referenced_file_is_marked_missing_when_it_is_gone_and_never_deleted_by_remove() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, _) = session(dir.path());
    write(dir.path(), "outside.pdf", &["Referenced words"]);
    let added = tool(&mut a, "library_add", json!({ "path": "outside.pdf", "copy": false }));
    assert_eq!(
        (added["copied"].as_bool(), added["location"]["kind"].as_str(), added["missing"].as_bool()),
        (Some(false), Some("external"), Some(false))
    );
    assert!(!dir.path().join("lib/Documents").exists(), "nothing was copied");
    let id = added["id"].as_str().unwrap().to_string();

    std::fs::remove_file(dir.path().join("outside.pdf")).unwrap();
    let list = tool(&mut a, "library_list", json!({}));
    assert_eq!(list["documents"][0]["missing"], true, "still listed, marked missing");
    assert_eq!(tool(&mut a, "library_list", json!({ "missing_only": true }))["documents"].as_array().unwrap().len(), 1);
    assert!(fails(&mut a, "library_open", json!({ "id": id })).contains("missing"));
    // Its text is still searchable: the index does not depend on the file being there.
    assert_eq!(tool(&mut a, "library_search", json!({ "query": "referenced" }))["results"].as_array().unwrap().len(), 1);
    let removed = tool(&mut a, "library_remove", json!({ "id": id }));
    assert_eq!(removed["deleted_copy"], false);
}

#[test]
fn an_open_document_is_added_and_tags_can_be_changed() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, _) = session(dir.path());
    write(dir.path(), "open.pdf", &["Open document text"]);
    let doc = tool(&mut a, "doc_open", json!({ "path": "open.pdf" }))["doc"].as_u64().unwrap();
    let added = tool(&mut a, "library_add", json!({ "doc": doc, "tags": ["a", "b"] }));
    let id = added["id"].as_str().unwrap().to_string();
    let tagged = tool(&mut a, "library_tag", json!({ "id": id, "add": ["c"], "remove": ["A"], "title": "Renamed" }));
    assert_eq!((tagged["tags"].clone(), tagged["title"].as_str()), (json!(["b", "c"]), Some("Renamed")));
    assert_eq!(tool(&mut a, "library_list", json!({ "tag": "c" }))["documents"].as_array().unwrap().len(), 1);
    assert!(tool(&mut a, "library_list", json!({ "tag": "a" }))["documents"].as_array().unwrap().is_empty());
}

#[test]
fn bad_input_is_an_error_not_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, _) = session(dir.path());
    std::fs::write(dir.path().join("notes.pdf"), b"just words").unwrap();
    assert!(fails(&mut a, "library_add", json!({ "path": "notes.pdf" })).contains("not a PDF"));
    assert!(fails(&mut a, "library_add", json!({})).contains("path"));
    assert!(fails(&mut a, "library_add", json!({ "path": "missing.pdf" })).contains("not found"));
    assert!(fails(&mut a, "library_add", json!({ "path": "notes.pdf", "doc": 1 })).contains("not both"));
    assert!(fails(&mut a, "library_open", json!({ "id": "../../etc/passwd" })).contains("no document"));
    assert!(!dir.path().join("lib/library.json").exists(), "a failed add writes nothing");
}

#[test]
fn root_confines_the_library_folder() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = Automation::new().with_root(dir.path()).unwrap();
    write(dir.path(), "a.pdf", &["x"]);
    // No folder given and none configured: the default is outside --root, so it is not used.
    assert!(fails(&mut a, "library_list", json!({})).contains("library"));
    assert!(fails(&mut a, "library_add", json!({ "path": "a.pdf", "library": "../elsewhere" })).contains("outside"));
    let ok = tool(&mut a, "library_add", json!({ "path": "a.pdf", "library": "mine" }));
    assert!(ok["library"].as_str().unwrap().ends_with("mine"));
    assert!(dir.path().join("mine/library.json").is_file());
}

// ---- the vault ----------------------------------------------------------------------------

fn create_vault(a: &mut Automation) {
    let out = tool(a, "vault_create", json!({ "passphrase": PASS }));
    assert_eq!((out["created"].as_bool(), out["unlocked"].as_bool()), (Some(true), Some(true)));
}

/// Every file below `dir`, as `(path, bytes)`.
fn all_files(dir: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let path = e.unwrap().path();
        if path.is_dir() {
            out.extend(all_files(&path));
        } else {
            out.push((path.clone(), std::fs::read(path).unwrap()));
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[test]
fn a_vault_document_is_encrypted_opened_in_memory_and_saved_encrypted() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, _) = session(dir.path());
    create_vault(&mut a);
    write(dir.path(), "Passport.pdf", &["Passport number 998877"]);
    let added = tool(&mut a, "vault_add", json!({ "path": "Passport.pdf", "tags": ["id"] }));
    assert_eq!(added["verified"], true);
    assert_eq!(added["original_deleted"], false, "the original stays unless asked");
    let id = added["id"].as_str().unwrap().to_string();

    // Nothing readable under the library folder.
    for (path, bytes) in all_files(&dir.path().join("lib")) {
        assert!(!contains(&bytes, "Passport") && !contains(&bytes, "998877") && !contains(&bytes, "%PDF"), "{} is readable", path.display());
    }

    // Open: in memory, no file.
    let opened = tool(&mut a, "vault_open", json!({ "id": id }));
    assert!(opened["path"].is_null(), "no path: {opened}");
    assert_eq!(opened["plaintext_on_disk"], false);
    assert_eq!(opened["vault_id"], id.as_str());
    let doc = opened["doc"].as_u64().unwrap();
    let text = tool(&mut a, "text_extract", json!({ "doc": doc, "pages": [1] }));
    assert!(text.to_string().contains("998877"));

    // Edit and save: doc_save without a path encrypts again.
    tool(&mut a, "page_rotate", json!({ "doc": doc, "pages": [1], "degrees": 90 }));
    let saved = tool(&mut a, "doc_save", json!({ "doc": doc }));
    assert_eq!((saved["plaintext_on_disk"].as_bool(), saved["verified"].as_bool()), (Some(false), Some(true)));
    assert_eq!(saved["document"]["dirty"], false);
    for (path, bytes) in all_files(&dir.path().join("lib")) {
        assert!(!contains(&bytes, "998877") && !contains(&bytes, "%PDF"), "{} is readable after the save", path.display());
    }
    // The saved change is in the vault.
    let again = tool(&mut a, "vault_open", json!({ "id": id }));
    let info = tool(&mut a, "doc_info", json!({ "doc": again["doc"] }));
    assert!(info.to_string().contains("\"rotation\":90") || info.to_string().contains("\"rotate\":90"), "{info}");
}

#[test]
fn a_locked_vault_gives_nothing_and_its_documents_leave_search() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, _) = session(dir.path());
    create_vault(&mut a);
    write(dir.path(), "Bank.pdf", &["Account number 55123"]);
    write(dir.path(), "Public.pdf", &["Account of a public meeting"]);
    let secret = tool(&mut a, "vault_add", json!({ "path": "Bank.pdf" }))["id"].as_str().unwrap().to_string();
    tool(&mut a, "library_add", json!({ "path": "Public.pdf" }));

    // Unlocked: the vault is searched too.
    let hits = tool(&mut a, "library_search", json!({ "query": "account" }));
    assert_eq!(hits["vault_searched"], true);
    assert_eq!(hits["results"].as_array().unwrap().len(), 2);
    assert!(hits["results"].as_array().unwrap().iter().any(|r| r["vault"] == true));
    assert_eq!(tool(&mut a, "vault_list", json!({}))["documents"].as_array().unwrap().len(), 1);

    tool(&mut a, "vault_lock", json!({}));
    let hits = tool(&mut a, "library_search", json!({ "query": "account" }));
    assert_eq!(hits["vault_searched"], false);
    assert_eq!(hits["results"].as_array().unwrap().len(), 1, "only the public document: {hits}");
    assert!(!hits.to_string().contains("55123"));
    let listed = tool(&mut a, "vault_list", json!({}));
    assert_eq!((listed["exists"].as_bool(), listed["unlocked"].as_bool()), (Some(true), Some(false)));
    assert!(listed["documents"].as_array().unwrap().is_empty(), "a locked vault lists no documents");
    for (name, args) in
        [("vault_open", json!({ "id": secret })), ("vault_add", json!({ "path": "Bank.pdf" })), ("vault_remove", json!({ "id": secret }))]
    {
        assert!(fails(&mut a, name, args).contains("locked"), "{name}");
    }

    // Unlocked again with the passphrase, the document is back in search.
    assert!(fails(&mut a, "vault_unlock", json!({ "passphrase": "wrong passphrase" })).contains("does not open"));
    tool(&mut a, "vault_unlock", json!({ "passphrase": PASS }));
    assert_eq!(tool(&mut a, "library_search", json!({ "query": "55123" }))["results"].as_array().unwrap().len(), 1);
    assert_eq!(tool(&mut a, "library_search", json!({ "query": "55123", "vault": false }))["results"].as_array().unwrap().len(), 0);
}

#[test]
fn after_a_passphrase_unlock_the_remembered_key_unlocks_without_one() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, cache) = session(dir.path());
    let created = tool(&mut a, "vault_create", json!({ "passphrase": PASS, "remember": true }));
    assert_eq!((created["remembered"].as_bool(), created["key_cache"]["remembered"].as_bool()), (Some(true), Some(true)));
    tool(&mut a, "vault_lock", json!({}));
    assert_eq!(cache.loads(), 0);
    let unlocked = tool(&mut a, "vault_unlock", json!({}));
    assert_eq!(unlocked["unlocked"], true);
    assert_eq!(cache.loads(), 1, "the key came from the cache, which is where Touch ID is asked");
    // Forgetting the key makes the passphrase necessary again.
    let locked = tool(&mut a, "vault_lock", json!({ "forget_device_key": true }));
    assert_eq!(locked["device_key_forgotten"], true);
    assert!(fails(&mut a, "vault_unlock", json!({})).contains("passphrase"));
    // Not remembering is the default and remembers nothing.
    tool(&mut a, "vault_unlock", json!({ "passphrase": PASS }));
    tool(&mut a, "vault_lock", json!({}));
    assert!(fails(&mut a, "vault_unlock", json!({})).contains("passphrase"));
}

#[test]
fn the_original_is_deleted_only_after_a_verified_round_trip_and_only_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, _) = session(dir.path());
    create_vault(&mut a);
    write(dir.path(), "Tax.pdf", &["Tax return 2026"]);
    let added = tool(&mut a, "vault_add", json!({ "path": "Tax.pdf", "delete_original": true }));
    assert_eq!((added["verified"].as_bool(), added["original_deleted"].as_bool()), (Some(true), Some(true)));
    assert!(!dir.path().join("Tax.pdf").exists());
    // The vault still has it.
    let opened = tool(&mut a, "vault_open", json!({ "id": added["id"] }));
    assert!(tool(&mut a, "text_extract", json!({ "doc": opened["doc"], "pages": [1] })).to_string().contains("Tax return"));

    // An open document with unsaved changes: the file on disk is not what would be encrypted.
    write(dir.path(), "Draft.pdf", &["Draft"]);
    let doc = tool(&mut a, "doc_open", json!({ "path": "Draft.pdf" }))["doc"].as_u64().unwrap();
    tool(&mut a, "page_rotate", json!({ "doc": doc, "pages": [1], "degrees": 90 }));
    let why = fails(&mut a, "vault_add", json!({ "doc": doc, "delete_original": true }));
    assert!(why.contains("unsaved"), "{why}");
    assert!(dir.path().join("Draft.pdf").exists());
    // Without delete_original the edited state is encrypted, and the file stays.
    let kept = tool(&mut a, "vault_add", json!({ "doc": doc }));
    assert_eq!(kept["original_deleted"], false);
    assert!(dir.path().join("Draft.pdf").exists());
}

#[test]
fn locking_closes_clean_vault_documents_and_never_throws_away_unsaved_work() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, _) = session(dir.path());
    create_vault(&mut a);
    write(dir.path(), "One.pdf", &["one"]);
    write(dir.path(), "Two.pdf", &["two"]);
    let one = tool(&mut a, "vault_add", json!({ "path": "One.pdf" }))["id"].as_str().unwrap().to_string();
    let two = tool(&mut a, "vault_add", json!({ "path": "Two.pdf" }))["id"].as_str().unwrap().to_string();
    let clean = tool(&mut a, "vault_open", json!({ "id": one }))["doc"].as_u64().unwrap();
    let dirty = tool(&mut a, "vault_open", json!({ "id": two }))["doc"].as_u64().unwrap();
    tool(&mut a, "page_rotate", json!({ "doc": dirty, "pages": [1], "degrees": 90 }));

    let locked = tool(&mut a, "vault_lock", json!({}));
    assert_eq!(locked["closed_documents"], json!([clean]));
    assert_eq!(locked["unsaved_documents_kept_open"][0]["doc"], dirty);
    let listed = tool(&mut a, "doc_list", json!({}));
    assert_eq!(listed["documents"].as_array().unwrap().len(), 1);
    // Saving needs the vault: locked, the save fails and the work is still there.
    assert!(fails(&mut a, "doc_save", json!({ "doc": dirty })).contains("locked"));
    tool(&mut a, "vault_unlock", json!({ "passphrase": PASS }));
    tool(&mut a, "doc_save", json!({ "doc": dirty }));
    let locked = tool(&mut a, "vault_lock", json!({}));
    assert_eq!(locked["closed_documents"], json!([dirty]));
}

#[test]
fn the_vault_locks_itself_after_the_idle_time() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(MemoryCache::new());
    let mut a = Automation::new()
        .with_root(dir.path())
        .unwrap()
        .with_library_dir(dir.path().join("lib"))
        .with_vault_key_cache(cache)
        .with_vault_kdf(KdfParams::MIN_STRENGTH)
        .with_vault_idle_timeout(Some(Duration::from_millis(80)));
    create_vault(&mut a);
    assert_eq!(tool(&mut a, "vault_list", json!({}))["unlocked"], true);
    std::thread::sleep(Duration::from_millis(150));
    let listed = tool(&mut a, "vault_list", json!({}));
    assert_eq!(listed["unlocked"], false, "locked after the idle time: {listed}");
}

#[test]
fn removing_a_vault_document_deletes_its_file_and_a_second_create_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, _) = session(dir.path());
    create_vault(&mut a);
    assert!(fails(&mut a, "vault_create", json!({ "passphrase": "another passphrase" })).contains("already exists"));
    assert!(fails(&mut a, "vault_create", json!({ "passphrase": "x" })).contains("at least"));
    write(dir.path(), "a.pdf", &["gone"]);
    let id = tool(&mut a, "vault_add", json!({ "path": "a.pdf" }))["id"].as_str().unwrap().to_string();
    let files_before = all_files(&dir.path().join("lib/Vault")).len();
    tool(&mut a, "vault_remove", json!({ "id": id }));
    assert!(tool(&mut a, "vault_list", json!({}))["documents"].as_array().unwrap().is_empty());
    assert!(all_files(&dir.path().join("lib/Vault")).len() < files_before, "the encrypted document file is gone");
    assert!(fails(&mut a, "vault_open", json!({ "id": id })).contains("no document"));
}

#[test]
fn a_short_passphrase_is_refused_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, _) = session(dir.path());
    assert!(fails(&mut a, "vault_create", json!({ "passphrase": "short" })).contains("at least"));
    assert!(fails(&mut a, "vault_create", json!({})).contains("passphrase"));
    assert!(!dir.path().join("lib/Vault").exists());
}
