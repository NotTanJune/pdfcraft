//! Cloud drives: connect, browse, open, save back, save a copy and disconnect, against a server on
//! 127.0.0.1 that speaks the OAuth, Google Drive and Microsoft Graph endpoints (no real network).

use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use pdfcraft_clouddrive::mock::{FakeCloud, sample_pdf};
use pdfcraft_clouddrive::{ProviderKind, ProviderSettings, load_client};
use pdfcraft_ui_egui::PdfCraftApp;

use crate::gpu;

type H = Harness<'static, PdfCraftApp>;

fn harness(settings: [Option<ProviderSettings>; 2], cache: std::path::PathBuf) -> H {
    pdfcraft_assist::secret::keep_in_memory_only();
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe(move |_cc| {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        app.run_inline = true;
        app.set_option("left", "closed").unwrap();
        app.set_option("panel", "none").unwrap();
        app.cloud.overrides = settings;
        app.cloud.cache_dir = Some(cache);
        app
    });
    h.run_steps(4);
    h
}

/// Runs frames until `done` holds (the transfers run on their own threads).
fn wait(h: &mut H, what: &str, done: impl Fn(&PdfCraftApp) -> bool) {
    let until = Instant::now() + Duration::from_secs(20);
    while !done(h.state()) {
        assert!(Instant::now() < until, "timed out waiting for {what}; notice: {:?}", h.state().cloud.notice());
        h.run_steps(2);
        std::thread::sleep(Duration::from_millis(15));
    }
    h.run_steps(2);
}

fn idle(app: &PdfCraftApp) -> bool {
    !app.cloud.is_busy()
}

/// Connects Google Drive through the window, with the fake as the person's browser.
fn connect(h: &mut H, cloud: &FakeCloud) {
    assert!(h.state_mut().execute("cloud.drives"));
    h.run_steps(3);
    h.get_by_label("Connect").click();
    wait(h, "the sign-in address", |a| a.last_opened_url.is_some());
    let address = h.state().last_opened_url.clone().unwrap();
    assert!(address.starts_with(&format!("{}/authorize?", cloud.base())), "{address}");
    assert!(address.contains("code_challenge_method=S256") && address.contains("state="));
    (cloud.browser())(&address).unwrap();
    wait(h, "the account", |a| a.cloud.is_connected(ProviderKind::Google) && idle(a));
}

#[test]
fn the_window_connects_browses_opens_saves_back_and_disconnects() {
    let _gpu = gpu();
    let cloud = FakeCloud::start().unwrap();
    let folder = cloud.add_folder(None, "Contracts");
    let lease = cloud.add_file(Some(&folder), "lease.pdf", "application/pdf", &sample_pdf("Lease"));
    cloud.add_file(Some(&folder), "notes.md", "text/markdown", b"# Notes\n\nSome *markdown* text.\n");
    let cache = tempfile::tempdir().unwrap();
    let mut h = harness([Some(cloud.google_settings("ui-walk")), None], cache.path().to_path_buf());

    // Off until connected: opening the window sends nothing.
    assert!(h.state_mut().execute("cloud.drives"));
    h.run_steps(3);
    assert!(cloud.requests().is_empty());
    h.state_mut().cloud.open = false;
    h.run_steps(2);
    connect(&mut h, &cloud);
    h.get_by_label("Signed in as tester@example.com");
    wait(&mut h, "the root listing", |a| a.cloud.listed_names(ProviderKind::Google) == ["Contracts"]);

    // Into the folder: the breadcrumb shows the way, and a PDF opens.
    h.get_by_label("Contracts").click();
    wait(&mut h, "the folder listing", |a| a.cloud.listed_names(ProviderKind::Google).len() == 2);
    assert_eq!(h.state().cloud.listed_names(ProviderKind::Google), ["lease.pdf", "notes.md"]);
    h.get_by_label("Open lease.pdf").click();
    wait(&mut h, "the PDF to open", |a| a.views.len() == 1 && idle(a));
    h.run_steps(4);
    let doc = h.state().views[0].id;
    let link = h.state().cloud.link_of(doc).cloned().expect("the tab remembers its file");
    assert_eq!((link.id.as_str(), link.name.as_str(), link.exported), (lease.as_str(), "lease.pdf", false));
    let path = h.state().session.get(doc).and_then(|d| d.path.clone()).unwrap();
    assert!(std::path::Path::new(&path).starts_with(cache.path()), "the file is kept in the cache folder: {path}");

    // Markdown opens through the conversion that File > Open uses.
    h.state_mut().cloud.open = true;
    h.run_steps(3);
    h.get_by_label("Open notes.md").click();
    wait(&mut h, "the Markdown file to open", |a| a.views.len() == 2 && idle(a));
    assert!(h.state().cloud.link_of(h.state().views[1].id).is_none(), "a converted document has no file to save back to");

    // Save to drive: the saved bytes become a new revision, and the link follows the new version.
    h.state_mut().active = Some(0);
    let before = cloud.file(&lease).unwrap();
    assert!(h.state_mut().execute("cloud.save"));
    wait(&mut h, "the upload", |a| idle(a) && a.cloud.link_of(a.views[0].id).is_some_and(|l| l.version.as_deref() != Some("1")));
    let after = cloud.file(&lease).unwrap();
    assert!(after.version > before.version, "a new revision");
    assert!(after.bytes.starts_with(b"%PDF-"));

    // Somebody else saves first: Save to drive asks, and Replace it overwrites.
    cloud.edit(&lease, b"%PDF-1.7 someone else");
    assert!(h.state_mut().execute("cloud.save"));
    wait(&mut h, "the question", |a| a.cloud.has_conflict());
    h.get_by_label_contains("changed on the drive");
    assert_eq!(cloud.file(&lease).unwrap().bytes, b"%PDF-1.7 someone else", "nothing was uploaded");
    h.get_by_label("Replace it").click();
    wait(&mut h, "the replacement", |a| idle(a) && !a.cloud.has_conflict());
    assert!(cloud.file(&lease).unwrap().bytes.starts_with(b"%PDF-1.7\n"), "the document replaced the other change");

    // Save a copy: pick the root through the breadcrumb, then Save here.
    assert!(h.state_mut().execute("cloud.save_copy"));
    h.run_steps(3);
    h.get_by_label_contains("Save a copy to Google Drive");
    h.get_by_label("My Drive").click();
    wait(&mut h, "the root listing", |a| a.cloud.listed_names(ProviderKind::Google) == ["Contracts"]);
    h.get_by_label("Save here").click();
    wait(&mut h, "the copy", idle);
    let copies = cloud.find_all("lease.pdf");
    assert_eq!(copies.len(), 2, "the original and the copy");
    assert!(copies.iter().any(|f| f.parent.is_none()), "the copy is in the root");

    // Disconnect: the sign-in is gone and so are the links.
    h.state_mut().cloud.open = true;
    h.run_steps(3);
    h.get_by_label("Disconnect").click();
    wait(&mut h, "disconnecting", |a| idle(a) && !a.cloud.is_connected(ProviderKind::Google));
    assert!(cloud.revoked().len() == 1, "Google was asked to revoke the token");
    assert!(h.state().cloud.link_of(doc).is_none());
    assert!(pdfcraft_assist::secret::load_named("cloud/google.ui-walk").unwrap().is_none());
    h.get_by_label("Connect");
}

#[test]
fn onedrive_lists_and_opens_through_the_same_window() {
    let _gpu = gpu();
    let cloud = FakeCloud::start().unwrap();
    cloud.add_file(None, "Plan.pdf", "application/pdf", &sample_pdf("Plan"));
    let cache = tempfile::tempdir().unwrap();
    let mut h = harness([None, Some(cloud.microsoft_settings("ui-onedrive"))], cache.path().to_path_buf());
    h.state_mut().cloud.select(ProviderKind::Microsoft);
    assert!(h.state_mut().execute("cloud.drives"));
    h.run_steps(3);
    h.get_by_label("Connect").click();
    wait(&mut h, "the sign-in address", |a| a.last_opened_url.is_some());
    (cloud.browser())(&h.state().last_opened_url.clone().unwrap()).unwrap();
    wait(&mut h, "the listing", |a| a.cloud.listed_names(ProviderKind::Microsoft) == ["Plan.pdf"] && idle(a));
    h.get_by_label("Open Plan.pdf").click();
    wait(&mut h, "the PDF to open", |a| a.views.len() == 1 && idle(a));
    h.run_steps(4);
    let doc = h.state().views[0].id;
    assert_eq!(h.state().cloud.link_of(doc).map(|l| l.provider), Some(ProviderKind::Microsoft));
}

#[test]
fn a_transfer_can_be_cancelled_and_nothing_opens() {
    let _gpu = gpu();
    let cloud = FakeCloud::start().unwrap();
    cloud.add_file(None, "slow.pdf", "application/pdf", &sample_pdf("Slow"));
    let cache = tempfile::tempdir().unwrap();
    let mut h = harness([Some(cloud.google_settings("ui-cancel")), None], cache.path().to_path_buf());
    connect(&mut h, &cloud);
    wait(&mut h, "the listing", |a| a.cloud.listed_names(ProviderKind::Google) == ["slow.pdf"] && idle(a));
    cloud.slow_down_downloads(1200);
    h.get_by_label("Open slow.pdf").click();
    wait(&mut h, "the download to start", |a| a.cloud.is_busy());
    h.get_by_label_contains("Downloading slow.pdf");
    h.get_by_label("Cancel").click();
    h.run_steps(3);
    assert!(!h.state().cloud.is_busy(), "cancelled");
    std::thread::sleep(Duration::from_millis(1600));
    h.run_steps(4);
    assert!(h.state().views.is_empty(), "the cancelled download did not open");
    assert!(h.state().cloud.notice().is_none());
}

#[test]
fn without_a_client_id_nothing_can_be_connected_and_the_client_is_kept_in_the_keychain() {
    let _gpu = gpu();
    let cloud = FakeCloud::start().unwrap();
    let cache = tempfile::tempdir().unwrap();
    // No override: the window reads the saved client, and there is none.
    let mut h = harness([None, None], cache.path().to_path_buf());
    assert!(h.state_mut().execute("cloud.drives"));
    h.run_steps(3);
    h.get_by_label("Enter the client ID of your own app below.");
    h.get_by_label("Client ID").focus();
    h.run_steps(1);
    h.get_by_label("Client ID").type_text("my-desktop-client.apps.example");
    h.run_steps(3);
    h.get_by_label("Save settings").click();
    h.run_steps(3);
    assert_eq!(load_client(ProviderKind::Google).unwrap().client_id, "my-desktop-client.apps.example");
    assert!(cloud.requests().is_empty(), "saving a client contacts nobody");
    assert!(!h.state().cloud.is_connected(ProviderKind::Google));
    pdfcraft_clouddrive::save_client(ProviderKind::Google, &ProviderSettings::default()).unwrap();
}

#[test]
fn a_document_that_did_not_come_from_a_drive_saves_as_a_copy() {
    let _gpu = gpu();
    let cloud = FakeCloud::start().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let mut h = harness([Some(cloud.google_settings("ui-copy")), None], cache.path().to_path_buf());
    h.state_mut().open_bytes("mine.pdf", None, sample_pdf("Mine")).unwrap();
    h.run_steps(4);
    connect(&mut h, &cloud);
    assert!(h.state_mut().execute("cloud.save"));
    h.run_steps(3);
    h.get_by_label_contains("Save a copy to Google Drive");
    wait(&mut h, "the root listing", idle);
    h.get_by_label("Save here").click();
    wait(&mut h, "the copy", idle);
    let stored = cloud.find("mine.pdf").expect("uploaded");
    assert!(stored.bytes.starts_with(b"%PDF-") && stored.parent.is_none());
}
