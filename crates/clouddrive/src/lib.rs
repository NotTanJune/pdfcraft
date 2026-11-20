//! pdfcraft-clouddrive: Google Drive and OneDrive, opt-in (L4).
//!
//! Nothing here runs until the person connects an account, with a client ID from their own
//! Google Cloud or Microsoft Entra project (PdfCraft ships none, and no server sits between the
//! app and the provider):
//!
//! - [`Connection::connect`] runs OAuth 2.0 authorization code with PKCE (S256) and a loopback
//!   redirect: it listens on `127.0.0.1`, hands the sign-in address to a callback that opens the
//!   system browser, accepts one bounded redirect carrying the code and a matching `state`, and
//!   trades the code for tokens. The refresh token goes to the system keychain
//!   (`pdfcraft_assist::secret::save_named`); access tokens stay in memory and are refreshed when
//!   they expire. [`Connection::disconnect`] revokes the token where the provider has an endpoint
//!   for it (Google; Microsoft has none for desktop apps) and deletes the saved account.
//! - [`Drive`] lists folders (with paging) and searches by name, downloads with a size bound,
//!   progress and cancellation, uploads new files and replaces content, refusing to overwrite a
//!   file that changed since it was opened. Google Docs and Sheets download as PDF exports.
//! - [`files::DriveFiles`] offers a drive through the shared `craft_cloud` contract.
//!
//! Every request is bounded (time, response size, page count), https only (plain http only to this
//! computer, for tests), sends no cookies, follows no redirect with a token attached, and keeps
//! tokens, codes and client secrets out of errors and logs. The web build (`wasm32`) has no
//! network client here: [`Connection::new`] fails with [`CloudError::Unsupported`].
//!
//! `mock::FakeCloud` (feature `mock`) serves the endpoints this crate uses from memory, so tests
//! never reach the Internet.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod drive;
mod error;
pub mod files;
pub mod settings;
mod types;
pub mod url;

#[cfg(not(target_arch = "wasm32"))]
mod account;
#[cfg(not(target_arch = "wasm32"))]
mod api;
#[cfg(not(target_arch = "wasm32"))]
mod browser;
#[cfg(not(target_arch = "wasm32"))]
mod google;
#[cfg(not(target_arch = "wasm32"))]
mod graph;
#[cfg(not(target_arch = "wasm32"))]
mod http;
#[cfg(not(target_arch = "wasm32"))]
pub mod oauth;
#[cfg(not(target_arch = "wasm32"))]
mod pkce;
#[cfg(target_arch = "wasm32")]
mod unsupported;

#[cfg(all(feature = "mock", not(target_arch = "wasm32")))]
pub mod mock;

#[cfg(not(target_arch = "wasm32"))]
pub use account::{Connection, Disconnected, Status};
#[cfg(target_arch = "wasm32")]
pub use unsupported::{Connection, Disconnected, Status};

#[cfg(not(target_arch = "wasm32"))]
pub use browser::open_in_browser;
/// The web build cannot open a browser from here.
#[cfg(target_arch = "wasm32")]
pub fn open_in_browser(_address: &str) -> Result<(), String> {
    Err("the web build cannot open a browser from here".into())
}

pub use drive::{
    DEFAULT_PAGE_SIZE, Drive, MAX_DOWNLOAD_BYTES, MAX_LIST_PAGES, MAX_PAGE_SIZE, MAX_UPLOAD_BYTES, SIMPLE_UPLOAD_BYTES, list_all, local_name,
    mime_for,
};
pub use error::CloudError;
pub use settings::{Endpoints, ProviderSettings, load_client, save_client};
pub use types::{Account, CancelToken, Download, Item, ListPage, OPENABLE_EXTENSIONS, Progress, ProviderKind, Query, is_openable, safe_file_name};

/// How long a person has to sign in in the browser.
pub const SIGN_IN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);
