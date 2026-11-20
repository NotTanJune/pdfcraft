//! Cloud drives: Google Drive and OneDrive in a window (File ▸ Cloud drives…).
//!
//! Off until the owner enters a client ID from their own Google Cloud or Microsoft Entra project and
//! connects an account ([`pdfcraft_clouddrive`]): nothing here touches the network before that, and
//! the client ID and the tokens live in the system keychain, never in the preferences file.
//!
//! - **Connect** opens the provider's sign-in page in the system browser and waits for the redirect
//!   on `127.0.0.1`. **Disconnect** revokes the token where the provider allows it and deletes the
//!   saved account.
//! - The **browser** lists a folder (breadcrumb, search by name, "Load more" for the next page).
//!   **Open** downloads a file into the app's cache folder and opens it as File ▸ Open does: a PDF as
//!   it is, Word, Markdown, LaTeX, CSV and Excel files through the in-process conversion, a Google
//!   Docs file as its PDF export.
//! - A PDF opened from a drive remembers its file and version. **Save to drive** saves it as Save
//!   does, then uploads the result as a new revision; if the file changed on the drive since it was
//!   opened, it asks before replacing it. **Save a copy to drive…** picks a folder and a name and
//!   uploads a new file (also for a document that did not come from a drive).
//! - Every transfer runs on its own thread, shows its progress, and can be cancelled.
//!
//! The web build has no network client here: the window says so.

// The web build has no sockets or keychain here: the window says so, and the machinery behind it is native only.
#![cfg_attr(target_arch = "wasm32", allow(dead_code, unused_mut))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use pdfcraft_clouddrive::{
    Account, CancelToken, CloudError, Connection, Item, ListPage, Progress, ProviderKind, ProviderSettings, Query, Status, load_client, save_client,
};
use pdfcraft_engine::DocId;

use crate::theme::Tokens;
use crate::{PdfCraftApp, SaveTarget, widgets};

/// The most files a download's remembered link list holds while their tabs open.
const MAX_AWAITING: usize = 8;

/// The folders on the way to the one shown. The first is the drive's root.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Crumb {
    id: Option<String>,
    name: String,
}

/// Where a document opened from a drive came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteLink {
    pub provider: ProviderKind,
    pub id: String,
    pub name: String,
    /// The drive's version of the file when it was opened or last saved.
    pub version: Option<String>,
    /// A Google Docs file opened as its PDF export: it cannot take a PDF back.
    pub exported: bool,
}

#[derive(Default)]
struct Pane {
    loaded: bool,
    /// What is typed in the settings fields.
    draft: ProviderSettings,
    /// What jobs use: the saved client.
    applied: ProviderSettings,
    show_settings: bool,
    status: Option<Status>,
    path: Vec<Crumb>,
    items: Vec<Item>,
    next: Option<String>,
    listed: bool,
    search_box: String,
    searching: Option<String>,
    error: Option<String>,
}

/// A new file to upload, waiting for a folder and a name.
struct SaveCopy {
    doc: DocId,
    bytes: Arc<Vec<u8>>,
    name: String,
}

/// What an upload in flight was for.
#[derive(Clone)]
struct UploadCtx {
    doc: Option<DocId>,
    link: Option<RemoteLink>,
    bytes: Arc<Vec<u8>>,
    name: String,
    /// A new file in this folder, instead of a new revision of `link`'s file.
    folder: Option<Option<String>>,
}

enum What {
    Connect,
    Disconnect,
    List { replace: bool },
    Download,
    Upload(UploadCtx),
}

enum Event {
    Progress(Progress),
    OpenBrowser(String),
    Done(Box<Done>),
}

enum Done {
    Connected(Account),
    Disconnected { revoked: bool, can_revoke: bool },
    Listed { page: ListPage },
    Downloaded { path: PathBuf, item: Item, file_name: String },
    Uploaded { item: Item },
    Failed(CloudError),
}

struct Job {
    kind: ProviderKind,
    what: What,
    rx: Receiver<Event>,
    cancel: CancelToken,
    progress: Option<Progress>,
    label: String,
}

/// A save back that found the file changed on the drive, waiting for the answer.
struct Conflict {
    upload: UploadCtx,
    name: String,
}

/// The window's state.
#[derive(Default)]
pub struct CloudState {
    pub open: bool,
    /// Settings that replace the saved client (tests, and hosts that bring their own).
    pub overrides: [Option<ProviderSettings>; 2],
    /// Where downloads are kept; the platform's cache folder when `None`.
    pub cache_dir: Option<PathBuf>,
    tab: Option<ProviderKind>,
    panes: [Pane; 2],
    job: Option<Job>,
    notice: Option<String>,
    links: Vec<(DocId, RemoteLink)>,
    awaiting: Vec<(String, RemoteLink)>,
    save: Option<SaveCopy>,
    conflict: Option<Conflict>,
}

fn slot(kind: ProviderKind) -> usize {
    match kind {
        ProviderKind::Google => 0,
        ProviderKind::Microsoft => 1,
    }
}

impl CloudState {
    fn kind(&self) -> ProviderKind {
        self.tab.unwrap_or(ProviderKind::Google)
    }

    fn pane(&mut self, kind: ProviderKind) -> &mut Pane {
        &mut self.panes[slot(kind)]
    }

    /// The settings jobs of `kind` use.
    fn settings(&self, kind: ProviderKind) -> ProviderSettings {
        self.overrides[slot(kind)].clone().unwrap_or_else(|| self.panes[slot(kind)].applied.clone())
    }

    /// Whether an account is connected for `kind` (as last read from the keychain).
    pub fn is_connected(&self, kind: ProviderKind) -> bool {
        self.panes[slot(kind)].status.as_ref().is_some_and(|s| s.account.is_some())
    }

    /// The account label shown for `kind`.
    pub fn account_label(&self, kind: ProviderKind) -> Option<String> {
        self.panes[slot(kind)].status.as_ref().and_then(|s| s.account.as_ref()).map(Account::display)
    }

    /// The names listed in the folder shown for `kind`.
    pub fn listed_names(&self, kind: ProviderKind) -> Vec<String> {
        self.panes[slot(kind)].items.iter().map(|i| i.name.clone()).collect()
    }

    /// What the window last said about a problem.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// A transfer or sign-in is running.
    pub fn is_busy(&self) -> bool {
        self.job.is_some()
    }

    /// Where `doc` came from, when it was opened from a drive.
    pub fn link_of(&self, doc: DocId) -> Option<&RemoteLink> {
        self.links.iter().find(|(d, _)| *d == doc).map(|(_, l)| l)
    }

    /// A save back is waiting for the answer to "the file changed on the drive".
    pub fn has_conflict(&self) -> bool {
        self.conflict.is_some()
    }

    /// Selects the tab of `kind` (tests, and Save a copy on a given drive).
    pub fn select(&mut self, kind: ProviderKind) {
        self.tab = Some(kind);
    }
}

/// What the window asks the app to do.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Tab(ProviderKind),
    SaveSettings,
    Connect,
    Disconnect,
    Enter(usize),
    Crumb(usize),
    Open(usize),
    Search,
    ClearSearch,
    More,
    Refresh,
    Cancel,
    SaveHere,
    CancelSave,
    Overwrite,
    CopyInstead,
    DropConflict,
}

/// What the drive's top folder is called in its own web page.
fn root_name(kind: ProviderKind) -> String {
    match kind {
        ProviderKind::Google => tl!("My Drive").to_string(),
        ProviderKind::Microsoft => tl!("My files").to_string(),
    }
}

fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// `Cloud` in the portable data folder, else the platform's per-user cache folder (as the
/// clipboard copy is kept): `~/Library/Caches/PdfCraft/Cloud` (macOS), `%LOCALAPPDATA%\PdfCraft\Cloud`
/// (Windows), `$XDG_CACHE_HOME/pdfcraft/cloud` or `~/.cache/pdfcraft/cloud` (others). Never the
/// shared temporary folder.
#[cfg(not(target_arch = "wasm32"))]
fn default_cache_dir() -> Option<PathBuf> {
    if let Some(dir) = crate::portable::data_dir() {
        return Some(dir.join("Cloud"));
    }
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from).filter(|p| p.is_absolute());
    if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Caches/PdfCraft/Cloud"))
    } else if cfg!(windows) {
        env("LOCALAPPDATA").map(|d| d.join("PdfCraft").join("Cloud"))
    } else {
        env("XDG_CACHE_HOME").or_else(|| env("HOME").map(|h| h.join(".cache"))).map(|d| d.join("pdfcraft/cloud"))
    }
}

/// Writes a downloaded file under `root/<provider>/<id hash>/<name>`, replacing an earlier copy.
#[cfg(not(target_arch = "wasm32"))]
fn write_cache(root: &Path, kind: ProviderKind, id: &str, file_name: &str, bytes: &[u8]) -> Result<PathBuf, CloudError> {
    use pdfcraft_clouddrive::safe_file_name;
    let io = |e: std::io::Error| CloudError::Io(e.to_string());
    let folder = root.join(kind.id()).join(format!("{:016x}", fnv(id)));
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(&folder).map_err(io)?;
    let path = folder.join(safe_file_name(file_name));
    let staging = folder.join(".download");
    std::fs::write(&staging, bytes).map_err(io)?;
    std::fs::rename(&staging, &path).map_err(|e| {
        let _ = std::fs::remove_file(&staging);
        io(e)
    })?;
    Ok(path)
}

#[cfg(target_arch = "wasm32")]
fn default_cache_dir() -> Option<PathBuf> {
    None
}

#[cfg(target_arch = "wasm32")]
fn write_cache(_root: &Path, _kind: ProviderKind, _id: &str, _file_name: &str, _bytes: &[u8]) -> Result<PathBuf, CloudError> {
    Err(CloudError::Unsupported)
}

impl PdfCraftApp {
    /// File ▸ Cloud drives…: shows the window.
    pub fn open_cloud_drives(&mut self) {
        self.cloud.open = true;
        let kind = self.cloud.kind();
        self.cloud.tab = Some(kind);
        self.cloud_load(kind);
        self.cloud_list_if_needed(kind);
    }

    /// Reads the saved client and account of `kind` (once).
    fn cloud_load(&mut self, kind: ProviderKind) {
        if self.cloud.pane(kind).loaded {
            return;
        }
        let saved = match load_client(kind) {
            Ok(s) => s,
            Err(e) => {
                self.cloud.notice = Some(e.to_string());
                ProviderSettings::default()
            }
        };
        let overridden = self.cloud.overrides[slot(kind)].as_ref().is_some_and(ProviderSettings::is_configured);
        let pane = self.cloud.pane(kind);
        pane.loaded = true;
        pane.draft = saved.clone();
        pane.applied = saved;
        pane.show_settings = !overridden && !pane.applied.is_configured();
        pane.path = vec![Crumb { id: None, name: root_name(kind) }];
        self.cloud_refresh_status(kind);
    }

    fn cloud_refresh_status(&mut self, kind: ProviderKind) {
        let settings = self.cloud.settings(kind);
        match Connection::status(kind, &settings) {
            Ok(status) => self.cloud.pane(kind).status = Some(status),
            Err(e) => self.cloud.notice = Some(e.to_string()),
        }
    }

    fn cloud_list_if_needed(&mut self, kind: ProviderKind) {
        let pane = self.cloud.pane(kind);
        if pane.listed || self.cloud.job.is_some() || !self.cloud.is_connected(kind) {
            return;
        }
        self.cloud_start_list(kind, false);
    }

    /// Runs `work` on its own thread; its events come back through `cloud_poll`.
    #[cfg(not(target_arch = "wasm32"))]
    fn cloud_spawn<F>(&mut self, kind: ProviderKind, what: What, label: String, work: F)
    where
        F: FnOnce(&dyn Fn(Event), &CancelToken) -> Done + Send + 'static,
    {
        // A listing in flight gives way to the next request; anything else must finish or be cancelled.
        if let Some(job) = self.cloud.job.take() {
            if matches!(job.what, What::List { .. }) {
                job.cancel.cancel();
            } else {
                self.cloud.job = Some(job);
                return;
            }
        }
        let cancel = CancelToken::new();
        let (tx, rx): (std::sync::mpsc::Sender<Event>, Receiver<Event>) = std::sync::mpsc::channel();
        let (worker_cancel, repaint) = (cancel.clone(), self.ctx.clone());
        let spawned = std::thread::Builder::new().name("pdfcraft-cloud".into()).spawn(move || {
            let send = |event: Event| {
                let _ = tx.send(event);
                if let Some(ctx) = &repaint {
                    ctx.request_repaint();
                }
            };
            let done = match pdfcraft_engine::guard(|| work(&send, &worker_cancel)) {
                Ok(done) => done,
                Err(_) => Done::Failed(CloudError::Network("the request failed unexpectedly".into())),
            };
            send(Event::Done(Box::new(done)));
        });
        if spawned.is_err() {
            self.cloud.notice = Some(tl!("Couldn't start the request.").to_string());
            return;
        }
        self.cloud.notice = None;
        self.cloud.job = Some(Job { kind, what, rx, cancel, progress: None, label });
    }

    #[cfg(target_arch = "wasm32")]
    fn cloud_spawn<F>(&mut self, _kind: ProviderKind, _what: What, _label: String, _work: F)
    where
        F: FnOnce(&dyn Fn(Event), &CancelToken) -> Done + Send + 'static,
    {
        self.cloud.notice = Some(CloudError::Unsupported.to_string());
    }

    fn cloud_start_connect(&mut self, kind: ProviderKind) {
        let settings = self.cloud.settings(kind);
        let label = crate::i18n::fmt(tl!("Signing in to {drive}…"), &[("drive", kind.label())]);
        self.cloud_spawn(kind, What::Connect, label, move |send, cancel| {
            let connection = match Connection::new(kind, &settings) {
                Ok(c) => c,
                Err(e) => return Done::Failed(e),
            };
            let open = |address: &str| {
                send(Event::OpenBrowser(address.to_string()));
                Ok(())
            };
            match connection.connect(&open, cancel, pdfcraft_clouddrive::SIGN_IN_TIMEOUT) {
                Ok(account) => Done::Connected(account),
                Err(e) => Done::Failed(e),
            }
        });
    }

    fn cloud_start_disconnect(&mut self, kind: ProviderKind) {
        let settings = self.cloud.settings(kind);
        let label = crate::i18n::fmt(tl!("Disconnecting {drive}…"), &[("drive", kind.label())]);
        self.cloud_spawn(kind, What::Disconnect, label, move |_, _| match Connection::new(kind, &settings).and_then(|c| c.disconnect()) {
            Ok(out) => Done::Disconnected { revoked: out.revoked, can_revoke: out.can_revoke },
            Err(e) => Done::Failed(e),
        });
    }

    /// Loads the folder shown for `kind`: its first page, or (with `more`) the page after the last.
    fn cloud_start_list(&mut self, kind: ProviderKind, more: bool) {
        let settings = self.cloud.settings(kind);
        let pane = self.cloud.pane(kind);
        let query = Query {
            folder: pane.path.last().and_then(|c| c.id.clone()),
            search: pane.searching.clone(),
            page: if more { pane.next.clone() } else { None },
            limit: 0,
        };
        if !more {
            pane.items.clear();
            pane.next = None;
            pane.error = None;
        }
        pane.listed = true;
        self.cloud_spawn(kind, What::List { replace: !more }, tl!("Loading…").to_string(), move |_, _| {
            match Connection::new(kind, &settings).and_then(|c| c.drive()).and_then(|d| d.list(&query)) {
                Ok(page) => Done::Listed { page },
                Err(e) => Done::Failed(e),
            }
        });
    }

    fn cloud_start_download(&mut self, kind: ProviderKind, item: Item) {
        let settings = self.cloud.settings(kind);
        let Some(root) = self.cloud.cache_dir.clone().or_else(default_cache_dir) else {
            self.cloud.notice = Some(tl!("There is no folder to keep downloads in.").to_string());
            return;
        };
        let label = crate::i18n::fmt(tl!("Downloading {name}…"), &[("name", &item.name)]);
        self.cloud_spawn(kind, What::Download, label, move |send, cancel| {
            let result = (|| {
                let drive = Connection::new(kind, &settings)?.drive()?;
                let got = drive.download(&item.id, pdfcraft_clouddrive::MAX_DOWNLOAD_BYTES, cancel, &mut |p| send(Event::Progress(p)))?;
                let path = write_cache(&root, kind, &got.item.id, &got.file_name, &got.bytes)?;
                Ok::<_, CloudError>(Done::Downloaded { path, item: got.item, file_name: got.file_name })
            })();
            result.unwrap_or_else(Done::Failed)
        });
    }

    /// Uploads `ctx.bytes`: a new revision of `ctx.link`'s file, or a new file in `ctx.folder`.
    /// `expected` is the version the new revision is based on (`None` replaces regardless).
    fn cloud_start_upload(&mut self, kind: ProviderKind, upload: UploadCtx, expected: Option<String>) {
        let settings = self.cloud.settings(kind);
        let label = crate::i18n::fmt(tl!("Uploading {name}…"), &[("name", &upload.name)]);
        let job_upload = upload.clone();
        self.cloud_spawn(kind, What::Upload(job_upload), label, move |send, cancel| {
            let result = (|| {
                let drive = Connection::new(kind, &settings)?.drive()?;
                let mut progress = |p: Progress| send(Event::Progress(p));
                let item = match (&upload.folder, &upload.link) {
                    (Some(folder), _) => drive.upload_new(folder.as_deref(), &upload.name, &upload.bytes, cancel, &mut progress)?,
                    (None, Some(link)) => drive.update(&link.id, &upload.bytes, expected.as_deref(), cancel, &mut progress)?,
                    (None, None) => return Err(CloudError::Invalid("nothing to upload to".into())),
                };
                Ok::<_, CloudError>(Done::Uploaded { item })
            })();
            result.unwrap_or_else(Done::Failed)
        });
    }

    /// File ▸ Save to drive. A document opened from a drive saves as Save does, then uploads as a
    /// new revision; any other document is saved as a copy.
    pub fn cloud_save_active(&mut self) {
        let Some(index) = self.active else { return };
        let Some(id) = self.views.get(index).map(|v| v.id) else { return };
        let Some(link) = self.cloud.link_of(id).cloned() else { return self.cloud_save_copy_active() };
        if link.exported {
            self.notify_tr("A Google Docs file cannot take a PDF as its content, so a copy is saved instead.");
            return self.cloud_save_copy_active();
        }
        self.save_then(index, SaveTarget::InPlace, move |app| app.cloud_upload_saved(id));
    }

    /// The document is saved to its file: upload that as a new revision.
    fn cloud_upload_saved(&mut self, id: DocId) {
        let Some(link) = self.cloud.link_of(id).cloned() else { return };
        let Some(bytes) = self.session.get(id).map(|d| d.saved_bytes()) else { return };
        let upload = UploadCtx { doc: Some(id), name: link.name.clone(), link: Some(link.clone()), bytes, folder: None };
        let expected = link.version.clone();
        self.cloud_start_upload(link.provider, upload, expected);
    }

    /// File ▸ Save a copy to drive…: picks a folder and a name in the window.
    pub fn cloud_save_copy_active(&mut self) {
        let Some(index) = self.active else { return };
        let Some(id) = self.views.get(index).map(|v| v.id) else { return };
        if !self.commit_typing_in(id) {
            return;
        }
        let bytes = match self.session.save_bytes(id) {
            Ok(b) => b,
            Err(e) => {
                self.notify_fmt(
                    "Couldn't save {name}: {e}",
                    &[("name", &self.session.get(id).map(|d| d.name.clone()).unwrap_or_default()), ("e", &e.to_string())],
                );
                return;
            }
        };
        let name = self.session.get(id).map(|d| d.name.clone()).unwrap_or_else(|| "document".into());
        let name = if name.to_ascii_lowercase().ends_with(".pdf") { name } else { format!("{name}.pdf") };
        self.cloud.save = Some(SaveCopy { doc: id, bytes, name });
        self.open_cloud_drives();
    }

    /// Takes in what the threads have to say. Called every frame.
    fn cloud_poll(&mut self) {
        let mut finished = None;
        let mut browser = Vec::new();
        if let Some(job) = self.cloud.job.as_mut() {
            loop {
                match job.rx.try_recv() {
                    Ok(Event::Progress(p)) => job.progress = Some(p),
                    Ok(Event::OpenBrowser(address)) => browser.push(address),
                    Ok(Event::Done(done)) => {
                        finished = Some(*done);
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = Some(Done::Failed(CloudError::Network("the request stopped".into())));
                        break;
                    }
                }
            }
        }
        for address in browser {
            // The sign-in address: https on the provider's host (or this computer, for tests).
            self.open_url(&address);
        }
        if let Some(done) = finished
            && let Some(job) = self.cloud.job.take()
        {
            self.cloud_finish(job, done);
        }
        self.cloud_resolve_links();
        if self.cloud.job.is_some()
            && let Some(ctx) = &self.ctx
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn cloud_finish(&mut self, job: Job, done: Done) {
        let kind = job.kind;
        match done {
            Done::Connected(account) => {
                self.cloud_refresh_status(kind);
                self.cloud.pane(kind).listed = false;
                self.notify_fmt("Connected {drive} as {who}", &[("drive", kind.label()), ("who", &account.display())]);
                self.cloud_list_if_needed(kind);
            }
            Done::Disconnected { revoked, can_revoke } => {
                self.cloud.links.retain(|(_, l)| l.provider != kind);
                self.cloud.awaiting.retain(|(_, l)| l.provider != kind);
                let pane = self.cloud.pane(kind);
                pane.items.clear();
                pane.next = None;
                pane.listed = false;
                pane.path = vec![Crumb { id: None, name: root_name(kind) }];
                self.cloud_refresh_status(kind);
                let template = if revoked || !can_revoke {
                    "Disconnected {drive}. The saved sign-in was deleted."
                } else {
                    "Disconnected {drive}. The saved sign-in was deleted, but the provider could not be reached to revoke it."
                };
                self.notify_fmt(template, &[("drive", kind.label())]);
            }
            Done::Listed { page } => {
                let replace = matches!(job.what, What::List { replace: true });
                let pane = self.cloud.pane(kind);
                if replace {
                    pane.items.clear();
                }
                pane.items.extend(page.items);
                pane.next = page.next;
                pane.error = None;
            }
            Done::Downloaded { path, item, file_name } => {
                let path_text = path.to_string_lossy().into_owned();
                let is_pdf = file_name.to_ascii_lowercase().ends_with(".pdf");
                if is_pdf {
                    let link = RemoteLink {
                        provider: kind,
                        id: item.id.clone(),
                        name: item.name.clone(),
                        version: item.version.clone(),
                        exported: item.exported_as_pdf,
                    };
                    self.cloud.awaiting.retain(|(p, _)| *p != path_text);
                    self.cloud.awaiting.push((path_text.clone(), link));
                    let extra = self.cloud.awaiting.len().saturating_sub(MAX_AWAITING);
                    self.cloud.awaiting.drain(..extra);
                }
                #[cfg(not(target_arch = "wasm32"))]
                self.open_path(&path_text);
                self.cloud.open = false;
            }
            Done::Uploaded { item } => {
                let What::Upload(upload) = job.what else { return };
                if upload.folder.is_some() {
                    self.cloud.save = None;
                    self.notify_fmt("Saved a copy to {drive}: {name}", &[("drive", kind.label()), ("name", &item.name)]);
                    self.cloud.pane(kind).listed = false;
                    self.cloud_list_if_needed(kind);
                } else {
                    if let Some(doc) = upload.doc
                        && let Some((_, link)) = self.cloud.links.iter_mut().find(|(d, _)| *d == doc)
                    {
                        link.version = item.version.clone();
                    }
                    self.notify_fmt("Saved to {drive}: {name}", &[("drive", kind.label()), ("name", &item.name)]);
                }
            }
            Done::Failed(CloudError::Cancelled) => {}
            Done::Failed(CloudError::Conflict { name, .. }) => {
                if let What::Upload(upload) = job.what {
                    self.cloud.conflict = Some(Conflict { upload, name });
                }
            }
            Done::Failed(error) => {
                if error.needs_sign_in() {
                    self.cloud_refresh_status(kind);
                }
                let text = error.to_string();
                if matches!(job.what, What::List { .. }) {
                    self.cloud.pane(kind).error = Some(text.clone());
                } else {
                    self.cloud.notice = Some(text.clone());
                }
                if !self.cloud.open {
                    self.notify(text);
                }
            }
        }
    }

    /// A downloaded PDF's tab, once it is open, gets the link to its file.
    fn cloud_resolve_links(&mut self) {
        let mut found: Vec<(String, DocId, RemoteLink)> = Vec::new();
        for (path, link) in &self.cloud.awaiting {
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(i) = self.tab_showing(path)
                && let Some(view) = self.views.get(i)
            {
                found.push((path.clone(), view.id, link.clone()));
            }
            #[cfg(target_arch = "wasm32")]
            let _ = (path, link);
        }
        for (path, id, link) in found {
            self.cloud.awaiting.retain(|(p, _)| *p != path);
            self.cloud.links.retain(|(d, _)| *d != id);
            self.cloud.links.push((id, link));
        }
        let open: Vec<DocId> = self.views.iter().map(|v| v.id).collect();
        self.cloud.links.retain(|(d, _)| open.contains(d));
    }

    fn cloud_apply(&mut self, action: Action) {
        let kind = self.cloud.kind();
        match action {
            Action::Tab(k) => {
                self.cloud.tab = Some(k);
                self.cloud_load(k);
                self.cloud_list_if_needed(k);
            }
            Action::SaveSettings => {
                let draft = self.cloud.pane(kind).draft.clone();
                match save_client(kind, &draft) {
                    Ok(()) => {
                        let pane = self.cloud.pane(kind);
                        pane.applied = draft;
                        pane.show_settings = !pane.applied.is_configured();
                        self.cloud.notice = None;
                        self.cloud_refresh_status(kind);
                    }
                    Err(e) => self.cloud.notice = Some(e.to_string()),
                }
            }
            Action::Connect => self.cloud_start_connect(kind),
            Action::Disconnect => self.cloud_start_disconnect(kind),
            Action::Enter(i) => {
                let pane = self.cloud.pane(kind);
                if let Some(item) = pane.items.get(i).filter(|it| it.is_folder).cloned() {
                    pane.searching = None;
                    pane.search_box.clear();
                    pane.path.push(Crumb { id: Some(item.id), name: item.name });
                    self.cloud_start_list(kind, false);
                }
            }
            Action::Crumb(i) => {
                let pane = self.cloud.pane(kind);
                if i < pane.path.len() {
                    pane.path.truncate(i + 1);
                    pane.searching = None;
                    pane.search_box.clear();
                    self.cloud_start_list(kind, false);
                }
            }
            Action::Open(i) => {
                if let Some(item) = self.cloud.pane(kind).items.get(i).filter(|it| !it.is_folder).cloned() {
                    self.cloud_start_download(kind, item);
                }
            }
            Action::Search => {
                let pane = self.cloud.pane(kind);
                let term = pane.search_box.trim().to_string();
                pane.searching = (!term.is_empty()).then_some(term);
                self.cloud_start_list(kind, false);
            }
            Action::ClearSearch => {
                let pane = self.cloud.pane(kind);
                pane.searching = None;
                pane.search_box.clear();
                self.cloud_start_list(kind, false);
            }
            Action::More => self.cloud_start_list(kind, true),
            Action::Refresh => self.cloud_start_list(kind, false),
            Action::Cancel => {
                if let Some(job) = self.cloud.job.take() {
                    job.cancel.cancel();
                }
            }
            Action::SaveHere => {
                let folder = self.cloud.pane(kind).path.last().and_then(|c| c.id.clone());
                if let Some(save) = self.cloud.save.as_ref() {
                    let name = save.name.trim().to_string();
                    let upload = UploadCtx { doc: Some(save.doc), link: None, bytes: save.bytes.clone(), name, folder: Some(folder) };
                    self.cloud_start_upload(kind, upload, None);
                }
            }
            Action::CancelSave => self.cloud.save = None,
            Action::Overwrite => {
                if let Some(conflict) = self.cloud.conflict.take() {
                    let kind = conflict.upload.link.as_ref().map_or(kind, |l| l.provider);
                    self.cloud_start_upload(kind, conflict.upload, None);
                }
            }
            Action::CopyInstead => {
                if let Some(conflict) = self.cloud.conflict.take() {
                    self.cloud.save =
                        Some(SaveCopy { doc: conflict.upload.doc.unwrap_or(DocId(0)), bytes: conflict.upload.bytes, name: conflict.upload.name });
                    self.open_cloud_drives();
                }
            }
            Action::DropConflict => self.cloud.conflict = None,
        }
    }

    /// Draws the window and the question about a changed file, and takes in the threads' news.
    /// Called every frame.
    pub(crate) fn cloud_frame(&mut self, ctx: &egui::Context) {
        self.cloud_poll();
        if self.cloud.conflict.is_some()
            && let Some(action) = conflict_dialog(ctx, &mut self.cloud)
        {
            self.cloud_apply(action);
        }
        if !self.cloud.open {
            return;
        }
        let kind = self.cloud.kind();
        self.cloud_load(kind);
        let t = Tokens::get(ctx);
        let mut open = true;
        let mut action = None;
        egui::Window::new(tl!("Cloud drives"))
            .id(egui::Id::unique("cloud-drives-window"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(egui::vec2(560.0, 520.0))
            .min_width(380.0)
            .default_pos(egui::pos2(160.0, 90.0))
            .show(ctx, |ui| {
                widgets::begin_dialog(ui);
                action = body(ui, &t, &mut self.cloud);
            });
        if !open {
            self.cloud.open = false;
            self.cloud.save = None;
        }
        if let Some(action) = action {
            self.cloud_apply(action);
        }
    }
}

/// The question after a save found the file changed on the drive.
fn conflict_dialog(ctx: &egui::Context, st: &mut CloudState) -> Option<Action> {
    let name = st.conflict.as_ref().map(|c| c.name.clone()).unwrap_or_default();
    let mut action = None;
    widgets::show_modal(ctx, "cloud-conflict", |ui| {
        ui.set_width(380.0);
        ui.label(egui::RichText::new(crate::i18n::fmt(tl!("{name} changed on the drive"), &[("name", &name)])).strong());
        ui.add_space(6.0);
        ui.add(egui::Label::new(tl!("Someone saved a newer version after you opened it. Replacing it loses their changes.")).wrap());
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if widgets::pill_button(ui, tl!("Replace it"), false).clicked() {
                action = Some(Action::Overwrite);
            }
            if widgets::pill_button(ui, tl!("Save a copy instead…"), true).clicked() {
                action = Some(Action::CopyInstead);
            }
            if widgets::pill_button(ui, tl!("Cancel"), false).clicked() {
                action = Some(Action::DropConflict);
            }
        });
    });
    action
}

fn muted(t: &Tokens, text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into()).color(t.text_muted)
}

/// The window's contents.
fn body(ui: &mut egui::Ui, t: &Tokens, st: &mut CloudState) -> Option<Action> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (t, st);
        ui.label(tl!("Cloud drives need the desktop app."));
        return None;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut action = None;
        let kind = st.kind();
        let busy = st.job.as_ref().is_some_and(|j| !matches!(j.what, What::List { .. }));
        let listing = st.job.as_ref().is_some_and(|j| matches!(j.what, What::List { .. }));
        let saving = st.save.is_some();

        ui.horizontal(|ui| {
            for k in ProviderKind::ALL {
                if ui.selectable_label(kind == k, k.label()).clicked() && kind != k && !busy {
                    action = Some(Action::Tab(k));
                }
            }
        });
        ui.separator();

        let connected = st.is_connected(kind);
        let account = st.account_label(kind);
        let has_override = st.overrides[slot(kind)].as_ref().is_some_and(ProviderSettings::is_configured);
        let pane = st.pane(kind);
        let usable = has_override || pane.applied.is_configured();
        let configured = usable || pane.draft.is_configured();

        // Account row.
        ui.horizontal(|ui| match (&account, connected) {
            (Some(who), true) => {
                ui.label(crate::i18n::fmt(tl!("Signed in as {who}"), &[("who", who)]));
                if ui.add_enabled(!busy, egui::Button::new(tl!("Disconnect"))).clicked() {
                    action = Some(Action::Disconnect);
                }
            }
            _ => {
                let can_connect = usable && !busy;
                if ui.add_enabled(can_connect, egui::Button::new(tl!("Connect"))).clicked() {
                    action = Some(Action::Connect);
                }
                if !usable {
                    ui.label(muted(t, tl!("Enter the client ID of your own app below.")));
                }
            }
        });

        // Settings: the owner's own client.
        ui.toggle_value(&mut pane.show_settings, tl!("Settings"));
        if pane.show_settings || !configured {
            egui::Grid::new(("cloud-settings-grid", slot(kind))).num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                let label = ui.label(tl!("Client ID"));
                ui.add(widgets::line(&mut pane.draft.client_id).desired_width(f32::INFINITY)).labelled_by(label.id);
                ui.end_row();
                let default_scopes = pdfcraft_clouddrive::settings::default_scopes(kind);
                let label = ui.label(tl!("Scopes"));
                ui.add(widgets::line(&mut pane.draft.scopes).hint_text(default_scopes).desired_width(f32::INFINITY)).labelled_by(label.id);
                ui.end_row();
                if kind == ProviderKind::Google {
                    let label = ui.label(tl!("Client secret"));
                    ui.add(widgets::line(&mut pane.draft.client_secret).password(true).desired_width(f32::INFINITY)).labelled_by(label.id);
                    ui.end_row();
                } else {
                    let label = ui.label(tl!("Authority"));
                    ui.add(
                        widgets::line(&mut pane.draft.authority)
                            .hint_text(pdfcraft_clouddrive::settings::MICROSOFT_DEFAULT_AUTHORITY)
                            .desired_width(f32::INFINITY),
                    )
                    .labelled_by(label.id);
                    ui.end_row();
                }
            });
            let changed = pane.draft != pane.applied;
            if ui.add_enabled(changed, egui::Button::new(tl!("Save settings"))).clicked() {
                action = Some(Action::SaveSettings);
            }
        }
        ui.add_space(4.0);

        // The save-a-copy bar.
        if saving {
            ui.separator();
            ui.label(egui::RichText::new(crate::i18n::fmt(tl!("Save a copy to {drive}"), &[("drive", kind.label())])).strong());
            ui.horizontal(|ui| {
                let label = ui.label(tl!("Name"));
                if let Some(save) = st.save.as_mut() {
                    ui.add(widgets::line(&mut save.name).desired_width(240.0)).labelled_by(label.id);
                }
                let ok = connected && !busy && st.save.as_ref().is_some_and(|s| !s.name.trim().is_empty());
                if ui.add_enabled(ok, egui::Button::new(tl!("Save here"))).clicked() {
                    action = Some(Action::SaveHere);
                }
                if ui.button(tl!("Cancel")).clicked() {
                    action = Some(Action::CancelSave);
                }
            });
            ui.separator();
        }

        // The browser.
        if connected {
            let pane = st.pane(kind);
            ui.horizontal_wrapped(|ui| {
                let last = pane.path.len().saturating_sub(1);
                for (i, crumb) in pane.path.iter().enumerate() {
                    if i > 0 {
                        ui.label(muted(t, "/"));
                    }
                    if i == last && pane.searching.is_none() {
                        ui.label(egui::RichText::new(&crumb.name).strong());
                    } else if ui.link(&crumb.name).clicked() && !busy {
                        action = Some(Action::Crumb(i));
                    }
                }
                if let Some(term) = &pane.searching {
                    ui.label(muted(t, "/"));
                    ui.label(egui::RichText::new(crate::i18n::fmt(tl!("Search: {term}"), &[("term", term)])).strong());
                }
            });
            ui.horizontal(|ui| {
                let field = ui.add(widgets::line(&mut pane.search_box).hint_text(tl!("Search by name")).desired_width(220.0));
                let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (ui.add_enabled(!busy, egui::Button::new(tl!("Search"))).clicked() || enter) && !busy {
                    action = Some(Action::Search);
                }
                if pane.searching.is_some() && ui.button(tl!("Clear")).clicked() && !busy {
                    action = Some(Action::ClearSearch);
                }
                if ui.add_enabled(!busy, egui::Button::new(tl!("Refresh"))).clicked() {
                    action = Some(Action::Refresh);
                }
            });
            let height = (ui.available_height() - 70.0).max(120.0);
            egui::ScrollArea::vertical().id_salt(("cloud-items", slot(kind))).max_height(height).auto_shrink([false, false]).show(ui, |ui| {
                if pane.items.is_empty() && !listing {
                    match &pane.error {
                        Some(error) => {
                            ui.add(egui::Label::new(egui::RichText::new(error).color(egui::Color32::from_rgb(0xD1, 0x3B, 0x3B))).wrap());
                        }
                        None => {
                            ui.label(muted(t, tl!("Nothing here that PdfCraft can open.")));
                        }
                    }
                }
                for (i, item) in pane.items.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.add(widgets::icons::image(if item.is_folder { "folder" } else { "file-text" }, 14.0, t.icon));
                        if item.is_folder {
                            if ui.link(&item.name).clicked() && !busy {
                                action = Some(Action::Enter(i));
                            }
                        } else {
                            ui.label(&item.name);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let open = ui.add_enabled(!busy, egui::Button::new(tl!("Open")));
                                // Each row's button is named by its file, for screen readers and for tests.
                                open.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::Role::Button,
                                        !busy,
                                        crate::i18n::fmt(tl!("Open {name}"), &[("name", &item.name)]),
                                    )
                                });
                                if open.clicked() {
                                    action = Some(Action::Open(i));
                                }
                                if let Some(size) = item.size {
                                    ui.label(muted(t, human_size(size)));
                                }
                            });
                        }
                    });
                }
                if pane.next.is_some() && ui.add_enabled(!busy && !listing, egui::Button::new(tl!("Load more"))).clicked() {
                    action = Some(Action::More);
                }
            });
        }

        // What is running, and why something failed.
        if let Some(job) = &st.job {
            ui.separator();
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(16.0));
                ui.label(&job.label);
                if let Some(p) = job.progress.filter(|p| p.total.is_some_and(|t| t > 0)) {
                    let fraction = p.total.map_or(0.0, |total| (p.done as f64 / total as f64).clamp(0.0, 1.0)) as f32;
                    ui.add(egui::ProgressBar::new(fraction).desired_width(140.0));
                }
                if ui.button(tl!("Cancel")).clicked() {
                    action = Some(Action::Cancel);
                }
            });
        }
        if let Some(notice) = &st.notice {
            ui.add(egui::Label::new(egui::RichText::new(notice).color(egui::Color32::from_rgb(0xD1, 0x3B, 0x3B))).wrap());
        }
        action
    }
}

/// `1.4 MB`, from bytes.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn the_cache_path_is_per_file_and_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_cache(dir.path(), ProviderKind::Google, "id-1", "../../evil.pdf", b"%PDF-a").unwrap();
        let b = write_cache(dir.path(), ProviderKind::Google, "id-2", "evil.pdf", b"%PDF-b").unwrap();
        assert!(a.starts_with(dir.path().join("google")) && b.starts_with(dir.path().join("google")));
        assert_ne!(a.parent(), b.parent(), "two files with one name stay apart");
        assert_eq!(a.file_name().unwrap(), "_.._evil.pdf");
        assert_eq!(std::fs::read(&a).unwrap(), b"%PDF-a");
        // Downloading again replaces the copy.
        let again = write_cache(dir.path(), ProviderKind::Google, "id-1", "../../evil.pdf", b"%PDF-new").unwrap();
        assert_eq!((again.clone(), std::fs::read(again).unwrap()), (a, b"%PDF-new".to_vec()));
    }
}
