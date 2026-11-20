//! The Library window: the documents the owner keeps in a synced folder (iCloud Drive's
//! `PdfCraft/` by default), searched by title, tag and text, and the encrypted vault in that
//! folder's `Vault/` subfolder.
//!
//! Everything slow runs on a worker thread and comes back as a [`Done`] polled each frame: reading
//! the index and the text of every document, adding a file (reading its text), the Argon2id key
//! derivation and Touch ID prompt of an unlock, and encrypting a document. Only the vault session
//! (the unlocked key) and the lists the window shows live on the interface thread.
//!
//! The vault opens a document from memory (`open_bytes` with no path) and marks it
//! `keep_off_disk`, so crash recovery never writes its plain content; Save on such a document
//! encrypts it again ([`PdfCraftApp::save_vault_document`]). The vault locks on demand, on quit and
//! after the idle time (no pointer or key input; 15 minutes unless changed): locking closes the
//! vault documents that have no unsaved changes and keeps the others open.
//!
//! Desktop only: the web build has no folder to sync.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use egui::{Align, Layout, RichText};
use pdfcraft_engine::DocId;
use pdfcraft_library::folder::default_library_dir;
use pdfcraft_library::vault::{KeyCache, check_passphrase, default_cache, open_from_cache};
use pdfcraft_library::{AddOptions, KdfParams, Library, Mode, Snapshot, Vault, VaultAddOptions, VaultEntry, VaultSession, VaultSnapshot};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::theme::{self, Tokens};
use crate::{PdfCraftApp, icons, panels::human_size, widgets};

/// The most results the window lists.
const MAX_ROWS: usize = 300;
/// The idle lock when nothing was chosen.
const DEFAULT_IDLE_MINUTES: u32 = 15;
/// The longest idle lock that can be set, in minutes (a day).
const MAX_IDLE_MINUTES: u32 = 24 * 60;

/// One line of the window: a library document or a vault document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub vault: bool,
    pub title: String,
    pub pages: u32,
    pub size: u64,
    pub tags: Vec<String>,
    pub opened_ms: i64,
    pub added_ms: i64,
    /// The file is not on this Mac (library documents only).
    pub missing: bool,
    /// Where the file is on this Mac (library documents only).
    pub path: Option<PathBuf>,
    /// The text around the first hit of the search, when the text matched.
    pub snippet: Option<String>,
}

/// An original file the person may delete now that its encrypted copy is verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDelete {
    pub path: PathBuf,
    pub title: String,
}

/// What a worker reports of a document it encrypted.
struct VaultAddedInfo {
    title: String,
    /// The source file, when it is what was encrypted and may be offered for deletion.
    original: Option<PathBuf>,
}

/// What a worker thread came back with.
enum Done {
    Loaded {
        snapshot: Result<Arc<Snapshot>, String>,
        vault: Option<Result<Arc<VaultSnapshot>, String>>,
    },
    Rows {
        generation: u64,
        rows: Vec<Row>,
    },
    Added(Result<pdfcraft_library::Added, String>),
    VaultAdded {
        result: Result<VaultAddedInfo, String>,
    },
    /// A best-effort note (a document was opened) that needs no more than its own completion.
    Noted,
    VaultUnlocked {
        result: Result<Vault, String>,
        remember: bool,
        created: bool,
    },
    VaultOpened(Result<(Zeroizing<Vec<u8>>, VaultEntry), String>),
    /// Something finished that changes what is listed: say it, then read the folder again.
    Changed(String),
    Failed(String),
}

/// The window's state.
pub struct LibraryUi {
    pub open: bool,
    folder: Option<PathBuf>,
    /// "Add current document" copies the file into the library folder (else it is referenced).
    pub copy_into_library: bool,
    /// Lock the vault after this many idle minutes; 0 never.
    pub idle_minutes: u32,
    /// Offer to keep the vault key on this Mac behind Touch ID when unlocking.
    pub remember_key: bool,
    cache: Arc<dyn KeyCache>,
    /// Key derivation cost of a new vault. Tests lower it; the window never does.
    pub kdf: KdfParams,
    session: Option<VaultSession>,
    snapshot: Option<Arc<Snapshot>>,
    vault_snapshot: Option<Arc<VaultSnapshot>>,
    pub rows: Vec<Row>,
    pub tags: Vec<(String, usize)>,
    pub warnings: Vec<String>,
    pub query: String,
    pub tag: Option<String>,
    pub missing_only: bool,
    search_generation: u64,
    busy: usize,
    /// The last thing the window had to say.
    pub notice: Option<String>,
    pub passphrase: Zeroizing<String>,
    pub confirm: Zeroizing<String>,
    /// Documents open in a tab that came from the vault, by vault document id.
    vault_docs: HashMap<DocId, String>,
    pub pending_delete: Option<PendingDelete>,
    confirm_remove: Option<(String, bool)>,
    /// The row whose tags are being edited (id, vault, the text so far, still to take the keyboard).
    tag_edit: Option<(String, bool, String, bool)>,
    tx: Sender<Done>,
    rx: Receiver<Done>,
    started: Instant,
    last_input: Duration,
    loaded: bool,
    /// The folder was read (or tried) since it was chosen: an error does not make the window try
    /// again every frame.
    attempted: bool,
    /// Height the vault section took last frame, so the list leaves it room.
    vault_height: f32,
    /// The vault's state as last read, and when: reading it opens `vault.json` and asks the
    /// keychain, so the window does that every couple of seconds, not every frame.
    status: Option<(Instant, pdfcraft_library::vault::Status)>,
}

impl Default for LibraryUi {
    fn default() -> Self {
        let (tx, rx) = channel();
        LibraryUi {
            open: false,
            folder: None,
            copy_into_library: true,
            idle_minutes: DEFAULT_IDLE_MINUTES,
            remember_key: true,
            cache: Arc::from(default_cache()),
            kdf: KdfParams::DEFAULT,
            session: None,
            snapshot: None,
            vault_snapshot: None,
            rows: Vec::new(),
            tags: Vec::new(),
            warnings: Vec::new(),
            query: String::new(),
            tag: None,
            missing_only: false,
            search_generation: 0,
            busy: 0,
            notice: None,
            passphrase: Zeroizing::new(String::new()),
            confirm: Zeroizing::new(String::new()),
            vault_docs: HashMap::new(),
            pending_delete: None,
            confirm_remove: None,
            tag_edit: None,
            tx,
            rx,
            started: Instant::now(),
            last_input: Duration::ZERO,
            loaded: false,
            attempted: false,
            vault_height: 230.0,
            status: None,
        }
    }
}

impl LibraryUi {
    /// The preferences' part: the folder, the idle lock and the two choices. Never a key.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "folder": self.folder.as_ref().map(|p| p.to_string_lossy()),
            "copy": self.copy_into_library,
            "idle_minutes": self.idle_minutes,
            "remember_key": self.remember_key,
        })
    }

    /// Reads [`Self::to_json`]; anything missing or unusable keeps what is set.
    pub(crate) fn restore(&mut self, v: &Value) {
        if let Some(folder) = v["folder"].as_str().filter(|f| !f.is_empty() && std::path::Path::new(f).is_absolute()) {
            self.folder = Some(PathBuf::from(folder));
        }
        if let Some(on) = v["copy"].as_bool() {
            self.copy_into_library = on;
        }
        if let Some(n) = v["idle_minutes"].as_u64() {
            self.idle_minutes = u32::try_from(n).unwrap_or(MAX_IDLE_MINUTES).min(MAX_IDLE_MINUTES);
        }
        if let Some(on) = v["remember_key"].as_bool() {
            self.remember_key = on;
        }
    }

    /// Chooses the library folder (the control channel and tests); empty restores the default.
    pub fn set_folder(&mut self, folder: &str) -> Result<(), String> {
        let folder = folder.trim();
        if folder.is_empty() {
            self.folder = None;
        } else if std::path::Path::new(folder).is_absolute() {
            self.folder = Some(PathBuf::from(folder));
        } else {
            return Err("the library folder must be an absolute path".to_string());
        }
        self.reset_for_new_folder();
        Ok(())
    }

    /// Where the vault key is remembered; tests pass an in-memory cache so no real keychain is used.
    pub fn set_key_cache(&mut self, cache: Arc<dyn KeyCache>) {
        self.cache = cache;
        self.session = None;
    }

    fn reset_for_new_folder(&mut self) {
        self.session = None;
        self.status = None;
        self.snapshot = None;
        self.vault_snapshot = None;
        self.rows.clear();
        self.tags.clear();
        self.loaded = false;
        self.attempted = false;
    }

    /// The library folder in use: the chosen one, else the default.
    pub fn root(&self) -> Option<PathBuf> {
        self.folder.clone().or_else(|| default_library_dir().map(|(dir, _)| dir))
    }

    /// Whether the default folder is in iCloud Drive (and none was chosen).
    fn in_icloud(&self) -> bool {
        self.folder.is_none() && default_library_dir().is_some_and(|(_, icloud)| icloud)
    }

    fn idle(&self) -> Option<Duration> {
        (self.idle_minutes > 0).then(|| Duration::from_secs(u64::from(self.idle_minutes) * 60))
    }

    /// The vault session for the folder in use, created locked on first use.
    fn session(&mut self) -> Option<&mut VaultSession> {
        let root = self.root()?;
        let (cache, idle) = (self.cache.clone(), self.idle());
        let session = self.session.get_or_insert_with(|| VaultSession::new(root.clone(), cache));
        if session.root() != root {
            session.set_root(root);
        }
        session.set_idle_timeout(idle);
        Some(session)
    }

    fn now(&self) -> Duration {
        self.started.elapsed()
    }

    /// The vault's state, read again when it is older than a couple of seconds or after a change.
    fn vault_status(&mut self) -> Option<pdfcraft_library::vault::Status> {
        if let Some((at, status)) = &self.status
            && at.elapsed() < Duration::from_secs(2)
        {
            return Some(status.clone());
        }
        let status = self.session()?.status();
        self.status = Some((Instant::now(), status.clone()));
        Some(status)
    }

    /// The vault is unlocked.
    pub fn is_unlocked(&self) -> bool {
        self.session.as_ref().is_some_and(VaultSession::is_unlocked)
    }

    /// A copy of the unlocked vault for a worker.
    fn vault_for_worker(&self) -> Option<Vault> {
        self.session.as_ref().and_then(|s| s.peek().cloned())
    }

    /// Moves the idle clock on by `by`, as if that long had passed with no input. For tests and
    /// the control channel; real time is never changed.
    pub fn advance_clock(&mut self, by: Duration) {
        if let Some(earlier) = self.started.checked_sub(by) {
            self.started = earlier;
        }
    }

    /// Work is in progress (the window shows a spinner).
    pub fn is_busy(&self) -> bool {
        self.busy > 0
    }

    /// The vault documents open in tabs.
    pub fn vault_documents(&self) -> impl Iterator<Item = (&DocId, &String)> {
        self.vault_docs.iter()
    }

    /// Whether `doc` is a tab opened from the vault.
    pub fn is_vault_document(&self, doc: DocId) -> bool {
        self.vault_docs.contains_key(&doc)
    }

    /// The listed document titled `title`, for tests and the control channel.
    pub fn row_titled(&self, title: &str) -> Option<&Row> {
        self.rows.iter().find(|r| r.title == title)
    }
}

/// What the window asked for this frame.
enum Action {
    Reload,
    Search,
    ChooseFolder,
    AddCurrent,
    Open(usize),
    Reveal(usize),
    Remove(usize),
    SetTags(usize, String),
    VaultCreate,
    VaultUnlock,
    VaultUnlockWithCache,
    VaultLock,
    VaultForgetKey,
    VaultAddCurrent,
    DeleteOriginal,
    KeepOriginal,
    IdleChanged,
}

/// The kind of file the active tab is, for the buttons that add it.
struct ActiveFile {
    /// The tab has a document with a file on disk and no vault binding.
    has_file: bool,
    /// The tab has any document (the vault can encrypt one without a file).
    has_document: bool,
}

fn date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(String::new, |d| d.format("%Y-%m-%d").to_string())
}

fn error_color() -> egui::Color32 {
    egui::Color32::from_rgb(0xD1, 0x3B, 0x3B)
}

impl PdfCraftApp {
    /// Opens the Library window and reads the folder.
    pub fn open_library(&mut self) {
        self.library_ui.open = true;
        self.library_reload();
    }

    /// Reads the index, the text and (when unlocked) the vault again, off the interface thread.
    pub fn library_reload(&mut self) {
        let Some(root) = self.library_ui.root() else {
            self.library_ui.notice = Some(tl!("There is no folder for the library: choose one.").to_string());
            return;
        };
        self.library_ui.session();
        self.library_ui.attempted = true;
        let vault = self.library_ui.vault_for_worker();
        self.library_job(move || {
            let snapshot = Library::open(&root).snapshot().map(Arc::new).map_err(|e| e.to_string());
            let vault = vault.map(|v| v.snapshot().map(Arc::new).map_err(|e| e.to_string()));
            Done::Loaded { snapshot, vault }
        });
    }

    /// Searches the loaded lists again (the query, the tag or the missing filter changed).
    pub fn library_search(&mut self) {
        let ui = &mut self.library_ui;
        ui.search_generation = ui.search_generation.wrapping_add(1);
        let generation = ui.search_generation;
        let (Some(root), snapshot) = (ui.root(), ui.snapshot.clone()) else { return };
        let vault_snapshot = ui.is_unlocked().then(|| ui.vault_snapshot.clone()).flatten();
        let (query, tag, missing_only) = (ui.query.clone(), ui.tag.clone(), ui.missing_only);
        self.library_job(move || {
            let tags: Vec<String> = tag.into_iter().collect();
            let lib = Library::open(&root);
            let mut scored: Vec<(u32, Row)> = Vec::new();
            if let Some(snapshot) = &snapshot {
                for hit in snapshot.search(&query, &tags, usize::MAX) {
                    let e = hit.entry;
                    let path = lib.resolve(&e);
                    let missing = !path.is_file();
                    if missing_only && !missing {
                        continue;
                    }
                    scored.push((
                        hit.score,
                        Row {
                            id: e.id,
                            vault: false,
                            title: e.title,
                            pages: e.pages,
                            size: e.size,
                            tags: e.tags,
                            opened_ms: e.opened_ms,
                            added_ms: e.added_ms,
                            missing,
                            path: Some(path),
                            snippet: hit.snippet,
                        },
                    ));
                }
            }
            if let Some(vs) = vault_snapshot.filter(|_| !missing_only) {
                for hit in vs.search(&query, &tags, usize::MAX) {
                    let e = hit.entry;
                    scored.push((
                        hit.score,
                        Row {
                            id: e.id,
                            vault: true,
                            title: e.title,
                            pages: e.pages,
                            size: e.size,
                            tags: e.tags,
                            opened_ms: e.opened_ms,
                            added_ms: e.added_ms,
                            missing: false,
                            path: None,
                            snippet: hit.snippet,
                        },
                    ));
                }
            }
            scored.sort_by_key(|a| std::cmp::Reverse(a.0));
            scored.truncate(MAX_ROWS);
            Done::Rows { generation, rows: scored.into_iter().map(|(_, r)| r).collect() }
        });
    }

    /// Runs `work` on a worker thread (inline under test) and takes its result in `poll_library`.
    /// A panic inside the work is caught and reported, never taking the app down.
    fn library_job(&mut self, work: impl FnOnce() -> Done + Send + 'static) {
        self.library_ui.busy += 1;
        let tx = self.library_ui.tx.clone();
        let ctx = self.ctx.clone();
        let run = move || {
            let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or_else(|_| Done::Failed("the library work stopped unexpectedly".to_string()));
            let _ = tx.send(done);
            if let Some(ctx) = &ctx {
                ctx.request_repaint();
            }
        };
        if self.run_inline {
            run();
        } else if let Err(e) = std::thread::Builder::new().name("pdfcraft-library".into()).spawn(run) {
            self.library_ui.busy -= 1;
            self.notify_error(e);
        }
    }

    /// Takes what the workers finished, locks an idle vault and forgets closed vault tabs. Called
    /// every frame by `logic`.
    pub(crate) fn poll_library(&mut self, ctx: &egui::Context) {
        while let Ok(done) = self.library_ui.rx.try_recv() {
            self.library_ui.busy = self.library_ui.busy.saturating_sub(1);
            self.apply_library_result(done);
        }
        let open_docs: Vec<DocId> = self.views.iter().map(|v| v.id).collect();
        self.library_ui.vault_docs.retain(|doc, _| open_docs.contains(doc));
        let now = self.library_ui.now();
        // Any key or pointer input counts as use of the app, and so keeps the vault unlocked.
        if ctx.input(|i| !i.events.is_empty() || i.pointer.is_moving()) {
            self.library_ui.last_input = now;
        }
        if self.library_ui.is_unlocked() {
            let idle = self.library_ui.idle();
            let since = now.saturating_sub(self.library_ui.last_input);
            if idle.is_some_and(|limit| since >= limit) {
                self.lock_vault();
                self.notify_tr("The vault locked after being idle");
            } else if let Some(limit) = idle {
                ctx.request_repaint_after(limit.saturating_sub(since).max(Duration::from_millis(250)).min(Duration::from_secs(30)));
            }
        }
        if self.library_ui.busy > 0 {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn apply_library_result(&mut self, done: Done) {
        match done {
            Done::Loaded { snapshot, vault } => {
                match snapshot {
                    Ok(s) => {
                        self.library_ui.tags = s.tag_counts();
                        self.library_ui.warnings = s.warnings.clone();
                        self.library_ui.snapshot = Some(s);
                        self.library_ui.loaded = true;
                    }
                    Err(why) => {
                        self.library_ui.snapshot = None;
                        self.library_ui.rows.clear();
                        self.library_ui.notice = Some(why);
                    }
                }
                match vault {
                    Some(Ok(v)) => {
                        self.library_ui.warnings.extend(v.warnings.iter().cloned());
                        self.library_ui.vault_snapshot = Some(v);
                    }
                    Some(Err(why)) => {
                        self.library_ui.vault_snapshot = None;
                        self.library_ui.notice = Some(why);
                    }
                    None => self.library_ui.vault_snapshot = None,
                }
                self.library_search();
            }
            Done::Rows { generation, rows } => {
                if generation == self.library_ui.search_generation {
                    self.library_ui.rows = rows;
                }
            }
            Done::Added(result) => match result {
                Ok(added) => {
                    let mut message = crate::i18n::fmt(
                        if added.created { tl!("Added {title} to the library") } else { tl!("Updated {title} in the library") },
                        &[("title", &added.entry.title)],
                    );
                    if let Some(note) = added.note {
                        message = format!("{message}. {note}");
                    }
                    self.library_ui.notice = Some(message);
                    self.library_reload();
                }
                Err(why) => self.library_ui.notice = Some(why),
            },
            Done::VaultAdded { result } => match result {
                Ok(info) => {
                    self.library_ui.notice =
                        Some(crate::i18n::fmt(tl!("Encrypted {title} into the vault. It was read back and matches."), &[("title", &info.title)]));
                    if let Some(path) = info.original {
                        self.library_ui.pending_delete = Some(PendingDelete { path, title: info.title });
                    }
                    self.library_reload();
                }
                Err(why) => self.library_ui.notice = Some(why),
            },
            Done::VaultUnlocked { result, remember, created } => {
                let now = self.library_ui.now();
                match result {
                    Ok(vault) => {
                        self.library_ui.status = None;
                        self.library_ui.passphrase = Zeroizing::new(String::new());
                        self.library_ui.confirm = Zeroizing::new(String::new());
                        self.library_ui.last_input = now;
                        let adopted = self.library_ui.session().map(|s| s.adopt(vault, remember, now));
                        self.library_ui.notice = match adopted {
                            Some(Ok(Some(why))) => {
                                Some(crate::i18n::fmt(tl!("Unlocked. Touch ID could not be set up: {why}"), &[("why", &why.to_string())]))
                            }
                            Some(Ok(None)) => {
                                Some(if created { tl!("Vault created and unlocked").to_string() } else { tl!("Vault unlocked").to_string() })
                            }
                            Some(Err(e)) => Some(e.to_string()),
                            None => Some(tl!("There is no folder for the library: choose one.").to_string()),
                        };
                        self.library_reload();
                    }
                    Err(why) => self.library_ui.notice = Some(why),
                }
            }
            Done::VaultOpened(result) => match result {
                Ok((bytes, entry)) => self.open_vault_bytes(bytes, entry),
                Err(why) => self.library_ui.notice = Some(why),
            },
            Done::Noted => {}
            Done::Changed(message) => {
                self.library_ui.notice = Some(message);
                self.library_reload();
            }
            Done::Failed(why) => self.library_ui.notice = Some(why),
        }
    }

    // ---- actions ---------------------------------------------------------------------------

    /// The tab's document and file, for the Add buttons.
    fn active_file(&self) -> ActiveFile {
        let doc = self.active_ids().and_then(|(_, id)| self.session.get(id));
        ActiveFile { has_file: doc.is_some_and(|d| d.path.is_some() && !self.library_ui.vault_docs.contains_key(&d.id)), has_document: doc.is_some() }
    }

    /// Adds the active document's file to the library.
    pub fn library_add_active(&mut self) {
        let Some((_, id)) = self.active_ids() else { return };
        let Some(doc) = self.session.get(id) else { return };
        if self.library_ui.vault_docs.contains_key(&id) {
            self.notify_tr("A vault document is not added to the library: that would copy it out of the vault");
            return;
        }
        let Some(path) = doc.path.clone().map(PathBuf::from).filter(|p| p.is_file()) else {
            self.notify_tr("Save the document first: the library keeps files");
            return;
        };
        let title = doc.info.title.as_deref().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
        let password = doc.password.clone();
        let Some(root) = self.library_ui.root() else {
            self.library_ui.notice = Some(tl!("There is no folder for the library: choose one.").to_string());
            return;
        };
        let mode = if self.library_ui.copy_into_library { Mode::Copy } else { Mode::Reference };
        self.library_ui.open = true;
        self.library_job(move || {
            let options = AddOptions { mode, title, tags: Vec::new(), password };
            Done::Added(Library::open(&root).add_file(&path, &options).map_err(|e| e.to_string()))
        });
    }

    /// Opens a library row in a tab, or a vault row from memory.
    fn library_open_row(&mut self, row: &Row) {
        if row.vault {
            self.vault_open(&row.id);
            return;
        }
        let Some(path) = row.path.clone() else { return };
        if !path.is_file() {
            self.library_ui.notice = Some(crate::i18n::fmt(
                tl!("{title} is not on this Mac. Its file was {path}"),
                &[("title", &row.title), ("path", &path.to_string_lossy())],
            ));
            return;
        }
        self.open_recent(&path.to_string_lossy());
        if let Some(root) = self.library_ui.root() {
            let id = row.id.clone();
            // Noting the open is best effort: the folder may be read-only or syncing.
            self.library_job(move || {
                let _ = Library::open(&root).mark_opened(&id);
                Done::Noted
            });
        }
    }

    /// Opens a vault document in a tab, decrypted into memory only.
    pub fn vault_open(&mut self, id: &str) {
        let Some(vault) = self.library_ui.vault_for_worker() else {
            self.library_ui.notice = Some(tl!("The vault is locked").to_string());
            return;
        };
        let now = self.library_ui.now();
        self.library_ui.last_input = now;
        let id = id.to_string();
        self.library_job(move || {
            let read = vault.read(&id).map_err(|e| e.to_string());
            let entry = vault
                .snapshot()
                .map_err(|e| e.to_string())
                .and_then(|s| s.get(&id).cloned().ok_or_else(|| "that document is not in the vault".to_string()));
            let _ = vault.mark_opened(&id);
            Done::VaultOpened(read.and_then(|bytes| entry.map(|e| (bytes, e))))
        });
    }

    /// Opens decrypted `bytes` as a tab with no path, kept off the disk, bound to its vault entry.
    fn open_vault_bytes(&mut self, bytes: Zeroizing<Vec<u8>>, entry: VaultEntry) {
        let name = if entry.name.is_empty() { format!("{}.pdf", entry.title) } else { entry.name.clone() };
        let before = self.views.len();
        if let Err(e) = self.open_bytes(&name, None, bytes.to_vec()) {
            self.notify_fmt("Couldn't open {name}: {e}", &[("name", &name), ("e", &e)]);
            return;
        }
        drop(bytes);
        // A document with a password of its own would wait for it with nothing to bind yet, and
        // opening it afterwards would not be kept off the disk: refuse it.
        if self.password_prompt.is_some() {
            self.password_prompt = None;
            self.library_ui.notice = Some(tl!("That document has a password of its own. The vault opens documents without one.").to_string());
            return;
        }
        if self.views.len() > before
            && let Some(id) = self.views.last().map(|v| v.id)
        {
            self.session.set_keep_off_disk(id, true);
            self.library_ui.vault_docs.insert(id, entry.id);
        }
    }

    /// Saves a tab that came from the vault by encrypting it again. Returns whether it was saved;
    /// the vault being locked opens the Library so it can be unlocked, and nothing is lost.
    pub(crate) fn save_vault_document(&mut self, doc: DocId) -> bool {
        let Some(vault_id) = self.library_ui.vault_docs.get(&doc).cloned() else { return false };
        let Some(vault) = self.library_ui.vault_for_worker() else {
            self.library_ui.open = true;
            self.notify_tr("The vault is locked. Unlock it in the Library to save this document.");
            return false;
        };
        let bytes = match self.session.save_bytes(doc) {
            Ok(b) => b,
            Err(e) => {
                self.notify_error(e);
                return false;
            }
        };
        if let Err(e) = self.session.check_save(doc, &bytes) {
            self.notify_error(e);
            return false;
        }
        let password = self.session.get(doc).and_then(|d| d.password.clone());
        match vault.replace(&vault_id, &bytes, password.as_deref()) {
            Ok(_) => {
                if let Err(e) = self.session.mark_saved(doc, bytes, None) {
                    self.notify_error(e);
                    return false;
                }
                if let Some(d) = self.session.get(doc)
                    && let Some(view) = self.views.iter_mut().find(|v| v.id == doc)
                {
                    view.document_changed(&d.info);
                }
                self.library_ui.last_input = self.library_ui.now();
                self.notify_tr("Saved to the vault (encrypted)");
                true
            }
            Err(e) => {
                self.notify_error(e);
                false
            }
        }
    }

    /// Locks the vault: the key is wiped, and the vault documents open in tabs are closed unless
    /// they have unsaved changes (those stay, and saving them needs an unlock).
    pub fn lock_vault(&mut self) {
        self.library_ui.status = None;
        if let Some(session) = self.library_ui.session.as_mut() {
            session.lock();
        }
        self.library_ui.vault_snapshot = None;
        let docs: Vec<DocId> = self.library_ui.vault_docs.keys().copied().collect();
        let mut kept = 0;
        for doc in docs {
            if self.session.get(doc).is_some_and(|d| d.dirty) {
                kept += 1;
                continue;
            }
            if let Some(index) = self.views.iter().position(|v| v.id == doc) {
                self.close_tab(index);
            }
            self.library_ui.vault_docs.remove(&doc);
        }
        if kept > 0 {
            self.notify_tr("The vault is locked. Documents with unsaved changes stay open; unlock the vault to save them.");
        }
        if self.library_ui.loaded {
            self.library_search();
        }
    }

    /// Encrypts a copy of the active document into the vault.
    pub fn vault_add_active(&mut self) {
        let Some((_, id)) = self.active_ids() else { return };
        let Some(vault) = self.library_ui.vault_for_worker() else {
            self.library_ui.open = true;
            self.library_ui.notice = Some(tl!("The vault is locked").to_string());
            return;
        };
        let Some(doc) = self.session.get(id) else { return };
        let (name, password, dirty) = (doc.name.clone(), doc.password.clone(), doc.dirty);
        let title = doc.info.title.as_deref().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
        // The original is only offered for deletion when it is what was encrypted.
        let original = doc.path.clone().map(PathBuf::from).filter(|p| p.is_file() && !dirty && !self.library_ui.vault_docs.contains_key(&id));
        let bytes = match self.session.save_bytes(id) {
            Ok(b) => b,
            Err(e) => {
                self.notify_error(e);
                return;
            }
        };
        self.library_ui.last_input = self.library_ui.now();
        self.library_job(move || {
            let options = VaultAddOptions { name, title, tags: Vec::new(), password };
            match vault.add(&bytes, &options) {
                Ok(added) if added.verified => Done::VaultAdded { result: Ok(VaultAddedInfo { title: added.entry.title, original }) },
                Ok(_) => Done::Failed("the encrypted copy could not be verified".to_string()),
                Err(e) => Done::Failed(e.to_string()),
            }
        });
    }

    fn library_action(&mut self, action: Action) {
        let now = self.library_ui.now();
        self.library_ui.last_input = now;
        match action {
            Action::Reload => self.library_reload(),
            Action::Search => self.library_search(),
            Action::ChooseFolder => {
                let dialog = rfd::AsyncFileDialog::new().set_title(tl!("Choose the library folder"));
                self.ask_one(crate::pickers::Ask::Folder(dialog), None, |app, dir| {
                    app.library_ui.folder = Some(dir);
                    app.library_ui.reset_for_new_folder();
                    app.library_reload();
                });
            }
            Action::AddCurrent => self.library_add_active(),
            Action::Open(i) => {
                if let Some(row) = self.library_ui.rows.get(i).cloned() {
                    self.library_open_row(&row);
                }
            }
            Action::Reveal(i) => {
                if let Some(path) = self.library_ui.rows.get(i).and_then(|r| r.path.clone()) {
                    // A missing file reveals the folder it should be in.
                    let target = if path.exists() { path } else { path.parent().map(std::path::Path::to_path_buf).unwrap_or(path) };
                    self.reveal_path(&target);
                }
            }
            Action::Remove(i) => self.library_remove_row(i),
            Action::SetTags(i, text) => self.library_set_tags(i, &text),
            Action::VaultCreate => self.vault_create_now(),
            Action::VaultUnlock => self.vault_unlock_now(),
            Action::VaultUnlockWithCache => self.vault_unlock_with_cache(),
            Action::VaultLock => self.lock_vault(),
            Action::VaultForgetKey => {
                self.library_ui.status = None;
                let result = self.library_ui.session().map(|s| s.forget_cached_key());
                self.library_ui.notice = Some(match result {
                    Some(Ok(())) => tl!("This Mac no longer remembers the vault key").to_string(),
                    Some(Err(e)) => e.to_string(),
                    None => tl!("There is no folder for the library: choose one.").to_string(),
                });
            }
            Action::VaultAddCurrent => self.vault_add_active(),
            Action::DeleteOriginal => {
                if let Some(pending) = self.library_ui.pending_delete.take() {
                    self.library_ui.notice = Some(match std::fs::remove_file(&pending.path) {
                        Ok(()) => crate::i18n::fmt(tl!("Deleted the original of {title}"), &[("title", &pending.title)]),
                        Err(e) => crate::i18n::fmt(tl!("Couldn't delete the original: {e}"), &[("e", &e.to_string())]),
                    });
                }
            }
            Action::KeepOriginal => self.library_ui.pending_delete = None,
            Action::IdleChanged => {
                let idle = self.library_ui.idle();
                if let Some(s) = self.library_ui.session.as_mut() {
                    s.set_idle_timeout(idle);
                }
            }
        }
    }

    fn library_remove_row(&mut self, i: usize) {
        let Some(row) = self.library_ui.rows.get(i).cloned() else { return };
        self.library_ui.confirm_remove = None;
        if row.vault {
            let Some(vault) = self.library_ui.vault_for_worker() else { return };
            // An open tab of the removed document is no longer the vault's to save.
            self.library_ui.vault_docs.retain(|_, v| v != &row.id);
            self.library_job(move || match vault.remove(&row.id) {
                Ok(e) => Done::Changed(crate::i18n::fmt(tl!("Removed {title} from the vault"), &[("title", &e.title)])),
                Err(e) => Done::Failed(e.to_string()),
            });
        } else if let Some(root) = self.library_ui.root() {
            self.library_job(move || match Library::open(&root).remove(&row.id) {
                Ok((e, _)) => Done::Changed(crate::i18n::fmt(tl!("Removed {title} from the library"), &[("title", &e.title)])),
                Err(e) => Done::Failed(e.to_string()),
            });
        }
    }

    fn library_set_tags(&mut self, i: usize, text: &str) {
        let Some(row) = self.library_ui.rows.get(i).cloned() else { return };
        let wanted = pdfcraft_library::index::clean_tags(text.split(',').map(str::trim));
        let remove: Vec<String> = row.tags.iter().filter(|t| !wanted.iter().any(|w| w.eq_ignore_ascii_case(t))).cloned().collect();
        let add: Vec<String> = wanted;
        if row.vault {
            let Some(vault) = self.library_ui.vault_for_worker() else { return };
            self.library_job(move || match vault.retag(&row.id, &add, &remove) {
                Ok(_) => Done::Changed(tl!("Tags saved").to_string()),
                Err(e) => Done::Failed(e.to_string()),
            });
        } else if let Some(root) = self.library_ui.root() {
            self.library_job(move || match Library::open(&root).retag(&row.id, &add, &remove) {
                Ok(_) => Done::Changed(tl!("Tags saved").to_string()),
                Err(e) => Done::Failed(e.to_string()),
            });
        }
    }

    fn vault_create_now(&mut self) {
        let Some(root) = self.library_ui.root() else { return };
        let (pass, confirm) = (self.library_ui.passphrase.clone(), self.library_ui.confirm.clone());
        if *pass != *confirm {
            self.library_ui.notice = Some(tl!("The two passphrases are not the same").to_string());
            return;
        }
        if let Err(e) = check_passphrase(&pass) {
            self.library_ui.notice = Some(e.to_string());
            return;
        }
        let (kdf, remember) = (self.library_ui.kdf, self.library_ui.remember_key);
        self.library_job(move || Done::VaultUnlocked {
            result: Vault::create(&root, &pass, kdf).map_err(|e| e.to_string()),
            remember,
            created: true,
        });
    }

    fn vault_unlock_now(&mut self) {
        let Some(root) = self.library_ui.root() else { return };
        let pass = self.library_ui.passphrase.clone();
        let remember = self.library_ui.remember_key;
        self.library_job(move || Done::VaultUnlocked { result: Vault::unlock(&root, &pass).map_err(|e| e.to_string()), remember, created: false });
    }

    fn vault_unlock_with_cache(&mut self) {
        let Some(root) = self.library_ui.root() else { return };
        let Some(cache) = self.library_ui.session().map(|s| s.cache()) else { return };
        self.library_job(move || Done::VaultUnlocked {
            result: open_from_cache(&root, cache.as_ref()).map_err(|e| e.to_string()),
            remember: false,
            created: false,
        });
    }

    /// Locks the vault on quit: the key is wiped from memory. The key cached for Touch ID stays.
    pub(crate) fn library_quit(&mut self) {
        if let Some(session) = self.library_ui.session.as_mut() {
            session.lock();
        }
    }

    // ---- the window ------------------------------------------------------------------------

    /// Draws the Library window when it is open.
    pub(crate) fn show_library_window(&mut self, ctx: &egui::Context) {
        if !self.library_ui.open {
            return;
        }
        let t = Tokens::get(ctx);
        let active = self.active_file();
        let mut open = true;
        let mut actions: Vec<Action> = Vec::new();
        egui::Window::new(tl!("Library"))
            .id(egui::Id::unique("library-window"))
            .open(&mut open)
            .default_size(egui::vec2(780.0, 680.0))
            .min_size(egui::vec2(520.0, 420.0))
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| window_body(ui, &t, &mut self.library_ui, &active, &mut actions));
        self.library_ui.open = open;
        for action in actions {
            self.library_action(action);
        }
        // The first time it is shown with nothing read yet.
        if self.library_ui.open && !self.library_ui.attempted && self.library_ui.busy == 0 {
            self.library_reload();
        }
    }
}

fn window_body(ui: &mut egui::Ui, t: &Tokens, state: &mut LibraryUi, active: &ActiveFile, actions: &mut Vec<Action>) {
    // The folder.
    ui.horizontal(|ui| {
        let folder = state.root().map_or_else(|| tl!("No folder").to_string(), |p| p.to_string_lossy().into_owned());
        ui.add(egui::Label::new(RichText::new(folder).color(t.text_muted).small()).truncate());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if icons::button(ui, "rotate-cw", 24.0, false, tl!("Read the folder again")).clicked() {
                actions.push(Action::Reload);
            }
            if ui.small_button(tl!("Folder…")).clicked() {
                actions.push(Action::ChooseFolder);
            }
            if state.is_busy() {
                ui.add(egui::Spinner::new().size(14.0));
            }
        });
    });
    if state.in_icloud() {
        ui.label(RichText::new(tl!("In iCloud Drive: shared with your other Macs")).color(t.text_muted).small());
    }
    if let Some(notice) = state.notice.clone() {
        ui.add(egui::Label::new(RichText::new(notice).color(t.text)).wrap());
    }
    for warning in state.warnings.iter().take(3) {
        ui.add(egui::Label::new(RichText::new(warning).color(error_color()).small()).wrap());
    }
    ui.add_space(6.0);

    // Search and filters.
    ui.horizontal(|ui| {
        let edit =
            egui::TextEdit::singleline(&mut state.query).desired_width(ui.available_width() - 150.0).hint_text(tl!("Search titles, tags and text"));
        if ui.add(edit).changed() {
            actions.push(Action::Search);
        }
        if ui.add(widgets::checkbox(&mut state.missing_only, tl!("Missing files"))).changed() {
            actions.push(Action::Search);
        }
    });
    if !state.tags.is_empty() {
        ui.horizontal_wrapped(|ui| {
            let all = state.tag.is_none();
            if ui.selectable_label(all, tl!("All")).clicked() && !all {
                state.tag = None;
                actions.push(Action::Search);
            }
            for (tag, count) in state.tags.clone() {
                let selected = state.tag.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(&tag));
                if ui.selectable_label(selected, format!("{tag} ({count})")).clicked() {
                    state.tag = if selected { None } else { Some(tag) };
                    actions.push(Action::Search);
                }
            }
        });
    }
    ui.add_space(4.0);

    // Adding.
    ui.horizontal(|ui| {
        if ui.add_enabled(active.has_file, egui::Button::new(tl!("Add current document"))).clicked() {
            actions.push(Action::AddCurrent);
        }
        ui.add(widgets::checkbox(&mut state.copy_into_library, tl!("Copy into the library folder")));
    });
    ui.separator();

    // The list takes the room the vault section leaves (as it was last frame).
    let height = (ui.available_height() - state.vault_height - 16.0).max(100.0);
    egui::ScrollArea::vertical().id_salt("library-rows").max_height(height).auto_shrink([false, false]).show(ui, |ui| {
        if state.rows.is_empty() {
            let text = if state.loaded { tl!("No documents match") } else { tl!("Reading the library…") };
            ui.label(RichText::new(text).color(t.text_muted));
        }
        for i in 0..state.rows.len() {
            let Some(row) = state.rows.get(i).cloned() else { break };
            row_ui(ui, t, state, i, &row, actions);
            ui.add_space(4.0);
        }
    });
    ui.separator();
    let used = ui.scope(|ui| vault_section(ui, t, state, active, actions)).response.rect.height();
    state.vault_height = used;
}

fn row_ui(ui: &mut egui::Ui, t: &Tokens, state: &mut LibraryUi, i: usize, row: &Row, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        if row.vault {
            let (icon_rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
            icons::paint(ui, icon_rect, "lock", 14.0, t.icon);
        }
        let title = if row.title.is_empty() { tl!("Untitled").to_string() } else { row.title.clone() };
        if ui.link(RichText::new(title).font(theme::semibold(13.5))).on_hover_text(tl!("Open")).clicked() {
            actions.push(Action::Open(i));
        }
        if row.missing {
            ui.label(RichText::new(tl!("Missing")).color(error_color()).small());
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let confirming = state.confirm_remove.as_ref().is_some_and(|(id, vault)| id == &row.id && *vault == row.vault);
            if confirming {
                if ui.small_button(tl!("Cancel")).clicked() {
                    state.confirm_remove = None;
                }
                if ui.small_button(tl!("Remove")).clicked() {
                    actions.push(Action::Remove(i));
                }
            } else if icons::button(ui, "x", 22.0, false, tl!("Remove from the library")).clicked() {
                state.confirm_remove = Some((row.id.clone(), row.vault));
            }
            if !row.vault && icons::button(ui, "folder-open", 22.0, false, tl!("Show in Finder")).clicked() {
                actions.push(Action::Reveal(i));
            }
            if icons::button(ui, "tag", 22.0, false, tl!("Edit tags")).clicked() {
                state.tag_edit = Some((row.id.clone(), row.vault, row.tags.join(", "), true));
            }
        });
    });
    let mut facts: Vec<String> = Vec::new();
    if row.pages > 0 {
        facts.push(crate::i18n::fmt(tl!("{n} pages"), &[("n", &row.pages.to_string())]));
    }
    facts.push(human_size(usize::try_from(row.size).unwrap_or(usize::MAX)));
    let when = date(row.opened_ms.max(row.added_ms));
    if !when.is_empty() {
        facts.push(when);
    }
    if !row.tags.is_empty() {
        facts.push(row.tags.join(", "));
    }
    ui.label(RichText::new(facts.join("  ·  ")).color(t.text_muted).small());
    if let Some(snippet) = &row.snippet {
        ui.add(egui::Label::new(RichText::new(snippet).color(t.text_faint).small().italics()).wrap());
    }
    if let Some((id, vault, draft, focus)) = state.tag_edit.as_mut()
        && *id == row.id
        && *vault == row.vault
    {
        let mut done = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            let label = ui.label(tl!("Tags"));
            let edit = ui.add(egui::TextEdit::singleline(draft).desired_width(260.0).hint_text(tl!("separated by commas"))).labelled_by(label.id);
            if std::mem::take(focus) {
                edit.request_focus();
            }
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                done = true;
            }
            if ui.small_button(tl!("Save")).clicked() {
                done = true;
            }
            if ui.small_button(tl!("Cancel")).clicked() {
                cancel = true;
            }
        });
        if done {
            actions.push(Action::SetTags(i, draft.clone()));
        }
        if done || cancel {
            state.tag_edit = None;
        }
    }
}

fn vault_section(ui: &mut egui::Ui, t: &Tokens, state: &mut LibraryUi, active: &ActiveFile, actions: &mut Vec<Action>) {
    ui.label(RichText::new(tl!("Vault")).font(theme::semibold(14.0)));
    let status = state.vault_status();
    let Some(status) = status else {
        ui.label(RichText::new(tl!("There is no folder for the library: choose one.")).color(t.text_muted));
        return;
    };
    // An original whose encrypted copy was verified.
    if let Some(pending) = state.pending_delete.clone() {
        ui.add(
            egui::Label::new(
                RichText::new(crate::i18n::fmt(
                    tl!("The encrypted copy of {title} was read back and matches. Delete the original file?"),
                    &[("title", &pending.title)],
                ))
                .color(t.text),
            )
            .wrap(),
        );
        ui.horizontal(|ui| {
            if widgets::pill_button(ui, tl!("Delete original"), false).clicked() {
                actions.push(Action::DeleteOriginal);
            }
            if widgets::pill_button(ui, tl!("Keep it"), true).clicked() {
                actions.push(Action::KeepOriginal);
            }
        });
    }
    if !status.exists {
        ui.label(
            RichText::new(tl!("Encrypted documents, unlocked with a passphrase. It cannot be recovered if you forget it."))
                .color(t.text_muted)
                .small(),
        );
        passphrase_field(ui, &mut state.passphrase, tl!("Passphrase"));
        passphrase_field(ui, &mut state.confirm, tl!("Repeat passphrase"));
        if status.cache_supported {
            ui.add(widgets::checkbox(&mut state.remember_key, crate::i18n::fmt(tl!("Unlock with {how} on this Mac"), &[("how", status.cache_name)])));
        }
        let ready = !state.passphrase.is_empty() && !state.confirm.is_empty() && !state.is_busy();
        if ui.add_enabled(ready, egui::Button::new(tl!("Create vault"))).clicked() {
            actions.push(Action::VaultCreate);
        }
    } else if !status.unlocked {
        ui.horizontal(|ui| {
            let field = passphrase_field_inline(ui, &mut state.passphrase, tl!("Passphrase"));
            let ready = !state.passphrase.is_empty() && !state.is_busy();
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.add_enabled(ready, egui::Button::new(tl!("Unlock"))).clicked() || (enter && ready)) && ready {
                actions.push(Action::VaultUnlock);
            }
            if status.cached
                && ui
                    .add_enabled(!state.is_busy(), egui::Button::new(crate::i18n::fmt(tl!("Unlock with {how}"), &[("how", status.cache_name)])))
                    .clicked()
            {
                actions.push(Action::VaultUnlockWithCache);
            }
        });
        if status.cache_supported {
            ui.add(widgets::checkbox(
                &mut state.remember_key,
                crate::i18n::fmt(tl!("Unlock with {how} on this Mac next time"), &[("how", status.cache_name)]),
            ));
        }
    } else {
        ui.horizontal(|ui| {
            if ui.button(tl!("Lock")).clicked() {
                actions.push(Action::VaultLock);
            }
            if ui.add_enabled(active.has_document, egui::Button::new(tl!("Add current document to the vault"))).clicked() {
                actions.push(Action::VaultAddCurrent);
            }
            if status.cached && ui.button(tl!("Forget the key on this Mac")).clicked() {
                actions.push(Action::VaultForgetKey);
            }
        });
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new(tl!("Lock after")).color(t.text_muted).small());
        let before = state.idle_minutes;
        ui.add(egui::DragValue::new(&mut state.idle_minutes).range(0..=MAX_IDLE_MINUTES).suffix(tl!(" min idle")));
        if state.idle_minutes != before {
            actions.push(Action::IdleChanged);
        }
        if state.idle_minutes == 0 {
            ui.label(RichText::new(tl!("never")).color(t.text_muted).small());
        }
    });
}

fn passphrase_field(ui: &mut egui::Ui, text: &mut Zeroizing<String>, label: &str) {
    ui.horizontal(|ui| {
        passphrase_field_inline(ui, text, label);
    });
}

fn passphrase_field_inline(ui: &mut egui::Ui, text: &mut Zeroizing<String>, label: &str) -> egui::Response {
    let name = ui.label(label);
    ui.add(egui::TextEdit::singleline(&mut **text).password(true).desired_width(220.0)).labelled_by(name.id)
}
