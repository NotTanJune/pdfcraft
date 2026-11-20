//! pdfcraft-assist: opt-in network helpers for reading (L4).
//!
//! Two things leave the machine from here, and only when the person turned them on:
//!
//! - [`chat`]: a question about text selected on a page, sent to an OpenAI-compatible Chat
//!   Completions endpoint with the person's own API key ([`secret`] keeps the key in the operating
//!   system keychain). Off until a key is set.
//! - [`preview`]: the title and description of a web page a link points to, for a hover card. Off
//!   unless the person turned on "Fetch web link previews".
//!
//! Neither runs on the web build (`wasm32`): their calls return [`chat::AiError::Unsupported`] and
//! [`preview::PreviewError::Unsupported`]. Nothing here reads a document, draws, or knows egui;
//! callers pass the text. Every request is bounded (time, size, redirects), sends no cookies and
//! never logs the API key.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod chat;
pub mod cite;
pub mod preview;
pub mod secret;

#[cfg(not(target_arch = "wasm32"))]
mod net;

#[cfg(all(feature = "mock", not(target_arch = "wasm32")))]
pub mod mock;

pub use chat::{AiConfig, AiError, Answer, CancelToken, Question, ask};
pub use secret::ApiKey;
