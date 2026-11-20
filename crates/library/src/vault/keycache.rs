//! Where a Mac keeps the unwrapped vault key after the first passphrase unlock, so that later
//! unlocks only ask for Touch ID (or the login password).
//!
//! macOS: a generic password in the data protection keychain whose access control requires user
//! presence (`kSecAccessControlUserPresence`: Touch ID, Apple Watch or the login password), is
//! available only while the Mac is unlocked with a passcode set, stays on this device
//! (`...ThisDeviceOnly`, not synchronizable) and is never in iCloud Keychain. Reading it makes
//! the system show the Touch ID prompt, which `reason` does not change (the system words it).
//!
//! The data protection keychain needs a signed app that carries a keychain access group. A build
//! without one (a development build run from `cargo`) is refused by the system with
//! `errSecMissingEntitlement`; that comes back as [`KeyCacheError::Unavailable`] and the vault
//! keeps asking for the passphrase. Nothing weaker is used instead.
//!
//! Other systems have no key cache: the passphrase is asked every time.

use std::collections::HashMap;
use std::sync::Mutex;

use super::crypto::VaultKey;

/// Why the cache could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyCacheError {
    /// Not available here (not macOS, or the build lacks the keychain entitlement).
    #[error("{0}")]
    Unavailable(String),
    /// The person dismissed the Touch ID or password prompt.
    #[error("the Touch ID or password prompt was cancelled")]
    Cancelled,
    /// The system refused for another reason.
    #[error("the keychain refused the request: {0}")]
    Refused(String),
}

/// A store for the unwrapped vault key of one vault, on this device only.
pub trait KeyCache: Send + Sync {
    /// What kind of store this is, for the interface: a short name such as "Touch ID".
    fn describe(&self) -> &'static str;
    /// Whether this store can work at all on this system.
    fn is_supported(&self) -> bool;
    /// Whether a key is stored for `vault_id`, without asking for authentication.
    fn has_key(&self, vault_id: &str) -> bool;
    /// The key for `vault_id`: this is where the system asks for Touch ID. `None` when nothing is
    /// stored.
    fn load(&self, vault_id: &str) -> Result<Option<VaultKey>, KeyCacheError>;
    /// Stores `key` for `vault_id`, replacing an earlier one.
    fn store(&self, vault_id: &str, key: &VaultKey) -> Result<(), KeyCacheError>;
    /// Removes the key for `vault_id`. Removing none is not an error.
    fn forget(&self, vault_id: &str) -> Result<(), KeyCacheError>;
}

/// The cache this system offers: the keychain on macOS, none elsewhere.
pub fn default_cache() -> Box<dyn KeyCache> {
    #[cfg(target_os = "macos")]
    {
        Box::new(keychain::KeychainCache)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(NoCache)
    }
}

/// No cache: the passphrase is asked every time.
#[derive(Debug, Default)]
pub struct NoCache;

impl KeyCache for NoCache {
    fn describe(&self) -> &'static str {
        "none"
    }
    fn is_supported(&self) -> bool {
        false
    }
    fn has_key(&self, _: &str) -> bool {
        false
    }
    fn load(&self, _: &str) -> Result<Option<VaultKey>, KeyCacheError> {
        Ok(None)
    }
    fn store(&self, _: &str, _: &VaultKey) -> Result<(), KeyCacheError> {
        Err(KeyCacheError::Unavailable("this system has no secure key store wired in: the passphrase is asked every time".to_string()))
    }
    fn forget(&self, _: &str) -> Result<(), KeyCacheError> {
        Ok(())
    }
}

/// A cache in this process's memory, for tests and headless runs that must not touch the real
/// keychain. It behaves like the keychain: the key survives a lock, not the process.
#[derive(Default)]
pub struct MemoryCache {
    keys: Mutex<HashMap<String, VaultKey>>,
    /// Loads that were asked for (each one is a Touch ID prompt on a Mac).
    loads: Mutex<u32>,
}

impl MemoryCache {
    pub fn new() -> MemoryCache {
        MemoryCache::default()
    }

    /// How many times [`KeyCache::load`] was called.
    pub fn loads(&self) -> u32 {
        self.loads.lock().map_or(0, |n| *n)
    }
}

impl KeyCache for MemoryCache {
    fn describe(&self) -> &'static str {
        "memory"
    }
    fn is_supported(&self) -> bool {
        true
    }
    fn has_key(&self, vault_id: &str) -> bool {
        self.keys.lock().is_ok_and(|k| k.contains_key(vault_id))
    }
    fn load(&self, vault_id: &str) -> Result<Option<VaultKey>, KeyCacheError> {
        if let Ok(mut n) = self.loads.lock() {
            *n += 1;
        }
        Ok(self.keys.lock().map_err(|_| KeyCacheError::Refused("poisoned".into()))?.get(vault_id).cloned())
    }
    fn store(&self, vault_id: &str, key: &VaultKey) -> Result<(), KeyCacheError> {
        self.keys.lock().map_err(|_| KeyCacheError::Refused("poisoned".into()))?.insert(vault_id.to_string(), key.clone());
        Ok(())
    }
    fn forget(&self, vault_id: &str) -> Result<(), KeyCacheError> {
        self.keys.lock().map_err(|_| KeyCacheError::Refused("poisoned".into()))?.remove(vault_id);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
pub(crate) mod keychain {
    use security_framework::access_control::{ProtectionMode, SecAccessControl};
    use security_framework::passwords::{
        AccessControlOptions, PasswordOptions, delete_generic_password_options, generic_password, set_generic_password_options,
    };

    use super::{KeyCache, KeyCacheError, VaultKey};

    /// The keychain service of the cached keys.
    pub const SERVICE: &str = "PdfCraft Vault";

    /// `errSecItemNotFound`.
    const NOT_FOUND: i32 = -25300;
    /// `errSecUserCanceled`.
    const USER_CANCELED: i32 = -128;
    /// `errSecAuthFailed`.
    const AUTH_FAILED: i32 = -25293;
    /// `errSecMissingEntitlement`: the app is not signed with a keychain access group.
    const MISSING_ENTITLEMENT: i32 = -34018;
    /// `errSecInteractionNotAllowed`: no screen to prompt on.
    const INTERACTION_NOT_ALLOWED: i32 = -25308;

    /// The keychain with user-presence access control.
    #[derive(Debug, Default)]
    pub struct KeychainCache;

    fn options(account: &str) -> PasswordOptions {
        let mut options = PasswordOptions::new_generic_password(SERVICE, account);
        options.use_protected_keychain();
        // Never in iCloud Keychain: the key stays on this Mac.
        options.set_access_synchronized(Some(false));
        options
    }

    fn map(e: security_framework::base::Error) -> KeyCacheError {
        match e.code() {
            USER_CANCELED | AUTH_FAILED => KeyCacheError::Cancelled,
            MISSING_ENTITLEMENT => KeyCacheError::Unavailable(
                "this build of PdfCraft is not signed with a keychain access group, so macOS will not keep the vault key behind Touch ID: the passphrase is asked every time".to_string(),
            ),
            INTERACTION_NOT_ALLOWED => KeyCacheError::Refused("macOS could not show the Touch ID prompt".to_string()),
            _ => KeyCacheError::Refused(e.to_string()),
        }
    }

    impl KeyCache for KeychainCache {
        fn describe(&self) -> &'static str {
            "Touch ID or the login password"
        }

        fn is_supported(&self) -> bool {
            true
        }

        fn has_key(&self, vault_id: &str) -> bool {
            use security_framework::item::{ItemClass, ItemSearchOptions, Limit};
            // Attributes only, never the data: reading the data is what asks for Touch ID, this
            // does not. A failure to look counts as "no key". The key lives in the data protection
            // keychain (`options`), and a search without `ignore_legacy_keychains` looks only in the
            // file-based login keychain, where it never is.
            let mut search = ItemSearchOptions::new();
            search
                .class(ItemClass::generic_password())
                .service(SERVICE)
                .account(vault_id)
                .ignore_legacy_keychains()
                .limit(Limit::Max(1))
                .load_attributes(true);
            search.search().is_ok_and(|found| !found.is_empty())
        }

        fn load(&self, vault_id: &str) -> Result<Option<VaultKey>, KeyCacheError> {
            match generic_password(options(vault_id)) {
                Ok(bytes) => Ok(VaultKey::from_bytes(&bytes)),
                Err(e) if e.code() == NOT_FOUND => Ok(None),
                Err(e) => Err(map(e)),
            }
        }

        fn store(&self, vault_id: &str, key: &VaultKey) -> Result<(), KeyCacheError> {
            let control = SecAccessControl::create_with_protection(
                Some(ProtectionMode::AccessibleWhenPasscodeSetThisDeviceOnly),
                AccessControlOptions::USER_PRESENCE.bits(),
            )
            .map_err(map)?;
            // Replace an earlier key: deleting an item does not ask for authentication.
            self.forget(vault_id)?;
            let mut options = options(vault_id);
            options.set_access_control(control);
            set_generic_password_options(key.expose(), options).map_err(map)
        }

        fn forget(&self, vault_id: &str) -> Result<(), KeyCacheError> {
            match delete_generic_password_options(options(vault_id)) {
                Ok(()) => Ok(()),
                // A build the system will not let keep a key has none to forget.
                Err(e) if e.code() == NOT_FOUND || e.code() == MISSING_ENTITLEMENT => Ok(()),
                Err(e) => Err(map(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memory_cache_stores_loads_and_forgets() {
        let cache = MemoryCache::new();
        let key = VaultKey::from_bytes(&[3u8; 32]).unwrap();
        assert!(cache.load("v").unwrap().is_none() && !cache.has_key("v"));
        cache.store("v", &key).unwrap();
        assert!(cache.has_key("v") && !cache.has_key("other"));
        assert_eq!(cache.load("v").unwrap().unwrap().expose(), key.expose());
        cache.forget("v").unwrap();
        cache.forget("v").unwrap();
        assert!(cache.load("v").unwrap().is_none());
        assert_eq!(cache.loads(), 3);
    }

    #[test]
    fn no_cache_refuses_to_store_and_says_why() {
        let cache = NoCache;
        assert!(!cache.is_supported());
        assert!(matches!(cache.store("v", &VaultKey::from_bytes(&[1u8; 32]).unwrap()), Err(KeyCacheError::Unavailable(_))));
        assert!(cache.load("v").unwrap().is_none());
    }

    /// Stores and removes a throwaway key in the real macOS keychain. Never run by `cargo test`: it
    /// touches the person's keychain, and whether it succeeds depends on the binary being signed
    /// with a keychain access group. Run it by hand with `-- --ignored` to see what this build gets.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "touches the real macOS keychain"]
    fn the_real_keychain_accepts_or_refuses_a_key_with_user_presence() {
        let cache = keychain::KeychainCache;
        let id = format!("pdfcraft-test-{}", crate::util::now_ms());
        let stored = cache.store(&id, &VaultKey::from_bytes(&[9u8; 32]).unwrap());
        eprintln!("store: {stored:?}");
        if stored.is_ok() {
            eprintln!("has_key: {}", cache.has_key(&id));
        }
        eprintln!("forget: {:?}", cache.forget(&id));
        assert!(!cache.has_key(&id), "the throwaway key is gone");
    }
}
