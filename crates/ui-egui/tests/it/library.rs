//! The Library window and the vault: listing, searching, opening, tags, missing files, the vault's
//! create, unlock, add, open, save and lock, and its idle lock. The key cache is in memory.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use pdfcraft_library::vault::MemoryCache;
use pdfcraft_library::{AddOptions, KdfParams, Library, Mode};
use pdfcraft_ui_egui::PdfCraftApp;

use crate::gpu;

const PASS: &str = "correct horse battery";

/// A PDF with one page per entry of `pages`, each drawing its text.
fn pdf(pages: &[&str]) -> Vec<u8> {
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
        let content = format!("BT /F1 18 Tf 40 340 Td ({text}) Tj ET");
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

fn write(dir: &Path, name: &str, pages: &[&str]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, pdf(pages)).unwrap();
    path
}

/// The app on a library folder in `dir`, with an in-memory key cache and the cheapest key
/// derivation; background work runs inline.
fn harness(dir: &Path, cache: Arc<MemoryCache>) -> Harness<'static, PdfCraftApp> {
    let lib = dir.join("lib");
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe(move |_cc| {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        app.run_inline = true;
        app.set_option("library-dir", &lib.to_string_lossy()).unwrap();
        app.library_ui.set_key_cache(cache);
        app.library_ui.kdf = KdfParams::MIN_STRENGTH;
        app
    });
    h.run_steps(4);
    h
}

fn titles(h: &Harness<'static, PdfCraftApp>) -> Vec<String> {
    h.state().library_ui.rows.iter().map(|r| r.title.clone()).collect()
}

/// Everything below `dir`, as `(path, bytes)`.
fn files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else { return out };
    for e in read {
        let path = e.unwrap().path();
        if path.is_dir() {
            out.extend(files(&path));
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
fn the_library_lists_searches_filters_by_tag_and_opens_a_document() {
    let _gpu = gpu();
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path().join("lib"));
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    lib.add_file(&write(&src, "Lease.pdf", &["Force majeure clause"]), &AddOptions { tags: vec!["legal".into()], ..Default::default() }).unwrap();
    lib.add_file(&write(&src, "Recipes.pdf", &["Banana bread"]), &AddOptions { tags: vec!["food".into()], ..Default::default() }).unwrap();
    let mut h = harness(dir.path(), Arc::new(MemoryCache::new()));

    assert!(h.state_mut().execute("library.open"), "View > Library");
    h.run_steps(4);
    assert_eq!(titles(&h).len(), 2);
    h.get_by_label("Lease");
    h.get_by_label("Recipes");

    // Text search.
    h.state_mut().library_ui.query = "majeure".into();
    h.state_mut().library_search();
    h.run_steps(3);
    assert_eq!(titles(&h), ["Lease"]);
    assert!(h.state().library_ui.rows[0].snippet.as_deref().is_some_and(|s| s.contains("Force majeure")));
    h.state_mut().library_ui.query.clear();
    h.state_mut().library_search();
    h.run_steps(3);

    // The tag chips filter.
    h.get_by_label("food (1)").click();
    h.run_steps(3);
    assert_eq!(titles(&h), ["Recipes"]);
    h.get_by_label("food (1)").click();
    h.run_steps(3);
    assert_eq!(titles(&h).len(), 2);

    // A click on the title opens the file in a tab.
    assert!(h.state().views.is_empty());
    h.get_by_label("Lease").click();
    h.run_steps(6);
    assert_eq!(h.state().views.len(), 1);
    let id = h.state().views[0].id;
    let path = h.state().session.get(id).and_then(|d| d.path.clone()).unwrap();
    assert!(path.ends_with("lib/Documents/Lease.pdf"), "{path}");
}

#[test]
fn adding_the_current_document_copies_or_references_it_and_a_gone_file_is_marked_missing() {
    let _gpu = gpu();
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path(), Arc::new(MemoryCache::new()));
    let file = write(dir.path(), "Contract.pdf", &["Indemnity words"]);
    h.state_mut().open_path(&file.to_string_lossy());
    h.run_steps(8);
    assert_eq!(h.state().views.len(), 1);

    // Referenced, not copied.
    h.state_mut().library_ui.copy_into_library = false;
    assert!(h.state_mut().execute("library.add"));
    h.run_steps(5);
    assert!(h.state().library_ui.notice.as_deref().is_some_and(|n| n.contains("Added")), "{:?}", h.state().library_ui.notice);
    assert_eq!(titles(&h), ["Contract"]);
    assert!(!dir.path().join("lib/Documents").exists());
    assert!(!h.state().library_ui.rows[0].missing);

    std::fs::remove_file(&file).unwrap();
    h.state_mut().library_reload();
    h.run_steps(4);
    assert!(h.state().library_ui.rows[0].missing, "kept in the list, marked missing");
    h.get_by_label("Missing");
    // Clicking it says where the file was instead of failing silently.
    h.get_by_label("Contract").click();
    h.run_steps(3);
    assert!(h.state().library_ui.notice.as_deref().is_some_and(|n| n.contains("is not on this Mac")), "{:?}", h.state().library_ui.notice);
    // The filter lists only the missing ones.
    h.state_mut().library_ui.missing_only = true;
    h.state_mut().library_search();
    h.run_steps(3);
    assert_eq!(titles(&h), ["Contract"]);
}

#[test]
fn tags_are_edited_in_the_window_and_a_removal_is_confirmed() {
    let _gpu = gpu();
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path().join("lib"));
    lib.add_file(&write(dir.path(), "A.pdf", &["alpha"]), &AddOptions::default()).unwrap();
    let mut h = harness(dir.path(), Arc::new(MemoryCache::new()));
    h.state_mut().open_library();
    h.run_steps(4);
    h.get_by_label("Edit tags").click();
    h.run_steps(2);
    // The tag field takes the keyboard when it opens.
    h.event(egui::Event::Text("one, two".into()));
    h.run_steps(2);
    h.get_by_label("Save").click();
    h.run_steps(8);
    assert_eq!(h.state().library_ui.rows[0].tags, ["one", "two"]);
    // Remove needs a second click.
    h.get_by_label("Remove from the library").click();
    h.run_steps(2);
    assert_eq!(titles(&h).len(), 1);
    h.get_by_label("Remove").click();
    h.run_steps(8);
    assert!(titles(&h).is_empty());
    assert!(!dir.path().join("lib/Documents/A.pdf").exists(), "the library's own copy went with it");
}

#[test]
fn the_vault_is_created_filled_opened_from_memory_saved_encrypted_and_locked() {
    let _gpu = gpu();
    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(MemoryCache::new());
    let mut h = harness(dir.path(), cache.clone());
    h.state_mut().open_library();
    h.run_steps(4);

    // Create: two passphrases that differ are refused.
    h.state_mut().library_ui.passphrase = PASS.to_string().into();
    h.state_mut().library_ui.confirm = "another passphrase".to_string().into();
    h.run_steps(2);
    h.get_by_label("Create vault").click();
    h.run_steps(3);
    assert!(!h.state().library_ui.is_unlocked());
    assert!(h.state().library_ui.notice.as_deref().is_some_and(|n| n.contains("not the same")), "{:?}", h.state().library_ui.notice);
    h.state_mut().library_ui.confirm = PASS.to_string().into();
    h.run_steps(2);
    h.get_by_label("Create vault").click();
    h.run_steps(4);
    assert!(h.state().library_ui.is_unlocked(), "{:?}", h.state().library_ui.notice);
    assert!(h.state().library_ui.passphrase.is_empty(), "the passphrase is not kept");

    // Add the open document; the original is offered for deletion only after the round trip.
    let file = write(dir.path(), "Passport.pdf", &["Passport number 998877"]);
    h.state_mut().open_path(&file.to_string_lossy());
    h.run_steps(8);
    h.get_by_label("Add current document to the vault").click();
    h.run_steps(5);
    assert!(h.state().library_ui.pending_delete.is_some(), "{:?}", h.state().library_ui.notice);
    assert!(file.exists());
    h.get_by_label("Delete original").click();
    h.run_steps(3);
    assert!(!file.exists(), "deleted after the verified copy");
    for (path, bytes) in files(&dir.path().join("lib")) {
        assert!(!contains(&bytes, "998877") && !contains(&bytes, "%PDF"), "{} is readable", path.display());
    }

    // The vault document is listed while unlocked and opens from memory with no path.
    h.state_mut().close_tab(0);
    h.run_steps(2);
    assert_eq!(titles(&h), ["Passport"]);
    h.get_by_label("Passport").click();
    h.run_steps(8);
    assert_eq!(h.state().views.len(), 1);
    let id = h.state().views[0].id;
    assert!(h.state().library_ui.is_vault_document(id));
    assert!(h.state().session.get(id).unwrap().path.is_none(), "no file");

    // An edit and Save: encrypted again, no plain file, no recovery snapshot.
    h.state_mut().session.apply(id, pdfcraft_engine::Edit::RotatePages { pages: vec![0], degrees: 90 }).unwrap();
    assert!(h.state().session.get(id).unwrap().dirty);
    assert!(h.state_mut().session.autosave_snapshots().is_empty(), "kept off the disk");
    assert!(h.state_mut().execute("file.save"));
    h.run_steps(4);
    assert!(!h.state().session.get(id).unwrap().dirty, "saved");
    for (path, bytes) in files(dir.path()) {
        assert!(!contains(&bytes, "998877") && !contains(&bytes, "%PDF"), "{} is readable after the save", path.display());
    }

    // Locking closes the clean tab, hides the vault from the list, and a locked save is refused
    // without losing the work.
    assert!(h.state_mut().execute("vault.lock"));
    h.run_steps(4);
    assert!(!h.state().library_ui.is_unlocked());
    assert!(h.state().views.is_empty(), "the clean vault tab closed with the lock");
    assert!(titles(&h).is_empty(), "locked: the vault's documents are not listed");
    h.get_by_label("Unlock");
}

#[test]
fn a_locked_vault_keeps_unsaved_work_and_refuses_to_save_until_unlocked() {
    let _gpu = gpu();
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path(), Arc::new(MemoryCache::new()));
    // A vault made through the library crate, as another Mac would have.
    let vault = pdfcraft_library::Vault::create(&dir.path().join("lib"), PASS, KdfParams::MIN_STRENGTH).unwrap();
    let id = vault.add(&pdf(&["Secret words"]), &pdfcraft_library::VaultAddOptions { name: "S.pdf".into(), ..Default::default() }).unwrap().entry.id;
    drop(vault);
    h.state_mut().open_library();
    h.run_steps(4);
    h.state_mut().library_ui.passphrase = PASS.to_string().into();
    h.run_steps(2);
    h.get_by_label("Unlock").click();
    h.run_steps(4);
    assert!(h.state().library_ui.is_unlocked());
    h.state_mut().vault_open(&id);
    h.run_steps(8);
    let doc = h.state().views[0].id;
    h.state_mut().session.apply(doc, pdfcraft_engine::Edit::RotatePages { pages: vec![0], degrees: 90 }).unwrap();

    h.state_mut().lock_vault();
    h.run_steps(3);
    assert_eq!(h.state().views.len(), 1, "unsaved work is never closed by a lock");
    assert!(!h.state_mut().save_active(pdfcraft_ui_egui::SaveTarget::InPlace), "locked: not saved");
    assert!(h.state().session.get(doc).unwrap().dirty);
    assert!(h.state().library_ui.open);
    // Unlocked again, the save goes through.
    h.state_mut().library_ui.passphrase = PASS.to_string().into();
    h.run_steps(2);
    h.get_by_label("Unlock").click();
    h.run_steps(4);
    assert!(h.state_mut().save_active(pdfcraft_ui_egui::SaveTarget::InPlace));
    assert!(!h.state().session.get(doc).unwrap().dirty);
}

#[test]
fn the_remembered_key_unlocks_without_the_passphrase_and_the_vault_locks_when_idle_or_on_quit() {
    let _gpu = gpu();
    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(MemoryCache::new());
    let mut h = harness(dir.path(), cache.clone());
    h.state_mut().library_ui.idle_minutes = 5;
    let vault = pdfcraft_library::Vault::create(&dir.path().join("lib"), PASS, KdfParams::MIN_STRENGTH).unwrap();
    drop(vault);
    h.state_mut().open_library();
    h.run_steps(4);
    h.state_mut().library_ui.passphrase = PASS.to_string().into();
    h.state_mut().library_ui.remember_key = true;
    h.run_steps(2);
    h.get_by_label("Unlock").click();
    h.run_steps(4);
    assert!(h.state().library_ui.is_unlocked());

    // Idle for longer than the limit: it locks by itself.
    h.state_mut().library_ui.advance_clock(Duration::from_secs(6 * 60));
    h.run_steps(3);
    assert!(!h.state().library_ui.is_unlocked(), "locked after being idle");

    // Touch ID (the cache) unlocks it without a passphrase.
    h.get_by_label("Unlock with memory").click();
    h.run_steps(4);
    assert!(h.state().library_ui.is_unlocked());
    assert_eq!(cache.loads(), 1);

    // Quitting wipes the key from memory.
    use eframe::App as _;
    h.state_mut().on_exit(None);
    assert!(!h.state().library_ui.is_unlocked());
}

#[test]
fn the_preferences_keep_the_folder_and_idle_time_but_never_a_passphrase() {
    let _gpu = gpu();
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path(), Arc::new(MemoryCache::new()));
    h.state_mut().library_ui.passphrase = "do not persist this".to_string().into();
    h.state_mut().library_ui.idle_minutes = 7;
    let saved = h.state().persist();
    assert!(saved.contains("idle_minutes") && saved.contains("lib"), "{saved}");
    assert!(!saved.contains("do not persist this"));
    let _ = Mode::Copy;
}
