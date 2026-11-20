//! Where the API key lives.
//!
//! macOS: the login keychain (a generic password, service [`SERVICE`]). Other systems have no
//! keychain wired in yet, so the key is kept for the running session only and [`store_kind`] says
//! so: the person is told it must be entered again next time, and nothing is written to a settings
//! file. `PDFCRAFT_AI_API_KEY` supplies a key on every system without storing it anywhere.
//!
//! A key is never logged and never part of an error message; [`ApiKey`]'s `Debug` hides it and its
//! memory is wiped when it drops.

use std::sync::atomic::{AtomicBool, Ordering};

use zeroize::Zeroizing;

/// The environment variable that supplies a key without storing it.
pub const KEY_ENV: &str = "PDFCRAFT_AI_API_KEY";

/// The keychain service name of the stored key.
pub const SERVICE: &str = "PdfCraft AI";

/// The keychain account name of the stored key.
pub const ACCOUNT: &str = "api-key";

/// The longest key accepted. Real keys are well under 300 bytes.
const MAX_KEY_BYTES: usize = 4096;

/// An API key. Prints as `ApiKey(<hidden>)`; wiped on drop.
#[derive(Clone)]
pub struct ApiKey(Zeroizing<String>);

impl ApiKey {
    /// A key from text a person typed or pasted: trimmed, and refused when empty, too long, or
    /// holding a space or control character (which would break the `Authorization` header).
    pub fn new(text: &str) -> Option<ApiKey> {
        let text = text.trim();
        if text.is_empty() || text.len() > MAX_KEY_BYTES || text.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return None;
        }
        Some(ApiKey(Zeroizing::new(text.to_string())))
    }

    /// The key itself, for the `Authorization` header only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(<hidden>)")
    }
}

/// How a saved key is kept on this system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Store {
    /// The operating system keychain: survives restarts, protected by the login.
    Keychain,
    /// Memory only: gone when the app quits. Used where no keychain is wired in.
    SessionOnly,
}

/// Why a key could not be read, saved or removed.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("that is not a usable API key (it is empty, too long, or has spaces or control characters)")]
    Invalid,
    #[error("the keychain refused the request: {0}")]
    Keychain(String),
}

/// How [`save`] keeps a key on this system.
pub fn store_kind() -> Store {
    if use_keychain() { Store::Keychain } else { Store::SessionOnly }
}

/// The key `PDFCRAFT_AI_API_KEY` names, when it is set to a usable one.
pub fn from_env() -> Option<ApiKey> {
    std::env::var(KEY_ENV).ok().and_then(|v| ApiKey::new(&v))
}

/// The key to use: `explicit` when given, else the environment's, else the stored one. `None`
/// means AI is off.
pub fn resolve(explicit: Option<&str>) -> Result<Option<ApiKey>, KeyError> {
    if let Some(text) = explicit {
        return ApiKey::new(text).map(Some).ok_or(KeyError::Invalid);
    }
    if let Some(key) = from_env() {
        return Ok(Some(key));
    }
    load()
}

/// The stored key, if there is one.
pub fn load() -> Result<Option<ApiKey>, KeyError> {
    imp::load()
}

/// Stores `text` as the key, replacing any earlier one.
pub fn save(text: &str) -> Result<(), KeyError> {
    let key = ApiKey::new(text).ok_or(KeyError::Invalid)?;
    imp::save(&key)
}

/// Removes the stored key. Removing none is not an error.
pub fn delete() -> Result<(), KeyError> {
    imp::delete()
}

/// Whether the keychain is used on this system and in this process.
fn use_keychain() -> bool {
    cfg!(target_os = "macos") && !MEMORY_ONLY.load(Ordering::Relaxed)
}

/// Keeps the key in this process's memory from now on, even where a keychain exists. For tests and
/// headless runs that must not read or write the person's keychain.
pub fn keep_in_memory_only() {
    MEMORY_ONLY.store(true, Ordering::Relaxed);
}

static MEMORY_ONLY: AtomicBool = AtomicBool::new(false);

/// The keychain service name of [`save_named`] secrets.
pub const NAMED_SERVICE: &str = "PdfCraft";

/// The longest named secret kept (a refresh token with the account's name and email is well under
/// 4 KiB).
const MAX_NAMED_BYTES: usize = 16 * 1024;

/// The longest account name accepted.
const MAX_ACCOUNT_BYTES: usize = 128;

/// A secret kept under a name of its own, for features other than Ask AI (a cloud drive's tokens).
/// Printing it hides the value and dropping it wipes the memory.
pub type Secret = Zeroizing<String>;

fn named_ok(account: &str) -> Result<(), KeyError> {
    let plain = !account.is_empty()
        && account.len() <= MAX_ACCOUNT_BYTES
        && account.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/'));
    if plain { Ok(()) } else { Err(KeyError::Invalid) }
}

/// Stores `value` under `account` in the same place [`save`] keeps the API key (the login keychain
/// on macOS, this session's memory elsewhere), replacing any earlier value. The service name differs
/// from the API key's, so the two never clash.
pub fn save_named(account: &str, value: &str) -> Result<(), KeyError> {
    named_ok(account)?;
    if value.is_empty() || value.len() > MAX_NAMED_BYTES {
        return Err(KeyError::Invalid);
    }
    imp::save_named(account, value)
}

/// The value stored under `account`, if there is one.
pub fn load_named(account: &str) -> Result<Option<Secret>, KeyError> {
    named_ok(account)?;
    imp::load_named(account)
}

/// Removes the value stored under `account`. Removing none is not an error.
pub fn delete_named(account: &str) -> Result<(), KeyError> {
    named_ok(account)?;
    imp::delete_named(account)
}

mod imp {
    #[cfg(target_os = "macos")]
    use super::use_keychain;
    use super::{ApiKey, KeyError};

    pub(super) fn load() -> Result<Option<ApiKey>, KeyError> {
        #[cfg(target_os = "macos")]
        if use_keychain() {
            return keychain::load();
        }
        session::load()
    }

    pub(super) fn save(key: &ApiKey) -> Result<(), KeyError> {
        #[cfg(target_os = "macos")]
        if use_keychain() {
            return keychain::save(key);
        }
        session::save(key)
    }

    pub(super) fn delete() -> Result<(), KeyError> {
        #[cfg(target_os = "macos")]
        if use_keychain() {
            return keychain::delete();
        }
        session::delete()
    }

    pub(super) fn save_named(account: &str, value: &str) -> Result<(), KeyError> {
        #[cfg(target_os = "macos")]
        if use_keychain() {
            return keychain::save_named(account, value);
        }
        session::save_named(account, value)
    }

    pub(super) fn load_named(account: &str) -> Result<Option<super::Secret>, KeyError> {
        #[cfg(target_os = "macos")]
        if use_keychain() {
            return keychain::load_named(account);
        }
        session::load_named(account)
    }

    pub(super) fn delete_named(account: &str) -> Result<(), KeyError> {
        #[cfg(target_os = "macos")]
        if use_keychain() {
            return keychain::delete_named(account);
        }
        session::delete_named(account)
    }

    #[cfg(target_os = "macos")]
    mod keychain {
        use super::super::{ACCOUNT, ApiKey, KeyError, SERVICE};
        use security_framework::passwords::{delete_generic_password, get_generic_password, set_generic_password};

        /// `errSecItemNotFound`.
        const NOT_FOUND: i32 = -25300;

        fn refused(e: security_framework::base::Error) -> KeyError {
            KeyError::Keychain(e.to_string())
        }

        pub(super) fn load() -> Result<Option<ApiKey>, KeyError> {
            match get_generic_password(SERVICE, ACCOUNT) {
                Ok(bytes) => Ok(std::str::from_utf8(&bytes).ok().and_then(ApiKey::new)),
                Err(e) if e.code() == NOT_FOUND => Ok(None),
                Err(e) => Err(refused(e)),
            }
        }

        pub(super) fn save(key: &ApiKey) -> Result<(), KeyError> {
            set_generic_password(SERVICE, ACCOUNT, key.expose().as_bytes()).map_err(refused)
        }

        pub(super) fn delete() -> Result<(), KeyError> {
            match delete_generic_password(SERVICE, ACCOUNT) {
                Ok(()) => Ok(()),
                Err(e) if e.code() == NOT_FOUND => Ok(()),
                Err(e) => Err(refused(e)),
            }
        }

        pub(super) fn save_named(account: &str, value: &str) -> Result<(), KeyError> {
            set_generic_password(super::super::NAMED_SERVICE, account, value.as_bytes()).map_err(refused)
        }

        pub(super) fn load_named(account: &str) -> Result<Option<super::super::Secret>, KeyError> {
            match get_generic_password(super::super::NAMED_SERVICE, account) {
                Ok(bytes) => Ok(std::str::from_utf8(&bytes).ok().filter(|t| !t.is_empty()).map(|t| super::super::Secret::new(t.to_string()))),
                Err(e) if e.code() == NOT_FOUND => Ok(None),
                Err(e) => Err(refused(e)),
            }
        }

        pub(super) fn delete_named(account: &str) -> Result<(), KeyError> {
            match delete_generic_password(super::super::NAMED_SERVICE, account) {
                Ok(()) => Ok(()),
                Err(e) if e.code() == NOT_FOUND => Ok(()),
                Err(e) => Err(refused(e)),
            }
        }
    }

    /// Without a keychain (or when told not to use it): a key in memory until the app quits.
    mod session {
        use super::super::{ApiKey, KeyError};
        use std::sync::Mutex;

        static SESSION: Mutex<Option<ApiKey>> = Mutex::new(None);

        fn slot() -> std::sync::MutexGuard<'static, Option<ApiKey>> {
            SESSION.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        pub(super) fn load() -> Result<Option<ApiKey>, KeyError> {
            Ok(slot().clone())
        }

        pub(super) fn save(key: &ApiKey) -> Result<(), KeyError> {
            *slot() = Some(key.clone());
            Ok(())
        }

        pub(super) fn delete() -> Result<(), KeyError> {
            *slot() = None;
            Ok(())
        }

        static NAMED: Mutex<Vec<(String, super::super::Secret)>> = Mutex::new(Vec::new());

        fn named() -> std::sync::MutexGuard<'static, Vec<(String, super::super::Secret)>> {
            NAMED.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        pub(super) fn save_named(account: &str, value: &str) -> Result<(), KeyError> {
            let mut all = named();
            all.retain(|(a, _)| a != account);
            all.push((account.to_string(), super::super::Secret::new(value.to_string())));
            Ok(())
        }

        pub(super) fn load_named(account: &str) -> Result<Option<super::super::Secret>, KeyError> {
            Ok(named().iter().find(|(a, _)| a == account).map(|(_, v)| v.clone()))
        }

        pub(super) fn delete_named(account: &str) -> Result<(), KeyError> {
            named().retain(|(a, _)| a != account);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_trimmed_and_checked() {
        assert_eq!(ApiKey::new("  sk-abc123 \n").map(|k| k.expose().to_string()).as_deref(), Some("sk-abc123"));
        assert!(ApiKey::new("").is_none());
        assert!(ApiKey::new("   ").is_none());
        assert!(ApiKey::new("sk abc").is_none(), "a space would split the header value");
        assert!(ApiKey::new("sk-abc\r\nX-Evil: 1").is_none(), "a line break would inject a header");
        assert!(ApiKey::new(&"k".repeat(MAX_KEY_BYTES + 1)).is_none());
    }

    #[test]
    fn a_key_does_not_print() {
        let key = ApiKey::new("sk-very-secret").unwrap();
        assert!(!format!("{key:?}").contains("secret"));
        assert!(!format!("{:?}", Some(&key)).contains("secret"));
    }

    #[test]
    fn an_explicit_key_wins_and_a_bad_one_is_refused() {
        assert_eq!(resolve(Some("sk-explicit")).unwrap().unwrap().expose(), "sk-explicit");
        assert!(matches!(resolve(Some("not usable")), Err(KeyError::Invalid)));
    }

    #[test]
    fn a_named_secret_is_kept_apart_from_the_api_key() {
        keep_in_memory_only();
        save_named("test/secret-a", "{\"refresh\": \"abc def\"}").unwrap();
        assert_eq!(load_named("test/secret-a").unwrap().as_deref().map(String::as_str), Some("{\"refresh\": \"abc def\"}"));
        assert!(load_named("test/secret-b").unwrap().is_none());
        save_named("test/secret-a", "second").unwrap();
        assert_eq!(load_named("test/secret-a").unwrap().as_deref().map(String::as_str), Some("second"));
        delete_named("test/secret-a").unwrap();
        delete_named("test/secret-a").unwrap();
        assert!(load_named("test/secret-a").unwrap().is_none());
        assert!(matches!(save_named("bad name", "x"), Err(KeyError::Invalid)));
        assert!(matches!(save_named("ok", ""), Err(KeyError::Invalid)));
        assert!(matches!(save_named("ok", &"x".repeat(MAX_NAMED_BYTES + 1)), Err(KeyError::Invalid)));
    }
}
