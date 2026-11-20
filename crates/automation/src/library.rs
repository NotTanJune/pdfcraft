//! The document library and the encrypted vault as tools: `library_add`, `library_list`,
//! `library_search`, `library_remove`, `library_open`, `library_tag`, and `vault_create`,
//! `vault_unlock`, `vault_lock`, `vault_add`, `vault_list`, `vault_open`, `vault_save`,
//! `vault_remove` (all in [`pdfcraft_library`]).
//!
//! The library folder is `library` in a call, else the one the host set
//! ([`Automation::with_library_dir`]), else `PDFCRAFT_LIBRARY_DIR`, else the default
//! (iCloud Drive's `PdfCraft/`, or the app's data folder). With `--root` set, a call must name a
//! `library` folder inside the root unless the host set one: the default is outside it.
//!
//! The vault's key lives in memory while the vault is unlocked, in this session only. A document
//! opened from the vault is opened from memory ([`Automation::vault_open`]) with no path, and
//! `doc_save` without a path saves it by encrypting it again ([`Automation::vault_save`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pdfcraft_engine::DocId;
use pdfcraft_library::vault::{KeyCache, check_passphrase, default_cache};
use pdfcraft_library::{AddOptions, Entry, KdfParams, Library, LibraryError, Mode, VaultAddOptions, VaultEntry, VaultSession};
use serde_json::{Value, json};

use crate::json::summary;
use crate::{Args, Automation, Result, ToolError, failed};

/// The environment variable that names the library folder.
pub const LIBRARY_ENV: &str = "PDFCRAFT_LIBRARY_DIR";
/// The environment variable that supplies the vault passphrase without putting it in an argument.
pub const PASSPHRASE_ENV: &str = "PDFCRAFT_VAULT_PASSPHRASE";

/// The most documents `library_list` and `library_search` return.
const MAX_LIST: usize = 1000;

/// What the library and vault tools keep between calls.
pub(crate) struct LibraryState {
    /// The library folder the host chose.
    dir: Option<PathBuf>,
    cache: Arc<dyn KeyCache>,
    kdf: KdfParams,
    idle: Option<Duration>,
    vault: Option<VaultSession>,
    /// Open documents that came from the vault, by the vault document they are.
    bound: HashMap<DocId, String>,
    started: Instant,
}

impl Default for LibraryState {
    fn default() -> Self {
        LibraryState {
            dir: None,
            cache: Arc::from(default_cache()),
            kdf: KdfParams::DEFAULT,
            idle: Some(pdfcraft_library::vault::DEFAULT_IDLE),
            vault: None,
            bound: HashMap::new(),
            started: Instant::now(),
        }
    }
}

/// What `vault_add` encrypts and where it came from.
struct VaultSource {
    bytes: Vec<u8>,
    /// The source file's name.
    name: String,
    /// The title the document gives itself.
    title: Option<String>,
    /// The source file, if it has one.
    file: Option<PathBuf>,
    /// The file is what `bytes` holds: no unsaved changes.
    clean: bool,
}

fn bad(message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs(message.into())
}

fn lib_err(e: LibraryError) -> ToolError {
    failed(e)
}

impl Automation {
    /// Use `dir` as the library folder for calls that name none.
    pub fn with_library_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.library.dir = Some(dir.into());
        self
    }

    /// Where the vault key is remembered after a passphrase unlock (the macOS keychain by
    /// default). Tests pass an in-memory cache so no real keychain is touched.
    pub fn with_vault_key_cache(mut self, cache: Arc<dyn KeyCache>) -> Self {
        self.library.cache = cache;
        self.library.vault = None;
        self
    }

    /// The key-derivation cost new vaults are made with (64 MiB, 3 passes by default). Tests use
    /// [`KdfParams::MIN_STRENGTH`] so they do not wait on the full cost.
    pub fn with_vault_kdf(mut self, params: KdfParams) -> Self {
        self.library.kdf = params;
        self
    }

    /// Lock the vault after this long without a vault tool call (15 minutes by default); `None`
    /// never locks on idle.
    pub fn with_vault_idle_timeout(mut self, idle: Option<Duration>) -> Self {
        self.library.idle = idle;
        self.library.vault = None;
        self
    }

    /// Whether `doc` is a document opened from the vault.
    pub(crate) fn is_vault_document(&self, doc: DocId) -> bool {
        self.library.bound.contains_key(&doc)
    }

    /// Forgets the vault binding of a closed document.
    pub(crate) fn forget_vault_document(&mut self, doc: DocId) {
        self.library.bound.remove(&doc);
    }

    fn since_start(&self) -> Duration {
        self.library.started.elapsed()
    }

    /// The library folder for this call.
    fn library_folder(&self, a: &Args) -> Result<PathBuf> {
        if let Some(path) = a.opt_str("library")? {
            return self.resolve(path, true);
        }
        if let Some(dir) = &self.library.dir {
            return Ok(dir.clone());
        }
        if let Some(env) = std::env::var_os(LIBRARY_ENV).filter(|v| !v.is_empty()) {
            let env = env.to_string_lossy().into_owned();
            return self.resolve(&env, true);
        }
        if self.root.is_some() {
            return Err(bad("pass `library`: a folder inside --root (the default library folder is outside it)"));
        }
        pdfcraft_library::folder::default_library_dir()
            .map(|(dir, _)| dir)
            .ok_or_else(|| bad("there is no home folder to put the library in: pass `library`"))
    }

    fn library_for(&self, a: &Args) -> Result<Library> {
        Ok(Library::open(self.library_folder(a)?))
    }

    /// The vault session of the library folder `dir`, created locked on first use.
    fn vault_session(&mut self, dir: &Path) -> &mut VaultSession {
        let (cache, idle) = (self.library.cache.clone(), self.library.idle);
        let session = self.library.vault.get_or_insert_with(|| {
            let mut session = VaultSession::new(dir, cache);
            session.set_idle_timeout(idle);
            session
        });
        if session.root() != dir {
            session.set_root(dir);
        }
        session
    }

    // ---- library -------------------------------------------------------------------------

    fn entry_json(lib: &Library, e: &Entry) -> Value {
        let kind = match &e.location {
            pdfcraft_library::Location::Library(_) => "library",
            pdfcraft_library::Location::External(_) => "external",
        };
        json!({
            "id": e.id, "title": e.title, "pages": e.pages, "tags": e.tags,
            "added_ms": e.added_ms, "opened_ms": e.opened_ms, "modified_ms": e.modified_ms,
            "size": e.size, "hash": e.hash,
            "location": { "kind": kind, "path": e.location.path() },
            "path": lib.resolve(e).to_string_lossy(),
            "missing": lib.is_missing(e),
        })
    }

    /// The file a `path` or `doc` argument names, with the title the document gives itself.
    fn source_file(&self, a: &Args) -> Result<(PathBuf, Option<String>, Option<String>)> {
        match (a.opt_str("path")?, a.opt_int("doc")?) {
            (Some(_), Some(_)) => Err(bad("pass `path` or `doc`, not both")),
            (Some(path), None) => Ok((self.resolve(path, false)?, None, None)),
            (None, Some(_)) => {
                let doc = self.doc(a)?;
                let path = doc.path.clone().ok_or_else(|| failed("that document has no file yet: save it first (the library holds files)"))?;
                let note = doc.dirty.then(|| "the document has unsaved changes; the file on disk was added".to_string());
                let title = doc.info.title.as_deref().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
                Ok((self.resolve(&path, false)?, title, note))
            }
            (None, None) => Err(bad("pass `path` (a PDF file) or `doc` (an open document)")),
        }
    }

    pub(crate) fn library_add(&mut self, a: &Args) -> Result<Value> {
        let lib = self.library_for(a)?;
        let (path, doc_title, unsaved) = self.source_file(a)?;
        let options = AddOptions {
            mode: if a.opt_bool("copy")?.unwrap_or(true) { Mode::Copy } else { Mode::Reference },
            title: a.opt_str("title")?.map(str::to_string).or(doc_title),
            tags: if a.get("tags").is_some() { a.strs("tags")?.into_iter().map(str::to_string).collect() } else { Vec::new() },
            password: a.opt_str("password")?.map(str::to_string),
        };
        let added = lib.add_file(&path, &options).map_err(lib_err)?;
        let mut out = Self::entry_json(&lib, &added.entry);
        out["created"] = json!(added.created);
        out["copied"] = json!(added.copied);
        out["library"] = json!(lib.root().to_string_lossy());
        out["note"] = json!(added.note.or(unsaved));
        Ok(out)
    }

    pub(crate) fn library_list(&mut self, a: &Args) -> Result<Value> {
        let lib = self.library_for(a)?;
        let snapshot = lib.snapshot().map_err(lib_err)?;
        let limit = a.opt_int("limit")?.map_or(200, |n| n.clamp(1, MAX_LIST as i64) as usize);
        let tags: Vec<String> = a.opt_str("tag")?.map(str::to_string).into_iter().collect();
        let missing_only = a.opt_bool("missing_only")?.unwrap_or(false);
        let mut documents = Vec::new();
        for hit in snapshot.search("", &tags, usize::MAX) {
            let json = Self::entry_json(&lib, &hit.entry);
            if missing_only && json["missing"] != json!(true) {
                continue;
            }
            documents.push(json);
            if documents.len() >= limit {
                break;
            }
        }
        let tag_list: Vec<Value> = snapshot.tag_counts().into_iter().map(|(tag, count)| json!({ "tag": tag, "count": count })).collect();
        Ok(json!({
            "library": lib.root().to_string_lossy(),
            "exists": lib.root().is_dir(),
            "documents": documents,
            "tags": tag_list,
            "warnings": snapshot.warnings,
        }))
    }

    pub(crate) fn library_search(&mut self, a: &Args) -> Result<Value> {
        let query = a.str("query")?.to_string();
        let lib = self.library_for(a)?;
        let snapshot = lib.snapshot().map_err(lib_err)?;
        let limit = a.opt_int("limit")?.map_or(20, |n| n.clamp(1, 200) as usize);
        let tags: Vec<String> = a.opt_str("tag")?.map(str::to_string).into_iter().collect();
        let with_vault = a.opt_bool("vault")?.unwrap_or(true);
        let mut results: Vec<(u32, Value)> = Vec::new();
        for hit in snapshot.search(&query, &tags, limit) {
            let mut e = Self::entry_json(&lib, &hit.entry);
            e["score"] = json!(hit.score);
            e["matched"] = json!(hit.fields.iter().map(|f| f.name()).collect::<Vec<_>>());
            e["snippet"] = json!(hit.snippet);
            e["vault"] = json!(false);
            results.push((hit.score, e));
        }
        // Vault documents appear only while the vault is unlocked, and never otherwise.
        let mut vault_searched = false;
        let dir = lib.root().to_path_buf();
        let now = self.since_start();
        if with_vault {
            let session = self.vault_session(&dir);
            session.expire(now);
            // Locked: nothing from the vault, and nothing said about what it holds.
            if session.is_unlocked()
                && let Ok(vault) = session.vault(now)
                && let Ok(snap) = vault.snapshot()
            {
                vault_searched = true;
                for hit in snap.search(&query, &tags, limit) {
                    let mut e = vault_entry_json(&hit.entry);
                    e["score"] = json!(hit.score);
                    e["matched"] = json!(hit.fields.iter().map(|f| f.name()).collect::<Vec<_>>());
                    e["snippet"] = json!(hit.snippet);
                    results.push((hit.score, e));
                }
            }
        }
        // Stable: equal scores keep the library's recency order, then the vault's.
        results.sort_by_key(|x| std::cmp::Reverse(x.0));
        results.truncate(limit);
        Ok(json!({
            "library": dir.to_string_lossy(),
            "query": query,
            "vault_searched": vault_searched,
            "results": results.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        }))
    }

    pub(crate) fn library_remove(&mut self, a: &Args) -> Result<Value> {
        let lib = self.library_for(a)?;
        let (entry, deleted_copy) = lib.remove(a.str("id")?).map_err(lib_err)?;
        Ok(json!({ "removed": entry.id, "title": entry.title, "deleted_copy": deleted_copy, "library": lib.root().to_string_lossy() }))
    }

    pub(crate) fn library_tag(&mut self, a: &Args) -> Result<Value> {
        let lib = self.library_for(a)?;
        let id = a.str("id")?;
        let add: Vec<String> = if a.get("add").is_some() { a.strs("add")?.into_iter().map(str::to_string).collect() } else { Vec::new() };
        let remove: Vec<String> = if a.get("remove").is_some() { a.strs("remove")?.into_iter().map(str::to_string).collect() } else { Vec::new() };
        if let Some(title) = a.opt_str("title")? {
            lib.retitle(id, title).map_err(lib_err)?;
        }
        let entry = if add.is_empty() && remove.is_empty() {
            lib.snapshot().map_err(lib_err)?.get(id).cloned().ok_or_else(|| failed(format!("no document {id} in the library")))?
        } else {
            lib.retag(id, &add, &remove).map_err(lib_err)?
        };
        Ok(Self::entry_json(&lib, &entry))
    }

    pub(crate) fn library_open(&mut self, a: &Args) -> Result<Value> {
        let lib = self.library_for(a)?;
        let id = a.str("id")?;
        let snapshot = lib.snapshot().map_err(lib_err)?;
        let entry = snapshot.get(id).cloned().ok_or_else(|| failed(format!("no document {id} in the library")))?;
        let file = lib.resolve(&entry);
        if !file.is_file() {
            return Err(failed(format!(
                "{} is missing on this computer ({}): restore it, or remove the entry with library_remove",
                entry.title,
                file.display()
            )));
        }
        let path = file.to_string_lossy().into_owned();
        let mut out = self.doc_open(&Args(&json!({ "path": path, "password": a.opt_str("password")? })))?;
        out["library_id"] = json!(entry.id);
        // A failure to note the open (the folder is read-only, or syncing) does not fail the open.
        out["marked_opened"] = json!(lib.mark_opened(&entry.id).is_ok());
        Ok(out)
    }

    // ---- vault ---------------------------------------------------------------------------

    /// The passphrase given in `passphrase`, else `PDFCRAFT_VAULT_PASSPHRASE`.
    fn passphrase(a: &Args) -> Result<Option<String>> {
        if let Some(p) = a.opt_str("passphrase")? {
            return Ok(Some(p.to_string()));
        }
        Ok(std::env::var(PASSPHRASE_ENV).ok().filter(|p| !p.is_empty()))
    }

    fn status_json(&mut self, dir: &Path) -> Value {
        let now = self.since_start();
        let session = self.vault_session(dir);
        session.expire(now);
        let s = session.status();
        json!({
            "library": dir.to_string_lossy(),
            "exists": s.exists,
            "unlocked": s.unlocked,
            "idle_lock_minutes": s.idle_timeout.map(|d| d.as_secs() / 60),
            "key_cache": { "supported": s.cache_supported, "remembered": s.cached, "name": s.cache_name },
        })
    }

    pub(crate) fn vault_create(&mut self, a: &Args) -> Result<Value> {
        let dir = self.library_folder(a)?;
        let passphrase = Self::passphrase(a)?.ok_or_else(|| bad("pass `passphrase` (or set PDFCRAFT_VAULT_PASSPHRASE)"))?;
        check_passphrase(&passphrase).map_err(lib_err)?;
        let remember = a.opt_bool("remember")?.unwrap_or(false);
        let (now, kdf) = (self.since_start(), self.library.kdf);
        let warning = self.vault_session(&dir).create(&passphrase, kdf, remember, now).map_err(lib_err)?;
        let mut out = self.status_json(&dir);
        out["created"] = json!(true);
        out["remembered"] = json!(remember && warning.is_none());
        out["key_cache_warning"] = json!(warning.map(|w| w.to_string()));
        Ok(out)
    }

    pub(crate) fn vault_unlock(&mut self, a: &Args) -> Result<Value> {
        let dir = self.library_folder(a)?;
        let now = self.since_start();
        let remember = a.opt_bool("remember")?.unwrap_or(false);
        let mut warning = None;
        match Self::passphrase(a)? {
            Some(passphrase) => {
                warning = self.vault_session(&dir).unlock_with_passphrase(&passphrase, remember, now).map_err(lib_err)?;
            }
            // No passphrase: the key this Mac remembers, which is where Touch ID is asked.
            None => self.vault_session(&dir).unlock_with_cache(now).map_err(lib_err)?,
        }
        let mut out = self.status_json(&dir);
        out["key_cache_warning"] = json!(warning.map(|w| w.to_string()));
        Ok(out)
    }

    pub(crate) fn vault_lock(&mut self, a: &Args) -> Result<Value> {
        let dir = self.library_folder(a)?;
        let forget = a.opt_bool("forget_device_key")?.unwrap_or(false);
        let discard = a.opt_bool("discard_changes")?.unwrap_or(false);
        // Documents open from the vault hold decrypted content: clean ones are closed with the
        // lock, one with unsaved changes is never thrown away (unless asked) and stays open.
        let bound: Vec<(DocId, String)> = self.library.bound.iter().map(|(d, v)| (*d, v.clone())).collect();
        let mut closed = Vec::new();
        let mut kept_open = Vec::new();
        for (doc, vault_id) in bound {
            let dirty = self.session.get(doc).is_some_and(|d| d.dirty);
            if dirty && !discard {
                kept_open.push(json!({ "doc": doc.0, "vault_id": vault_id }));
                continue;
            }
            self.session.close(doc);
            self.renderers.remove(&doc);
            self.texts.remove(&doc);
            self.library.bound.remove(&doc);
            closed.push(doc.0);
        }
        let session = self.vault_session(&dir);
        session.lock();
        let forgotten = if forget { Some(session.forget_cached_key().is_ok()) } else { None };
        let mut out = self.status_json(&dir);
        out["closed_documents"] = json!(closed);
        out["unsaved_documents_kept_open"] = json!(kept_open);
        out["device_key_forgotten"] = json!(forgotten);
        Ok(out)
    }

    pub(crate) fn vault_list(&mut self, a: &Args) -> Result<Value> {
        let dir = self.library_folder(a)?;
        let mut out = self.status_json(&dir);
        let now = self.since_start();
        let mut documents = Vec::new();
        let mut warnings = Vec::new();
        // A locked vault lists nothing: not even how many documents it holds.
        if let Ok(vault) = self.vault_session(&dir).vault(now) {
            let snap = vault.snapshot().map_err(lib_err)?;
            let tags: Vec<String> = a.opt_str("tag")?.map(str::to_string).into_iter().collect();
            documents = snap.search(a.opt_str("query")?.unwrap_or(""), &tags, MAX_LIST).iter().map(|h| vault_entry_json(&h.entry)).collect();
            warnings = snap.warnings.clone();
        }
        out["documents"] = json!(documents);
        out["warnings"] = json!(warnings);
        Ok(out)
    }

    /// The bytes a `path` or `doc` argument names, and the file they came from.
    fn vault_source(&mut self, a: &Args) -> Result<VaultSource> {
        match (a.opt_str("path")?, a.opt_int("doc")?) {
            (Some(_), Some(_)) => Err(bad("pass `path` or `doc`, not both")),
            (Some(path), None) => {
                let file = self.resolve(path, false)?;
                let bytes = crate::read_input(&file)?;
                let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                Ok(VaultSource { bytes, name, title: None, file: Some(file), clean: true })
            }
            (None, Some(_)) => {
                let doc = self.doc(a)?;
                let (id, name, title, path, dirty) =
                    (doc.id, doc.name.clone(), doc.info.title.as_deref().unwrap_or("").trim().to_string(), doc.path.clone(), doc.dirty);
                // What Save would write now, so edits made since the last save go along.
                let bytes = self.session.save_bytes(id).map_err(failed)?;
                let file = path.map(PathBuf::from);
                Ok(VaultSource { bytes: bytes.as_ref().clone(), name, title: Some(title).filter(|t| !t.is_empty()), file, clean: !dirty })
            }
            (None, None) => Err(bad("pass `path` (a PDF file) or `doc` (an open document)")),
        }
    }

    pub(crate) fn vault_add(&mut self, a: &Args) -> Result<Value> {
        let dir = self.library_folder(a)?;
        let delete_original = a.opt_bool("delete_original")?.unwrap_or(false);
        let VaultSource { bytes, name, title: doc_title, file, clean } = self.vault_source(a)?;
        // The file to delete, resolved against --root and the write root now, before anything is encrypted.
        let mut delete_target: Option<PathBuf> = None;
        if delete_original {
            if let Some(f) = &file {
                delete_target = Some(self.resolve(&f.to_string_lossy(), true)?);
            }
            if file.is_none() {
                return Err(bad("delete_original needs the document's file: this document has none"));
            }
            if !clean {
                return Err(bad("delete_original is refused while the document has unsaved changes: the file on disk is not what was encrypted"));
            }
            if let Some(f) = &file
                && f.starts_with(&dir)
                && f.components().any(|c| c.as_os_str() == pdfcraft_library::vault::VAULT_DIR)
            {
                return Err(bad("that file is inside the vault folder"));
            }
        }
        let options = VaultAddOptions {
            name,
            title: a.opt_str("title")?.map(str::to_string).or(doc_title),
            tags: if a.get("tags").is_some() { a.strs("tags")?.into_iter().map(str::to_string).collect() } else { Vec::new() },
            password: a.opt_str("password")?.map(str::to_string),
        };
        let now = self.since_start();
        let added = self.vault_session(&dir).vault(now).map_err(lib_err)?.add(&bytes, &options).map_err(lib_err)?;
        // Only after the encrypted copy was read back from disk and decrypted to the same bytes.
        let mut original_deleted = false;
        let mut delete_error = None;
        if delete_original && added.verified {
            match delete_target.as_ref().map(std::fs::remove_file) {
                Some(Ok(())) => original_deleted = true,
                Some(Err(e)) => delete_error = Some(e.to_string()),
                None => {}
            }
        }
        let mut out = vault_entry_json(&added.entry);
        out["verified"] = json!(added.verified);
        out["original_deleted"] = json!(original_deleted);
        out["delete_error"] = json!(delete_error);
        out["note"] = json!(added.note);
        Ok(out)
    }

    pub(crate) fn vault_open(&mut self, a: &Args) -> Result<Value> {
        let dir = self.library_folder(a)?;
        let id = a.str("id")?.to_string();
        let now = self.since_start();
        let (bytes, entry) = {
            let vault = self.vault_session(&dir).vault(now).map_err(lib_err)?;
            let bytes = vault.read(&id).map_err(lib_err)?;
            let entry = vault.snapshot().map_err(lib_err)?.get(&id).cloned().ok_or_else(|| failed(format!("no document {id} in the vault")))?;
            (bytes, entry)
        };
        let name = if entry.name.is_empty() { format!("{}.pdf", entry.title) } else { entry.name.clone() };
        // From memory: no path, so nothing names a file, and no recovery copy is ever written.
        let opened = self.session.open(name, None, Arc::new(bytes.to_vec()), a.opt_str("password")?).map_err(failed)?;
        drop(bytes);
        self.session.set_keep_off_disk(opened, true);
        self.library.bound.insert(opened, id.clone());
        if let Ok(vault) = self.vault_session(&dir).vault(now) {
            let _ = vault.mark_opened(&id);
        }
        let doc = self.session.get(opened).ok_or_else(|| failed("the document vanished"))?;
        let mut out = summary(doc);
        out["vault_id"] = json!(id);
        out["plaintext_on_disk"] = json!(false);
        Ok(out)
    }

    /// Saves a document that came from the vault by encrypting it again.
    pub(crate) fn vault_save(&mut self, a: &Args) -> Result<Value> {
        let dir = self.library_folder(a)?;
        let doc = self.doc(a)?.id;
        let vault_id =
            self.library.bound.get(&doc).cloned().ok_or_else(|| bad("that document did not come from the vault: open it with vault_open"))?;
        let bytes = self.session.save_bytes(doc).map_err(failed)?;
        self.session.check_save(doc, &bytes).map_err(failed)?;
        let now = self.since_start();
        let saved = self.vault_session(&dir).vault(now).map_err(lib_err)?.replace(&vault_id, &bytes, a.opt_str("password")?).map_err(lib_err)?;
        self.session.mark_saved(doc, bytes.clone(), None).map_err(failed)?;
        let d = self.session.get(doc).ok_or_else(|| failed("the saved document is no longer open"))?;
        Ok(json!({ "vault_id": vault_id, "bytes": bytes.len(), "verified": saved.verified, "document": summary(d), "plaintext_on_disk": false }))
    }

    pub(crate) fn vault_remove(&mut self, a: &Args) -> Result<Value> {
        let dir = self.library_folder(a)?;
        let id = a.str("id")?.to_string();
        let now = self.since_start();
        let entry = self.vault_session(&dir).vault(now).map_err(lib_err)?.remove(&id).map_err(lib_err)?;
        // An open copy of a removed document is no longer the vault's: it saves nowhere.
        self.library.bound.retain(|_, v| v != &id);
        Ok(json!({ "removed": entry.id, "title": entry.title }))
    }
}

fn vault_entry_json(e: &VaultEntry) -> Value {
    json!({
        "id": e.id, "title": e.title, "name": e.name, "pages": e.pages, "tags": e.tags,
        "added_ms": e.added_ms, "opened_ms": e.opened_ms, "modified_ms": e.modified_ms,
        "size": e.size, "hash": e.hash, "vault": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-page PDF.
    fn one_page() -> Vec<u8> {
        let content = "BT /F1 24 Tf 20 150 Td (Page 1) Tj ET";
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [4 0 R] /Count 1 /MediaBox [0 0 200 300] >>".to_string(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /Contents 5 0 R /Resources << /Font << /F1 3 0 R >> >> >>".to_string(),
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

    /// A document opened from the vault is never handed to crash recovery, which writes plain
    /// files: its edits are saved only by encrypting them again.
    #[test]
    fn a_vault_document_is_kept_out_of_crash_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = Automation::new()
            .with_library_dir(dir.path().join("lib"))
            .with_vault_key_cache(Arc::new(pdfcraft_library::vault::MemoryCache::new()))
            .with_vault_kdf(KdfParams::MIN_STRENGTH);
        let file = dir.path().join("a.pdf");
        std::fs::write(&file, one_page()).unwrap();
        let call = |a: &mut Automation, name: &str, args: Value| a.call(name, &args).unwrap();
        call(&mut a, "vault_create", json!({ "passphrase": "correct horse battery" }));
        let id = match call(&mut a, "vault_add", json!({ "path": file.to_string_lossy() })).remove(0) {
            crate::Content::Json(v) => v["id"].as_str().unwrap().to_string(),
            other => panic!("{other:?}"),
        };
        let doc = match call(&mut a, "vault_open", json!({ "id": id })).remove(0) {
            crate::Content::Json(v) => v["doc"].as_u64().unwrap(),
            other => panic!("{other:?}"),
        };
        call(&mut a, "page_rotate", json!({ "doc": doc, "pages": [1], "degrees": 90 }));
        assert!(a.session.get(DocId(doc)).unwrap().dirty);
        assert!(!a.session.has_pending_autosave(), "no recovery snapshot is due");
        assert!(a.session.autosave_snapshots().is_empty());
        // A document opened the ordinary way is snapshotted.
        let plain = match call(&mut a, "doc_open", json!({ "path": file.to_string_lossy() })).remove(0) {
            crate::Content::Json(v) => v["doc"].as_u64().unwrap(),
            other => panic!("{other:?}"),
        };
        call(&mut a, "page_rotate", json!({ "doc": plain, "pages": [1], "degrees": 90 }));
        assert_eq!(a.session.autosave_snapshots().len(), 1);
    }
}
