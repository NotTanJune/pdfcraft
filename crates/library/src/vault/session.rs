//! A vault that is locked or unlocked, for an app or a tool session: the unlocked key lives here
//! and nowhere else in memory, and it is dropped (wiped) on lock, on an idle timeout and when the
//! session ends.
//!
//! Time comes in from the caller as a monotonic [`Duration`] (the app's clock, a test's counter),
//! so the idle lock is testable without waiting and no clock is read here.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::{KdfParams, KeyCache, KeyCacheError, Vault, VaultKey};
use crate::error::{LibraryError, Result};

/// Lock after this long without use, unless set otherwise.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(15 * 60);

/// Opens the vault in `root` with the key `cache` holds for it (where the system asks for Touch ID
/// or the login password). A cached key that no longer fits the vault is removed. Does the slow,
/// prompting part of [`VaultSession::unlock_with_cache`], so a worker thread can run it.
pub fn open_from_cache(root: &Path, cache: &dyn KeyCache) -> Result<Vault> {
    let id = Vault::id_of(root)?;
    let key: Option<VaultKey> = cache.load(&id).map_err(|e| LibraryError::KeyCache(e.to_string()))?;
    let key = key.ok_or_else(|| LibraryError::KeyCache("no key is remembered on this Mac: unlock with the passphrase".to_string()))?;
    match Vault::open_with_key(root, key) {
        Ok(vault) => Ok(vault),
        Err(LibraryError::WrongPassphrase) => {
            let _ = cache.forget(&id);
            Err(LibraryError::KeyCache(
                "the key remembered on this Mac no longer opens the vault (it was recreated): unlock with the passphrase".to_string(),
            ))
        }
        Err(e) => Err(e),
    }
}

/// What the interface shows about the vault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// A vault exists in the library folder.
    pub exists: bool,
    pub unlocked: bool,
    /// The key cache works on this system (Touch ID on a Mac).
    pub cache_supported: bool,
    /// The cache holds this vault's key: unlocking can use Touch ID.
    pub cached: bool,
    /// What the cache is called to a person ("Touch ID or the login password").
    pub cache_name: &'static str,
    /// Lock after this long idle (`None`: never).
    pub idle_timeout: Option<Duration>,
}

/// The vault of one library folder, locked or unlocked.
pub struct VaultSession {
    root: PathBuf,
    vault: Option<Vault>,
    cache: Arc<dyn KeyCache>,
    idle: Option<Duration>,
    last_use: Duration,
}

impl VaultSession {
    /// A locked session for the vault in `root` using `cache`.
    pub fn new(root: impl Into<PathBuf>, cache: Arc<dyn KeyCache>) -> VaultSession {
        VaultSession { root: root.into(), vault: None, cache, idle: Some(DEFAULT_IDLE), last_use: Duration::ZERO }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Points the session at another library folder; the vault is locked first.
    pub fn set_root(&mut self, root: impl Into<PathBuf>) {
        self.lock();
        self.root = root.into();
    }

    /// Sets the idle lock; `None` never locks on idle.
    pub fn set_idle_timeout(&mut self, idle: Option<Duration>) {
        self.idle = idle;
    }

    pub fn idle_timeout(&self) -> Option<Duration> {
        self.idle
    }

    pub fn is_unlocked(&self) -> bool {
        self.vault.is_some()
    }

    /// The unlocked vault, or [`LibraryError::Locked`]. Counts as use.
    pub fn vault(&mut self, now: Duration) -> Result<&Vault> {
        self.expire(now);
        self.last_use = now;
        self.vault.as_ref().ok_or(LibraryError::Locked)
    }

    /// The unlocked vault without counting as use (for drawing).
    pub fn peek(&self) -> Option<&Vault> {
        self.vault.as_ref()
    }

    /// Notes activity, which pushes the idle lock back.
    pub fn touch(&mut self, now: Duration) {
        if self.vault.is_some() {
            self.last_use = now;
        }
    }

    /// Locks if the vault has been idle longer than the idle timeout. Returns whether it locked.
    pub fn expire(&mut self, now: Duration) -> bool {
        match (self.idle, &self.vault) {
            (Some(limit), Some(_)) if now.saturating_sub(self.last_use) >= limit => {
                self.lock();
                true
            }
            _ => false,
        }
    }

    /// Locks: the key is wiped from this session. The cached key stays for the next Touch ID.
    pub fn lock(&mut self) {
        self.vault = None;
    }

    /// What the interface shows.
    pub fn status(&self) -> Status {
        let exists = super::exists(&self.root);
        let cached = exists && Vault::id_of(&self.root).is_ok_and(|id| self.cache.has_key(&id));
        Status {
            exists,
            unlocked: self.vault.is_some(),
            cache_supported: self.cache.is_supported(),
            cached,
            cache_name: self.cache.describe(),
            idle_timeout: self.idle,
        }
    }

    /// Creates the vault and leaves it unlocked. With `remember` the key goes into the key cache;
    /// if that fails the vault is still created and unlocked, and the reason comes back.
    pub fn create(&mut self, passphrase: &str, params: KdfParams, remember: bool, now: Duration) -> Result<Option<KeyCacheError>> {
        let vault = Vault::create(&self.root, passphrase, params)?;
        let cache_error = if remember { self.cache.store(vault.id(), vault.key()).err() } else { None };
        self.vault = Some(vault);
        self.last_use = now;
        Ok(cache_error)
    }

    /// The key cache, for a worker that has to ask it (Touch ID) off the interface thread.
    pub fn cache(&self) -> Arc<dyn KeyCache> {
        self.cache.clone()
    }

    /// Takes an unlocked `vault` that was opened elsewhere (a worker thread did the key derivation
    /// or asked for Touch ID, which take time). With `remember` its key goes to the key cache; the
    /// cache's refusal, if any, comes back without undoing the unlock. A vault of another library
    /// folder is refused.
    pub fn adopt(&mut self, vault: Vault, remember: bool, now: Duration) -> Result<Option<KeyCacheError>> {
        if !vault.is_in(&self.root) {
            return Err(LibraryError::Invalid("that vault belongs to another library folder".to_string()));
        }
        let cache_error = if remember { self.cache.store(vault.id(), vault.key()).err() } else { None };
        self.vault = Some(vault);
        self.last_use = now;
        Ok(cache_error)
    }

    /// Unlocks with the passphrase. With `remember` the key goes into the key cache for Touch ID
    /// next time; the cache's refusal, if any, comes back without undoing the unlock.
    pub fn unlock_with_passphrase(&mut self, passphrase: &str, remember: bool, now: Duration) -> Result<Option<KeyCacheError>> {
        let vault = Vault::unlock(&self.root, passphrase)?;
        let cache_error = if remember { self.cache.store(vault.id(), vault.key()).err() } else { None };
        self.vault = Some(vault);
        self.last_use = now;
        Ok(cache_error)
    }

    /// Unlocks with the cached key (the system asks for Touch ID or the login password). A cached
    /// key that no longer fits the vault is removed.
    pub fn unlock_with_cache(&mut self, now: Duration) -> Result<()> {
        let vault = open_from_cache(&self.root, self.cache.as_ref())?;
        self.vault = Some(vault);
        self.last_use = now;
        Ok(())
    }

    /// Removes the key this Mac remembers, so unlocking asks for the passphrase again.
    pub fn forget_cached_key(&self) -> Result<()> {
        let id = Vault::id_of(&self.root)?;
        self.cache.forget(&id).map_err(|e| LibraryError::KeyCache(e.to_string()))
    }
}
