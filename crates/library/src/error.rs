//! What can go wrong, in words a person can act on.

/// Why a library or vault operation failed.
#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    /// A file could not be read, written or moved. The message names the file.
    #[error("{0}")]
    Io(String),
    /// A request that makes no sense (an empty name, a bad id, a passphrase that is too short).
    #[error("{0}")]
    Invalid(String),
    /// The thing asked for is not there.
    #[error("{0}")]
    NotFound(String),
    /// A file is not what it should be: truncated, edited, or made by something else.
    #[error("{0}")]
    Damaged(String),
    /// A file was written by a later version of PdfCraft than this one.
    #[error("this file was made by a newer PdfCraft: update PdfCraft to open it")]
    TooNew,
    /// A file is still being downloaded by the sync service.
    #[error("{0} is still downloading from the sync service: wait for it to finish, then try again")]
    NotDownloaded(String),
    /// The vault is locked: unlock it with its passphrase first.
    #[error("the vault is locked: unlock it with its passphrase first")]
    Locked,
    /// No vault in the library folder.
    #[error("there is no vault in this library folder: create one first")]
    NoVault,
    /// A vault already is in the library folder.
    #[error("a vault already exists in this library folder")]
    VaultExists,
    /// The passphrase (or the cached key) does not open the vault.
    #[error("that passphrase does not open the vault")]
    WrongPassphrase,
    /// The key cache (the macOS keychain) refused or is not available.
    #[error("{0}")]
    KeyCache(String),
    /// The system gave no random bytes.
    #[error("the system could not provide random numbers: {0}")]
    Random(String),
}

pub type Result<T> = std::result::Result<T, LibraryError>;
