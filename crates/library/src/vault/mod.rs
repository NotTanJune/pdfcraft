//! The vault: encrypted documents in the `Vault/` folder of the library folder, so that they sync
//! with it.
//!
//! - **Files.** `vault.json` (the Argon2id settings, the salt, the vault key wrapped by the
//!   passphrase's key, and a check blob), then per document `<id>.pcv` (the PDF, encrypted) and
//!   `<id>.meta.pcv` (its title, tags, page count, hash and text, encrypted). The names are random
//!   ids, so file names tell nothing.
//! - **Keys.** The vault key is 256 random bits. The passphrase goes through Argon2id to a second
//!   key that wraps it with AES-256-GCM, so the vault opens on another Mac with the passphrase,
//!   and a passphrase change needs only the wrapped key rewritten (not offered yet).
//! - **Plaintext.** Documents are decrypted into memory ([`Vault::read`]) and saved by encrypting
//!   again ([`Vault::replace`]); no plaintext is written to disk by this module.
//! - **Sync.** Metadata is one file per document, so two Macs editing different documents never
//!   conflict; two edits to the same document keep the newer (`modified_ms`), like the library
//!   index, and a removal is a tombstone. Conflict copies the sync service makes (`<id> 2.pcv`)
//!   are read too.

mod crypto;
mod keycache;
mod session;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::{LibraryError, Result};
use crate::extract;
use crate::index::{clean_tags, valid_id};
use crate::search::{self, Field, Item};
use crate::util::{self, MAX_FILE_BYTES, cut_bytes, now_ms, random_bytes, random_id, read_bounded, sha256_hex, write_atomic};

pub use crypto::{KdfParams, VaultKey};
pub use keycache::{KeyCache, KeyCacheError, MemoryCache, NoCache, default_cache};
pub use session::{DEFAULT_IDLE, Status, VaultSession, open_from_cache};

/// The vault's folder inside the library folder.
pub const VAULT_DIR: &str = "Vault";
/// The vault's key file.
pub const VAULT_FILE: &str = "vault.json";

/// The shortest passphrase accepted for a new vault.
pub const MIN_PASSPHRASE_CHARS: usize = 8;
const MAX_PASSPHRASE_BYTES: usize = 1024;
const MAX_VAULT_FILE: u64 = 64 * 1024;
const MAX_META_FILE: u64 = 4 * 1024 * 1024;
const FORMAT: u32 = 1;
const SALT_BYTES: usize = 16;
const CHECK_TEXT: &[u8] = b"pdfcraft vault key check";
const TOMBSTONE_DAYS: i64 = 90;

#[derive(Serialize, Deserialize)]
struct KdfFile {
    alg: String,
    version: u32,
    m_kib: u32,
    t: u32,
    p: u32,
    salt: String,
}

#[derive(Serialize, Deserialize)]
struct VaultFile {
    format: u32,
    id: String,
    kdf: KdfFile,
    /// The vault key under the passphrase's key (a sealed file image, base64).
    wrap: String,
    /// A known text under the vault key, to tell a right key from a wrong one (base64).
    check: String,
}

/// One document of the vault, as kept in its encrypted metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultEntry {
    pub id: String,
    /// The name of the file it came from, used when it is saved elsewhere.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub pages: u32,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub added_ms: i64,
    #[serde(default)]
    pub opened_ms: i64,
    #[serde(default)]
    pub modified_ms: i64,
    #[serde(default)]
    pub size: u64,
    /// SHA-256 of the plaintext, in hex.
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub deleted: bool,
}

impl VaultEntry {
    fn rank(&self) -> (i64, bool, &str) {
        (self.modified_ms, self.deleted, &self.hash)
    }

    fn tombstone(&self, now: i64) -> VaultEntry {
        VaultEntry {
            id: self.id.clone(),
            name: String::new(),
            title: String::new(),
            pages: 0,
            tags: Vec::new(),
            added_ms: self.added_ms,
            opened_ms: self.opened_ms,
            modified_ms: now.max(self.modified_ms.saturating_add(1)),
            size: 0,
            hash: String::new(),
            deleted: true,
        }
    }
}

/// The merge of two versions of one document: the newer edit wins, the later open and the earlier
/// add are kept.
fn merge_entry(a: &VaultEntry, b: &VaultEntry) -> VaultEntry {
    let (win, _) = if a.rank() >= b.rank() { (a, b) } else { (b, a) };
    let mut out = win.clone();
    out.opened_ms = a.opened_ms.max(b.opened_ms);
    out.added_ms = match (a.added_ms, b.added_ms) {
        (0, x) | (x, 0) => x,
        (x, y) => x.min(y),
    };
    out
}

#[derive(Serialize, Deserialize)]
struct MetaFile {
    entry: VaultEntry,
    #[serde(default)]
    text: String,
}

/// The vault's documents as of one moment.
#[derive(Clone, Debug, Default)]
pub struct VaultSnapshot {
    pub entries: BTreeMap<String, VaultEntry>,
    texts: BTreeMap<String, String>,
    /// Files that could not be read.
    pub warnings: Vec<String>,
}

/// What adding a document to the vault does.
#[derive(Clone, Debug, Default)]
pub struct VaultAddOptions {
    /// The name of the source file (for saving it out again).
    pub name: String,
    pub title: Option<String>,
    pub tags: Vec<String>,
    pub password: Option<String>,
}

/// An added document and the result of the round trip that proved it can be read back.
#[derive(Clone, Debug)]
pub struct VaultAdded {
    pub entry: VaultEntry,
    /// The file was read back from disk, decrypted and compared with what was added: byte for
    /// byte the same. Only then is deleting the original safe.
    pub verified: bool,
    pub note: Option<String>,
}

/// An unlocked vault: its folder and its key. Dropping it wipes the key. A clone (for a worker
/// thread) holds its own copy of the key, wiped when the clone is dropped.
#[derive(Clone)]
pub struct Vault {
    dir: PathBuf,
    id: String,
    key: VaultKey,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault").field("dir", &self.dir).field("id", &self.id).finish_non_exhaustive()
    }
}

/// Whether `passphrase` is acceptable for a new vault.
pub fn check_passphrase(passphrase: &str) -> Result<()> {
    if passphrase.chars().count() < MIN_PASSPHRASE_CHARS {
        return Err(LibraryError::Invalid(format!("the passphrase needs at least {MIN_PASSPHRASE_CHARS} characters")));
    }
    if passphrase.len() > MAX_PASSPHRASE_BYTES {
        return Err(LibraryError::Invalid(format!("the passphrase is longer than {MAX_PASSPHRASE_BYTES} bytes")));
    }
    Ok(())
}

fn vault_dir(library_root: &Path) -> PathBuf {
    library_root.join(VAULT_DIR)
}

/// Whether a vault exists in the library folder `library_root`.
pub fn exists(library_root: &Path) -> bool {
    let file = vault_dir(library_root).join(VAULT_FILE);
    file.is_file() || util::is_placeholder_only(&file)
}

fn read_vault_file(dir: &Path) -> Result<VaultFile> {
    let path = dir.join(VAULT_FILE);
    if util::is_placeholder_only(&path) {
        return Err(LibraryError::NotDownloaded(VAULT_FILE.to_string()));
    }
    if !path.is_file() {
        return Err(LibraryError::NoVault);
    }
    let bytes = read_bounded(&path, MAX_VAULT_FILE)?;
    let file: VaultFile = serde_json::from_slice(&bytes).map_err(|e| LibraryError::Damaged(format!("{}: {e}", path.display())))?;
    if file.format > FORMAT {
        return Err(LibraryError::TooNew);
    }
    if file.format == 0 || !valid_id(&file.id) {
        return Err(LibraryError::Damaged(format!("{} is not a vault key file", path.display())));
    }
    Ok(file)
}

/// The associated-data name of the wrapped key: it binds the settings and the salt, so changing
/// either makes the passphrase fail instead of deriving another key.
fn wrap_name(kdf: &KdfFile) -> String {
    format!("key-wrap:{}:{}:{}:{}:{}", kdf.alg, kdf.m_kib, kdf.t, kdf.p, kdf.salt)
}

fn decode_b64(what: &str, text: &str) -> Result<Vec<u8>> {
    B64.decode(text).map_err(|_| LibraryError::Damaged(format!("the vault key file has a damaged {what}")))
}

impl Vault {
    /// Creates a vault in `library_root/Vault` protected by `passphrase`, and returns it unlocked.
    /// Refuses when the folder already has one (or a half-synced one), so two Macs creating a
    /// vault at the same time cannot overwrite each other's key.
    pub fn create(library_root: &Path, passphrase: &str, params: KdfParams) -> Result<Vault> {
        check_passphrase(passphrase)?;
        params.check()?;
        let dir = vault_dir(library_root);
        if exists(library_root) {
            return Err(LibraryError::VaultExists);
        }
        if let Ok(entries) = std::fs::read_dir(&dir)
            && entries
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().starts_with("vault") && e.file_name().to_string_lossy().ends_with(".json"))
        {
            return Err(LibraryError::VaultExists);
        }
        let id = random_id::<8>()?;
        let key = VaultKey::generate()?;
        let salt = random_bytes::<SALT_BYTES>()?;
        let kdf = KdfFile { alg: "argon2id".to_string(), version: 19, m_kib: params.m_kib, t: params.t, p: params.p, salt: B64.encode(salt) };
        let kek = crypto::derive(passphrase, &salt, params)?;
        let wrap = crypto::seal(&kek, crypto::MAGIC_WRAP, &id, &wrap_name(&kdf), key.expose())?;
        let check = crypto::seal(&key, crypto::MAGIC_CHECK, &id, "key-check", CHECK_TEXT)?;
        let file = VaultFile { format: FORMAT, id: id.clone(), kdf, wrap: B64.encode(wrap), check: B64.encode(check) };
        let json = serde_json::to_vec_pretty(&file).map_err(|e| LibraryError::Damaged(e.to_string()))?;
        write_atomic(&dir.join(VAULT_FILE), &json)?;
        Ok(Vault { dir, id, key })
    }

    /// Opens the vault with its passphrase. [`LibraryError::WrongPassphrase`] when it is not.
    pub fn unlock(library_root: &Path, passphrase: &str) -> Result<Vault> {
        let dir = vault_dir(library_root);
        let file = read_vault_file(&dir)?;
        if file.kdf.alg != "argon2id" || file.kdf.version != 19 {
            return Err(LibraryError::Damaged(format!("the vault uses {} v{}, which this PdfCraft cannot read", file.kdf.alg, file.kdf.version)));
        }
        let params = KdfParams { m_kib: file.kdf.m_kib, t: file.kdf.t, p: file.kdf.p };
        let salt = decode_b64("salt", &file.kdf.salt)?;
        if !(8..=64).contains(&salt.len()) {
            return Err(LibraryError::Damaged("the vault key file has a damaged salt".to_string()));
        }
        let kek = crypto::derive(passphrase, &salt, params)?;
        let wrapped = decode_b64("wrapped key", &file.wrap)?;
        let key_bytes = match crypto::open(&kek, crypto::MAGIC_WRAP, &file.id, &wrap_name(&file.kdf), &wrapped) {
            Ok(bytes) => bytes,
            Err(LibraryError::TooNew) => return Err(LibraryError::TooNew),
            Err(_) => return Err(LibraryError::WrongPassphrase),
        };
        let key = VaultKey::from_bytes(&key_bytes).ok_or_else(|| LibraryError::Damaged("the wrapped vault key has the wrong length".to_string()))?;
        Vault::with_checked_key(dir, file, key)
    }

    /// Opens the vault with its key (from the key cache). [`LibraryError::WrongPassphrase`] when
    /// the key is not this vault's (the vault was recreated, or the cache is stale).
    pub fn open_with_key(library_root: &Path, key: VaultKey) -> Result<Vault> {
        let dir = vault_dir(library_root);
        let file = read_vault_file(&dir)?;
        Vault::with_checked_key(dir, file, key)
    }

    fn with_checked_key(dir: PathBuf, file: VaultFile, key: VaultKey) -> Result<Vault> {
        let check = decode_b64("check", &file.check)?;
        match crypto::open(&key, crypto::MAGIC_CHECK, &file.id, "key-check", &check) {
            Ok(text) if text.as_slice() == CHECK_TEXT => Ok(Vault { dir, id: file.id, key }),
            Err(LibraryError::TooNew) => Err(LibraryError::TooNew),
            _ => Err(LibraryError::WrongPassphrase),
        }
    }

    /// Whether this vault is the one in the library folder `library_root`.
    pub fn is_in(&self, library_root: &Path) -> bool {
        self.dir == vault_dir(library_root)
    }

    /// The vault's id: the name its key is cached under.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The vault key, for the key cache.
    pub fn key(&self) -> &VaultKey {
        &self.key
    }

    /// The id of the vault in `library_root` without unlocking it.
    pub fn id_of(library_root: &Path) -> Result<String> {
        Ok(read_vault_file(&vault_dir(library_root))?.id)
    }

    fn doc_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.pcv"))
    }

    fn meta_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.meta.pcv"))
    }

    /// Every file of the folder for `id`: the canonical one first, then conflict copies.
    fn files_of(&self, id: &str, suffix: &str) -> Vec<PathBuf> {
        let Ok(dir) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut found: Vec<(bool, PathBuf)> = dir
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let (item, canonical) = split_name(&name, suffix)?;
                (item == id).then(|| (!canonical, e.path()))
            })
            .collect();
        found.sort();
        found.into_iter().map(|(_, p)| p).collect()
    }

    /// Reads the metadata of every document, decrypted. Files that do not open are skipped and
    /// said in `warnings`.
    pub fn snapshot(&self) -> Result<VaultSnapshot> {
        let mut snapshot = VaultSnapshot::default();
        let dir = match std::fs::read_dir(&self.dir) {
            Ok(d) => d,
            Err(_) => return Ok(snapshot),
        };
        let mut metas: Vec<(String, bool, PathBuf)> = dir
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let (id, canonical) = split_name(&name, ".meta.pcv")?;
                Some((id, !canonical, e.path()))
            })
            .collect();
        metas.sort_by(|a, b| (a.1, &a.2).cmp(&(b.1, &b.2)));
        for (id, _, path) in metas {
            let loaded = read_bounded(&path, MAX_META_FILE)
                .and_then(|bytes| crypto::open(&self.key, crypto::MAGIC_META, &self.id, &id, &bytes))
                .and_then(|plain| serde_json::from_slice::<MetaFile>(&plain).map_err(|e| LibraryError::Damaged(format!("{e}"))));
            let meta = match loaded {
                Ok(m) if m.entry.id == id => m,
                Ok(_) => {
                    snapshot.warnings.push(format!("{}: names another document", path.display()));
                    continue;
                }
                Err(e) => {
                    snapshot.warnings.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            let mut entry = meta.entry;
            entry.tags = clean_tags(entry.tags.iter().map(String::as_str));
            let text = cut_bytes(&meta.text, extract::MAX_TEXT_BYTES).to_string();
            match snapshot.entries.get(&id).cloned() {
                Some(existing) => {
                    // A conflict copy: the newer edit's metadata and text, the later open.
                    let newer = entry.rank() > existing.rank();
                    snapshot.entries.insert(id.clone(), merge_entry(&existing, &entry));
                    if newer {
                        snapshot.texts.insert(id.clone(), text);
                    }
                }
                None => {
                    snapshot.entries.insert(id.clone(), entry);
                    snapshot.texts.insert(id.clone(), text);
                }
            }
        }
        Ok(snapshot)
    }

    fn write_meta(&self, entry: &VaultEntry, text: &str) -> Result<()> {
        let meta = MetaFile { entry: entry.clone(), text: text.to_string() };
        let json = Zeroizing::new(serde_json::to_vec(&meta).map_err(|e| LibraryError::Damaged(e.to_string()))?);
        let sealed = crypto::seal(&self.key, crypto::MAGIC_META, &self.id, &entry.id, &json)?;
        write_atomic(&self.meta_path(&entry.id), &sealed)?;
        // The file just written holds the merge of the copies; the copies would only be merged again.
        for copy in self.files_of(&entry.id, ".meta.pcv").into_iter().skip(1) {
            let _ = std::fs::remove_file(copy);
        }
        Ok(())
    }

    /// Encrypts `bytes` into the vault as a new document, then reads it back from disk and
    /// compares it with `bytes`. If that does not match, nothing is kept and it is an error.
    pub fn add(&self, bytes: &[u8], options: &VaultAddOptions) -> Result<VaultAdded> {
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(LibraryError::Invalid("that document is larger than the 512 MiB the vault accepts".to_string()));
        }
        let facts = extract::read_facts(Arc::new(bytes.to_vec()), options.password.as_deref())?;
        let id = random_id::<16>()?;
        let hash = sha256_hex(bytes);
        let now = now_ms();
        let name = util::safe_file_name(&options.name);
        let title = options
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map_or_else(|| name.trim_end_matches(".pdf").to_string(), str::to_string);
        let entry = VaultEntry {
            id: id.clone(),
            name,
            title: cut_bytes(&title, 512).to_string(),
            pages: u32::try_from(facts.pages).unwrap_or(u32::MAX),
            tags: clean_tags(options.tags.iter().map(String::as_str)),
            added_ms: now,
            opened_ms: 0,
            modified_ms: now,
            size: bytes.len() as u64,
            hash: hash.clone(),
            deleted: false,
        };
        self.write_document(&id, bytes)?;
        if let Err(e) = self.write_meta(&entry, &facts.text) {
            let _ = std::fs::remove_file(self.doc_path(&id));
            return Err(e);
        }
        let verified = self.read_back_matches(&id, &hash);
        if !verified {
            let _ = std::fs::remove_file(self.doc_path(&id));
            let _ = std::fs::remove_file(self.meta_path(&id));
            return Err(LibraryError::Damaged("the encrypted copy did not read back the same, so it was not kept: nothing was changed".to_string()));
        }
        let note = if facts.unreadable {
            Some("the document could not be opened (password-protected or damaged), so its pages and text are not indexed".to_string())
        } else if facts.text.trim().is_empty() {
            Some("the document has no text layer: run Scan & OCR to make it searchable".to_string())
        } else {
            None
        };
        Ok(VaultAdded { entry, verified, note })
    }

    fn write_document(&self, id: &str, bytes: &[u8]) -> Result<()> {
        let sealed = crypto::seal(&self.key, crypto::MAGIC_DOC, &self.id, id, bytes)?;
        write_atomic(&self.doc_path(id), &sealed)
    }

    /// Reads the document file back from disk and decrypts it: whether it equals `hash`.
    fn read_back_matches(&self, id: &str, hash: &str) -> bool {
        read_bounded(&self.doc_path(id), MAX_FILE_BYTES + 1024)
            .and_then(|file| crypto::open(&self.key, crypto::MAGIC_DOC, &self.id, id, &file))
            .is_ok_and(|plain| sha256_hex(&plain) == hash)
    }

    /// The decrypted document, in memory only. When the sync service left conflict copies of the
    /// file, the one that matches the newest metadata is used.
    pub fn read(&self, id: &str) -> Result<Zeroizing<Vec<u8>>> {
        let snapshot = self.snapshot()?;
        let entry =
            snapshot.entries.get(id).filter(|e| !e.deleted).ok_or_else(|| LibraryError::NotFound(format!("no document {id} in the vault")))?;
        let mut fallback: Option<Zeroizing<Vec<u8>>> = None;
        for path in self.files_of(id, ".pcv") {
            if util::is_placeholder_only(&path) {
                continue;
            }
            let Ok(file) = read_bounded(&path, MAX_FILE_BYTES + 1024) else { continue };
            let Ok(plain) = crypto::open(&self.key, crypto::MAGIC_DOC, &self.id, id, &file) else { continue };
            if sha256_hex(&plain) == entry.hash {
                return Ok(plain);
            }
            fallback.get_or_insert(plain);
        }
        if util::is_placeholder_only(&self.doc_path(id)) {
            return Err(LibraryError::NotDownloaded(format!("{id}.pcv")));
        }
        fallback.ok_or_else(|| LibraryError::Damaged(format!("the encrypted file of document {id} is missing or does not open")))
    }

    /// Saves new content for a document: encrypts it again with a new nonce, atomically, and
    /// updates the metadata. Read back and compared like [`Self::add`]; a failed save leaves the
    /// old content in place.
    pub fn replace(&self, id: &str, bytes: &[u8], password: Option<&str>) -> Result<VaultAdded> {
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(LibraryError::Invalid("that document is larger than the 512 MiB the vault accepts".to_string()));
        }
        let snapshot = self.snapshot()?;
        let old = snapshot
            .entries
            .get(id)
            .filter(|e| !e.deleted)
            .cloned()
            .ok_or_else(|| LibraryError::NotFound(format!("no document {id} in the vault")))?;
        let old_file = read_bounded(&self.doc_path(id), MAX_FILE_BYTES + 1024).ok();
        let facts = extract::read_facts(Arc::new(bytes.to_vec()), password)?;
        let hash = sha256_hex(bytes);
        self.write_document(id, bytes)?;
        if !self.read_back_matches(id, &hash) {
            // Put the old file back rather than leave a copy that does not read.
            if let Some(old_file) = old_file {
                let _ = write_atomic(&self.doc_path(id), &old_file);
            }
            return Err(LibraryError::Damaged("the encrypted copy did not read back the same, so the old content was kept".to_string()));
        }
        let mut entry = old;
        entry.pages = u32::try_from(facts.pages).unwrap_or(u32::MAX);
        entry.size = bytes.len() as u64;
        entry.hash = hash;
        entry.modified_ms = now_ms().max(entry.modified_ms.saturating_add(1));
        self.write_meta(&entry, &facts.text)?;
        for copy in self.files_of(id, ".pcv").into_iter().skip(1) {
            let _ = std::fs::remove_file(copy);
        }
        Ok(VaultAdded { entry, verified: true, note: None })
    }

    /// Removes a document: its encrypted file is deleted and a tombstone remains for the other Macs.
    pub fn remove(&self, id: &str) -> Result<VaultEntry> {
        let snapshot = self.snapshot()?;
        let entry = snapshot
            .entries
            .get(id)
            .filter(|e| !e.deleted)
            .cloned()
            .ok_or_else(|| LibraryError::NotFound(format!("no document {id} in the vault")))?;
        let tomb = entry.tombstone(now_ms());
        self.write_meta(&tomb, "")?;
        for file in self.files_of(id, ".pcv") {
            let _ = std::fs::remove_file(file);
        }
        self.prune(&snapshot);
        Ok(entry)
    }

    fn prune(&self, snapshot: &VaultSnapshot) {
        let horizon = now_ms().saturating_sub(TOMBSTONE_DAYS * 86_400_000);
        for e in snapshot.entries.values().filter(|e| e.deleted && e.modified_ms < horizon) {
            for file in self.files_of(&e.id, ".meta.pcv") {
                let _ = std::fs::remove_file(file);
            }
        }
    }

    /// Changes a document's tags.
    pub fn retag(&self, id: &str, add: &[String], remove: &[String]) -> Result<VaultEntry> {
        self.update_meta(id, |entry| {
            let drop: Vec<String> = remove.iter().map(|t| t.trim().to_lowercase()).collect();
            let kept: Vec<String> = entry.tags.iter().filter(|t| !drop.contains(&t.to_lowercase())).cloned().collect();
            entry.tags = clean_tags(kept.iter().map(String::as_str).chain(add.iter().map(String::as_str)));
            true
        })
    }

    /// Notes that a document was opened now.
    pub fn mark_opened(&self, id: &str) -> Result<VaultEntry> {
        self.update_meta(id, |entry| {
            entry.opened_ms = now_ms();
            false
        })
    }

    fn update_meta(&self, id: &str, change: impl FnOnce(&mut VaultEntry) -> bool) -> Result<VaultEntry> {
        let snapshot = self.snapshot()?;
        let mut entry = snapshot
            .entries
            .get(id)
            .filter(|e| !e.deleted)
            .cloned()
            .ok_or_else(|| LibraryError::NotFound(format!("no document {id} in the vault")))?;
        if change(&mut entry) {
            entry.modified_ms = now_ms().max(entry.modified_ms.saturating_add(1));
        }
        self.write_meta(&entry, snapshot.texts.get(id).map_or("", String::as_str))?;
        Ok(entry)
    }
}

/// `(item id, canonical)` of a vault file name ending in `suffix`: `abc.pcv` is `abc`, canonical;
/// `abc 2.pcv` is `abc`, a conflict copy. `.meta.pcv` names are not documents (`.pcv`) and the
/// other way round.
fn split_name(name: &str, suffix: &str) -> Option<(String, bool)> {
    if name.starts_with('.') {
        return None;
    }
    let stem = name.strip_suffix(suffix)?;
    if suffix == ".pcv" && stem.ends_with(".meta") {
        return None;
    }
    let id_len = stem.bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_').count();
    if id_len == 0 {
        return None;
    }
    let (id, rest) = stem.split_at(id_len);
    if !valid_id(id) {
        return None;
    }
    if rest.is_empty() {
        Some((id.to_string(), true))
    } else if rest.starts_with([' ', '(', '.']) {
        // `abc 2`, `abc (conflicted copy)`, `abc.sync-conflict-...`; a bare `.meta` is not one.
        (rest != ".meta").then(|| (id.to_string(), false))
    } else {
        None
    }
}

impl VaultSnapshot {
    /// The documents that are not removals, most recently opened (or added) first.
    pub fn entries(&self) -> Vec<VaultEntry> {
        let mut list: Vec<VaultEntry> = self.entries.values().filter(|e| !e.deleted).cloned().collect();
        list.sort_by(|a, b| (b.opened_ms.max(b.added_ms), &a.title, &a.id).cmp(&(a.opened_ms.max(a.added_ms), &b.title, &b.id)));
        list
    }

    pub fn get(&self, id: &str) -> Option<&VaultEntry> {
        self.entries.get(id).filter(|e| !e.deleted)
    }

    /// The same search as the library's.
    pub fn search(&self, query: &str, tags: &[String], limit: usize) -> Vec<VaultHit> {
        let words = search::words(query);
        let wanted: Vec<String> = tags.iter().map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()).collect();
        let mut hits = Vec::new();
        for (rank, entry) in self.entries().into_iter().enumerate() {
            if !wanted.iter().all(|w| entry.tags.iter().any(|t| &t.to_lowercase() == w)) {
                continue;
            }
            let text = self.texts.get(&entry.id).map_or("", String::as_str);
            let item = Item { title: &entry.title, tags: &entry.tags, text };
            let Some(m) = search::matches(&item, &words) else { continue };
            hits.push(VaultHit { entry, score: m.score, fields: m.fields, snippet: m.snippet, recency: rank });
        }
        hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.recency.cmp(&b.recency)));
        hits.truncate(limit);
        hits
    }
}

/// One vault search result.
#[derive(Clone, Debug)]
pub struct VaultHit {
    pub entry: VaultEntry,
    pub score: u32,
    pub fields: Vec<Field>,
    pub snippet: Option<String>,
    recency: usize,
}

#[cfg(test)]
mod tests;
