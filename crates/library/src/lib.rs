//! pdfcraft-library: the document library and the encrypted vault (L4).
//!
//! - [`Library`] is an index of documents (title, page count, tags, times, size, hash, location)
//!   kept as JSON in a library folder, with the documents' text in a second file for full-text
//!   search. The folder is meant to be synced between the owner's Macs (iCloud Drive's `PdfCraft/`
//!   by default, see [`folder`]), so locations inside it are relative, writes are atomic, and
//!   concurrent edits and sync conflict copies merge per entry.
//! - [`vault`] keeps encrypted documents in the `Vault/` folder inside the library folder:
//!   AES-256-GCM per file, a random vault key wrapped by an Argon2id key from a passphrase, and on
//!   a Mac the unwrapped key cached in the keychain behind Touch ID.
//!
//! Nothing here knows about egui, opens a socket, or writes decrypted document bytes to disk.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod error;
pub mod extract;
pub mod folder;
pub mod index;
pub mod search;
pub mod store;
pub mod text;
pub mod util;
pub mod vault;

pub use error::{LibraryError, Result};
pub use extract::Facts;
pub use index::{Entry, Index, Location};
pub use store::{AddOptions, Added, Hit, Library, Mode, Snapshot};
pub use vault::{KdfParams, Vault, VaultAddOptions, VaultEntry, VaultSession, VaultSnapshot};
