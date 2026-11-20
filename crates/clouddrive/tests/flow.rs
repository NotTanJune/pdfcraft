//! Connecting, browsing, downloading, uploading and disconnecting, against a server on 127.0.0.1
//! that speaks the OAuth, Google Drive and Microsoft Graph endpoints (no real network).

use std::time::Duration;

use pdfcraft_assist::secret;
use pdfcraft_clouddrive::mock::FakeCloud;
use pdfcraft_clouddrive::{CancelToken, CloudError, Connection, ProviderKind, Query};

const PDF: &[u8] = b"%PDF-1.7\n% a file on the drive\n";

fn cloud() -> FakeCloud {
    secret::keep_in_memory_only();
    FakeCloud::start().unwrap()
}

fn connect(cloud: &FakeCloud, kind: ProviderKind, profile: &str) -> Connection {
    let settings = match kind {
        ProviderKind::Google => cloud.google_settings(profile),
        ProviderKind::Microsoft => cloud.microsoft_settings(profile),
    };
    let connection = Connection::new(kind, &settings).unwrap();
    connection.connect(&cloud.browser(), &CancelToken::new(), Duration::from_secs(20)).unwrap();
    connection
}

fn names(items: &[pdfcraft_clouddrive::Item]) -> Vec<&str> {
    items.iter().map(|i| i.name.as_str()).collect()
}

fn no_progress() -> impl FnMut(pdfcraft_clouddrive::Progress) {
    |_| {}
}

#[test]
fn google_connect_uses_pkce_and_keeps_only_the_refresh_token_in_the_keychain() {
    let cloud = cloud();
    cloud.require_client_secret("google-secret-1");
    let settings = cloud.google_settings("g-connect");
    let before = Connection::status(ProviderKind::Google, &settings).unwrap();
    assert!(before.configured && before.account.is_none());

    let connection = Connection::new(ProviderKind::Google, &settings).unwrap();
    let account = connection.connect(&cloud.browser(), &CancelToken::new(), Duration::from_secs(20)).unwrap();
    assert_eq!(account.email.as_deref(), Some("tester@example.com"));
    assert_eq!(account.name.as_deref(), Some("Test User"));

    // The sign-in address had PKCE, a state and the loopback redirect; the token request had the verifier and the secret.
    let authorize = cloud.requests_to("/authorize");
    assert_eq!(authorize.len(), 1);
    assert!(authorize[0].path.contains("code_challenge_method=S256") && authorize[0].path.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A"));
    assert!(!authorize[0].path.contains("secret"), "the client secret never goes in the browser's address");
    let token = String::from_utf8(cloud.requests_to("/token")[0].body.clone()).unwrap();
    assert!(token.contains("grant_type=authorization_code") && token.contains("code_verifier=") && token.contains("client_secret=google-secret-1"));
    // No token ever appears in an address.
    for request in cloud.requests() {
        assert!(!request.path.contains("at-") && !request.path.contains("rt-"), "{}", request.path);
    }
    // The keychain holds the refresh token and the label, not the access token.
    let saved = secret::load_named("cloud/google.g-connect").unwrap().unwrap();
    assert!(saved.contains("rt-") && saved.contains("tester@example.com") && !saved.contains("at-"), "{}", &*saved);
    let after = Connection::status(ProviderKind::Google, &settings).unwrap();
    assert_eq!(after.account.unwrap().email.as_deref(), Some("tester@example.com"));
}

#[test]
fn google_browse_pages_search_and_the_listed_types() {
    let cloud = cloud();
    cloud.cap_page_size(2);
    let folder = cloud.add_folder(None, "Contracts");
    cloud.add_file(Some(&folder), "lease.pdf", "application/pdf", PDF);
    cloud.add_file(Some(&folder), "notes.md", "text/plain", b"# notes");
    cloud.add_file(Some(&folder), "table.csv", "text/csv", b"a,b\n1,2\n");
    cloud.add_file(Some(&folder), "holiday.mp4", "video/mp4", b"video");
    cloud.add_file(Some(&folder), "readme.txt", "text/plain", b"plain");
    cloud.add_file(None, "Root file.pdf", "application/pdf", PDF);
    cloud.add_file(None, "Planning", "application/vnd.google-apps.document", b"");
    let drive = connect(&cloud, ProviderKind::Google, "g-browse").drive().unwrap();

    let root = drive.list(&Query::default()).unwrap();
    assert_eq!(names(&root.items), ["Contracts", "Planning"], "folders first; the cap of 2 gave a first page; {root:?}");
    assert!(root.items[1].exported_as_pdf);
    let root2 = drive.list(&Query { page: root.next.clone(), ..Query::default() }).unwrap();
    assert_eq!(names(&root2.items), ["Root file.pdf"]);
    assert!(root2.next.is_none());

    let (all, truncated) = pdfcraft_clouddrive::list_all(drive.as_ref(), &Query { folder: Some(folder.clone()), ..Query::default() }).unwrap();
    assert!(!truncated);
    assert_eq!(names(&all), ["lease.pdf", "notes.md", "table.csv"], "only the types PdfCraft opens: the video and the .txt are not listed");

    let found = drive.list(&Query { search: Some("lease".into()), ..Query::default() }).unwrap();
    assert_eq!(names(&found.items), ["lease.pdf"], "a search covers the whole drive");
    let none = drive.list(&Query { search: Some("it's \\ ' or trashed = true".into()), ..Query::default() }).unwrap();
    assert!(none.items.is_empty(), "a quote in a search term cannot widen the query");
}

#[test]
fn google_download_is_bounded_reports_progress_and_exports_documents() {
    let cloud = cloud();
    let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let big = cloud.add_file(None, "big.pdf", "application/pdf", &body);
    let doc = cloud.add_file(None, "Planning", "application/vnd.google-apps.document", b"");
    let drive = connect(&cloud, ProviderKind::Google, "g-download").drive().unwrap();

    let mut seen = Vec::new();
    let got = drive.download(&big, 1 << 20, &CancelToken::new(), &mut |p| seen.push(p)).unwrap();
    assert_eq!(got.bytes, body);
    assert_eq!(got.file_name, "big.pdf");
    assert_eq!(got.item.version.as_deref(), Some("1"));
    assert!(seen.len() > 2 && seen.windows(2).all(|w| w[0].done <= w[1].done), "progress only goes forward");
    assert_eq!(seen.last().map(|p| (p.done, p.total)), Some((300_000, Some(300_000))));

    assert!(matches!(drive.download(&big, 1000, &CancelToken::new(), &mut no_progress()), Err(CloudError::TooLarge { limit: 1000, .. })));
    let media_calls = cloud.requests().iter().filter(|r| r.path.contains("alt=media")).count();
    assert_eq!(media_calls, 1, "the size limit is checked before the bytes are fetched");

    let exported = drive.download(&doc, 1 << 20, &CancelToken::new(), &mut no_progress()).unwrap();
    assert_eq!(exported.file_name, "Planning.pdf");
    assert!(exported.bytes.starts_with(b"%PDF-"));

    let cancel = CancelToken::new();
    let c = cancel.clone();
    let result = drive.download(&big, 1 << 20, &cancel, &mut |p| {
        if p.done > 100_000 {
            c.cancel();
        }
    });
    assert_eq!(result.unwrap_err(), CloudError::Cancelled);
}

#[test]
fn google_uploads_small_and_large_files_and_updates_without_clobbering() {
    let cloud = cloud();
    let folder = cloud.add_folder(None, "Out");
    let existing = cloud.add_file(None, "doc.pdf", "application/pdf", PDF);
    let drive = connect(&cloud, ProviderKind::Google, "g-upload").drive().unwrap();

    // Small: one multipart request into the folder.
    let small = drive.upload_new(Some(&folder), "copy.pdf", b"%PDF-small", &CancelToken::new(), &mut no_progress()).unwrap();
    let stored = cloud.file(&small.id).unwrap();
    assert_eq!((stored.name.as_str(), stored.parent.as_deref(), stored.bytes.as_slice()), ("copy.pdf", Some(folder.as_str()), &b"%PDF-small"[..]));

    // Large: a resumable session in chunks, with progress.
    let large: Vec<u8> = (0..4_500_000u32).map(|i| (i % 253) as u8).collect();
    let mut seen = Vec::new();
    let big = drive.upload_new(None, "large.pdf", &large, &CancelToken::new(), &mut |p| seen.push(p.done)).unwrap();
    assert_eq!(cloud.file(&big.id).unwrap().bytes, large);
    assert!(cloud.requests().iter().filter(|r| r.method == "PUT" && r.path.starts_with("/upload/session/")).count() >= 2, "sent in chunks");
    assert!(seen.windows(2).all(|w| w[0] <= w[1]) && seen.last() == Some(&4_500_000));
    assert!(
        cloud.requests().iter().filter(|r| r.path.starts_with("/upload/session/")).all(|r| r.header("authorization").is_none()),
        "the session address gets no token"
    );

    // Update: refused when the file changed since it was opened; allowed on a match and when forced.
    let opened = drive.stat(&existing).unwrap();
    cloud.edit(&existing, b"%PDF-edited elsewhere");
    let conflict = drive.update(&existing, b"%PDF-mine", opened.version.as_deref(), &CancelToken::new(), &mut no_progress()).unwrap_err();
    assert!(matches!(&conflict, CloudError::Conflict { name, .. } if name == "doc.pdf"), "{conflict:?}");
    assert_eq!(cloud.file(&existing).unwrap().bytes, b"%PDF-edited elsewhere", "nothing was written");
    let current = drive.stat(&existing).unwrap();
    let saved = drive.update(&existing, b"%PDF-mine", current.version.as_deref(), &CancelToken::new(), &mut no_progress()).unwrap();
    assert_eq!(cloud.file(&existing).unwrap().bytes, b"%PDF-mine");
    assert_ne!(saved.version, current.version, "the new version comes back for the next conflict check");
    drive.update(&existing, b"%PDF-forced", None, &CancelToken::new(), &mut no_progress()).unwrap();
    assert_eq!(cloud.file(&existing).unwrap().bytes, b"%PDF-forced");

    // A large replace goes through a session too.
    drive.update(&existing, &large, None, &CancelToken::new(), &mut no_progress()).unwrap();
    assert_eq!(cloud.file(&existing).unwrap().bytes, large);

    // Names are checked.
    assert!(matches!(drive.upload_new(None, "a/b.pdf", b"x", &CancelToken::new(), &mut no_progress()), Err(CloudError::Invalid(_))));
}

#[test]
fn a_google_document_cannot_be_overwritten_with_a_pdf() {
    let cloud = cloud();
    let doc = cloud.add_file(None, "Planning", "application/vnd.google-apps.document", b"");
    let drive = connect(&cloud, ProviderKind::Google, "g-doc").drive().unwrap();
    let error = drive.update(&doc, PDF, None, &CancelToken::new(), &mut no_progress()).unwrap_err();
    assert!(matches!(&error, CloudError::Invalid(m) if m.contains("save a copy")), "{error:?}");
}

#[test]
fn an_expired_access_token_is_refreshed_once_and_the_call_goes_through() {
    let cloud = cloud();
    cloud.add_file(None, "a.pdf", "application/pdf", PDF);
    let connection = connect(&cloud, ProviderKind::Google, "g-refresh");
    let drive = connection.drive().unwrap();
    assert_eq!(drive.list(&Query::default()).unwrap().items.len(), 1);
    let before = cloud.token_requests();
    cloud.expire_access_tokens();
    assert_eq!(drive.list(&Query::default()).unwrap().items.len(), 1);
    assert_eq!(cloud.token_requests(), before + 1, "one refresh");
    let refresh = String::from_utf8(cloud.requests_to("/token").last().unwrap().body.clone()).unwrap();
    assert!(refresh.contains("grant_type=refresh_token"));
    // The next call reuses the new token.
    drive.list(&Query::default()).unwrap();
    assert_eq!(cloud.token_requests(), before + 1);
}

#[test]
fn a_revoked_grant_asks_to_connect_again_and_never_prints_a_token() {
    let cloud = cloud();
    let connection = connect(&cloud, ProviderKind::Google, "g-revoked");
    let drive = connection.drive().unwrap();
    cloud.expire_access_tokens();
    cloud.revoke_all_refresh_tokens();
    let error = drive.list(&Query::default()).unwrap_err();
    assert_eq!(error, CloudError::Unauthenticated("Google Drive"));
    assert!(error.needs_sign_in());
    for text in [error.to_string(), format!("{error:?}")] {
        assert!(!text.contains("rt-") && !text.contains("at-"), "{text}");
    }
}

#[test]
fn disconnecting_revokes_at_google_and_deletes_the_saved_account() {
    let cloud = cloud();
    let settings = cloud.google_settings("g-disconnect");
    let connection = connect(&cloud, ProviderKind::Google, "g-disconnect");
    let refresh = secret::load_named("cloud/google.g-disconnect")
        .unwrap()
        .unwrap()
        .split("\"refresh_token\":\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    let outcome = connection.disconnect().unwrap();
    assert!(outcome.was_connected && outcome.revoked && outcome.can_revoke);
    assert_eq!(cloud.revoked(), vec![refresh]);
    assert!(secret::load_named("cloud/google.g-disconnect").unwrap().is_none());
    assert!(Connection::status(ProviderKind::Google, &settings).unwrap().account.is_none());
    assert_eq!(connection.drive().err(), Some(CloudError::NotConnected("Google Drive")));
    let again = connection.disconnect().unwrap();
    assert!(!again.was_connected, "disconnecting twice is not an error");
}

#[test]
fn refusing_in_the_browser_connects_nothing() {
    let cloud = cloud();
    let settings = cloud.google_settings("g-refuse");
    let connection = Connection::new(ProviderKind::Google, &settings).unwrap();
    let error = connection.connect(&cloud.refusing_browser(), &CancelToken::new(), Duration::from_secs(20)).unwrap_err();
    assert!(matches!(&error, CloudError::SignIn(m) if m.contains("access_denied")), "{error:?}");
    assert!(secret::load_named("cloud/google.g-refuse").unwrap().is_none());
    assert_eq!(cloud.token_requests(), 0, "no code, so no exchange");
}

#[test]
fn no_refresh_token_or_a_refused_client_connects_nothing() {
    let cloud = cloud();
    cloud.withhold_refresh_token(true);
    let connection = Connection::new(ProviderKind::Google, &cloud.google_settings("g-norefresh")).unwrap();
    let error = connection.connect(&cloud.browser(), &CancelToken::new(), Duration::from_secs(20)).unwrap_err();
    assert!(matches!(&error, CloudError::SignIn(m) if m.contains("refresh token")), "{error:?}");
    assert!(secret::load_named("cloud/google.g-norefresh").unwrap().is_none());

    let cloud = self::cloud();
    cloud.require_client_secret("the-real-secret");
    let mut settings = cloud.google_settings("g-badsecret");
    settings.client_secret = "wrong".into();
    let error = Connection::new(ProviderKind::Google, &settings)
        .unwrap()
        .connect(&cloud.browser(), &CancelToken::new(), Duration::from_secs(20))
        .unwrap_err();
    assert!(matches!(&error, CloudError::SignIn(m) if m.contains("client ID or secret")), "{error:?}");
    assert!(!error.to_string().contains("wrong"), "the secret is not echoed");
    assert!(secret::load_named("cloud/google.g-badsecret").unwrap().is_none());
}

#[test]
fn connecting_can_be_cancelled_and_needs_a_client_id() {
    let cloud = cloud();
    let connection = Connection::new(ProviderKind::Microsoft, &cloud.microsoft_settings("m-cancel")).unwrap();
    let cancel = CancelToken::new();
    let c = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        c.cancel();
    });
    let started = std::time::Instant::now();
    let error = connection.connect(&|_| Ok(()), &cancel, Duration::from_secs(60)).unwrap_err();
    assert_eq!(error, CloudError::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(10));
    let none = Connection::new(ProviderKind::Google, &pdfcraft_clouddrive::ProviderSettings::default());
    assert_eq!(none.err(), Some(CloudError::NotConfigured("Google Drive")));
    let failing = connection.connect(&|_| Err("no browser".into()), &CancelToken::new(), Duration::from_secs(5)).unwrap_err();
    assert!(matches!(&failing, CloudError::SignIn(m) if m.contains("could not open your browser")), "{failing:?}");
}

#[test]
fn onedrive_browse_search_and_download_without_the_token_on_the_redirect() {
    let cloud = cloud();
    cloud.cap_page_size(2);
    let folder = cloud.add_folder(None, "Papers");
    cloud.add_file(Some(&folder), "thesis.pdf", "application/pdf", PDF);
    cloud.add_file(Some(&folder), "it's.docx", "application/octet-stream", b"PK");
    cloud.add_file(Some(&folder), "movie.mkv", "video/x-matroska", b"v");
    cloud.add_file(Some(&folder), "a.md", "text/markdown", b"# a");
    let connection = connect(&cloud, ProviderKind::Microsoft, "m-browse");
    let drive = connection.drive().unwrap();

    let root = drive.list(&Query::default()).unwrap();
    assert_eq!(names(&root.items), ["Papers"]);
    let first = drive.list(&Query { folder: Some(folder.clone()), ..Query::default() }).unwrap();
    assert!(first.next.is_some(), "the cap of 2 pages the folder");
    let (all, truncated) = pdfcraft_clouddrive::list_all(drive.as_ref(), &Query { folder: Some(folder.clone()), ..Query::default() }).unwrap();
    assert!(!truncated);
    let mut got = names(&all);
    got.sort();
    assert_eq!(got, ["a.md", "it's.docx", "thesis.pdf"], "the video is not listed");

    let found = drive.list(&Query { search: Some("it's".into()), ..Query::default() }).unwrap();
    assert_eq!(names(&found.items), ["it's.docx"], "a quote in a search term is doubled in the OData literal");

    let thesis = all.iter().find(|i| i.name == "thesis.pdf").unwrap();
    let mut seen = Vec::new();
    let got = drive.download(&thesis.id, 1 << 20, &CancelToken::new(), &mut |p| seen.push(p)).unwrap();
    assert_eq!(got.bytes, PDF);
    assert!(seen.last().is_some_and(|p| p.done == PDF.len() as u64));
    let content = cloud.requests().into_iter().filter(|r| r.path.contains("/content")).collect::<Vec<_>>();
    assert!(content.iter().all(|r| r.header("authorization").is_some()), "the API call carries the token");
    let blob = cloud.requests_to("/blob/");
    assert_eq!(blob.len(), 1);
    assert!(blob[0].header("authorization").is_none(), "the pre-authorized download link never gets the token");

    let account = drive.account().unwrap();
    assert_eq!((account.email.as_deref(), account.name.as_deref()), (Some("tester@example.com"), Some("Test User")));
}

#[test]
fn onedrive_uploads_renames_on_a_clash_and_replaces_with_a_conflict_check() {
    let cloud = cloud();
    cloud.rotate_refresh_tokens(true);
    let existing = cloud.add_file(None, "doc.pdf", "application/pdf", PDF);
    let connection = connect(&cloud, ProviderKind::Microsoft, "m-upload");
    let drive = connection.drive().unwrap();

    let copy = drive.upload_new(None, "doc.pdf", b"%PDF-copy", &CancelToken::new(), &mut no_progress()).unwrap();
    assert_eq!(cloud.file(&copy.id).unwrap().name, "doc (1).pdf", "an existing name is never overwritten by a copy");
    assert_eq!(cloud.file(&existing).unwrap().bytes, PDF);

    let large: Vec<u8> = (0..4_500_000u32).map(|i| (i % 249) as u8).collect();
    let big = drive.upload_new(None, "large.pdf", &large, &CancelToken::new(), &mut no_progress()).unwrap();
    assert_eq!(cloud.file(&big.id).unwrap().bytes, large);
    let chunks: Vec<_> = cloud.requests().into_iter().filter(|r| r.method == "PUT" && r.path.starts_with("/upload/session/")).collect();
    assert!(chunks.len() >= 2 && chunks.iter().all(|r| r.header("authorization").is_none()), "chunks go to the session address without a token");

    let opened = drive.stat(&existing).unwrap();
    cloud.edit(&existing, b"%PDF-edited elsewhere");
    let conflict = drive.update(&existing, b"%PDF-mine", opened.version.as_deref(), &CancelToken::new(), &mut no_progress()).unwrap_err();
    assert!(matches!(conflict, CloudError::Conflict { .. }));
    assert_eq!(cloud.file(&existing).unwrap().bytes, b"%PDF-edited elsewhere");
    let current = drive.stat(&existing).unwrap();
    let saved = drive.update(&existing, b"%PDF-mine", current.version.as_deref(), &CancelToken::new(), &mut no_progress()).unwrap();
    assert_eq!(cloud.file(&existing).unwrap().bytes, b"%PDF-mine");
    assert_ne!(saved.version, current.version);
    drive.update(&existing, &large, None, &CancelToken::new(), &mut no_progress()).unwrap();
    assert_eq!(cloud.file(&existing).unwrap().bytes, large, "a large replace goes through an upload session");
    assert!(matches!(drive.stat("nope"), Err(CloudError::NotFound(_))));

    // Microsoft rotates refresh tokens: after an expiry the new one is saved and still works.
    let old = secret::load_named("cloud/onedrive.m-upload").unwrap().unwrap().to_string();
    cloud.expire_access_tokens();
    drive.list(&Query::default()).unwrap();
    let new = secret::load_named("cloud/onedrive.m-upload").unwrap().unwrap().to_string();
    assert_ne!(old, new, "the rotated refresh token was saved");
    cloud.expire_access_tokens();
    drive.list(&Query::default()).unwrap();
}

#[test]
fn onedrive_disconnect_deletes_tokens_and_says_it_could_not_revoke() {
    let cloud = cloud();
    let connection = connect(&cloud, ProviderKind::Microsoft, "m-disconnect");
    let outcome = connection.disconnect().unwrap();
    assert!(outcome.was_connected && !outcome.revoked && !outcome.can_revoke);
    assert!(cloud.revoked().is_empty());
    assert!(secret::load_named("cloud/onedrive.m-disconnect").unwrap().is_none());
}

#[test]
fn a_next_page_link_to_another_host_is_refused_and_never_contacted() {
    let cloud = cloud();
    cloud.cap_page_size(1);
    cloud.add_file(None, "a.pdf", "application/pdf", PDF);
    cloud.add_file(None, "b.pdf", "application/pdf", PDF);
    cloud.poison_next_links("https://evil.example/v1.0/me/drive/root/children?$skip=1");
    let drive = connect(&cloud, ProviderKind::Microsoft, "m-poison").drive().unwrap();
    let first = drive.list(&Query::default()).unwrap();
    let link = first.next.unwrap();
    assert!(link.starts_with("https://evil.example"));
    let error = drive.list(&Query { page: Some(link), ..Query::default() }).unwrap_err();
    assert!(matches!(&error, CloudError::BadResponse(m) if m.contains("not on the service")), "{error:?}");
}

#[test]
fn the_shared_craft_cloud_contract_works_over_either_drive() {
    use craft_cloud::{Accounts, Files};
    let cloud = cloud();
    let id = cloud.add_file(None, "a.pdf", "application/pdf", PDF);
    for (kind, profile) in [(ProviderKind::Google, "c-google"), (ProviderKind::Microsoft, "c-ms")] {
        let drive = connect(&cloud, kind, profile).drive().unwrap();
        let files = pdfcraft_clouddrive::files::DriveFiles::new(drive.as_ref());
        assert!(files.list("").unwrap().iter().any(|e| e.name == "a.pdf"));
        assert_eq!(files.read(&id).unwrap(), PDF);
        assert_eq!(files.me().unwrap().email.as_deref(), Some("tester@example.com"));
        let entry = files.stat(&id).unwrap();
        let etag = entry.etag.clone().unwrap();
        let written = files.write(&id, b"%PDF-new", Some(&etag)).unwrap();
        assert_eq!(cloud.file(&id).unwrap().bytes, b"%PDF-new");
        let stale = files.write(&id, b"%PDF-stale", Some(&etag)).unwrap_err();
        assert!(matches!(stale, craft_cloud::Error::Conflict(_)), "{stale:?}");
        assert!(written.etag.is_some_and(|e| e != etag));
        cloud.edit(&id, PDF);
        let name = format!("new-{profile}.pdf");
        let created = files.create("", &name, b"%PDF-n").unwrap();
        assert_eq!(created.name, name);
        assert!(files.shared_with_me().unwrap().is_empty());
    }
}
