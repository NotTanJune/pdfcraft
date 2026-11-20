//! One error type for everything a cloud drive can go wrong with.
//!
//! The cases are the ones a caller acts on differently: a missing account asks the person to
//! connect, a conflict asks before overwriting, a cancelled job says nothing. No variant ever
//! carries a token, a client secret or an authorization code: messages come from this crate's own
//! wording or from the provider's `error.message`, cut to one short line.

#[cfg(not(target_arch = "wasm32"))]
use crate::types::ProviderKind;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CloudError {
    #[error("cloud drives need the desktop app; this build runs in a browser")]
    Unsupported,
    #[error("{0} is not connected: connect it in Cloud drives first")]
    NotConnected(&'static str),
    #[error("{0} is not set up: enter its client ID in Cloud drives first")]
    NotConfigured(&'static str),
    #[error("{0} refused the saved sign-in: connect it again")]
    Unauthenticated(&'static str),
    #[error("not allowed: {0}")]
    Forbidden(String),
    #[error("not found: {0}")]
    NotFound(String),
    /// The file on the drive is not the one that was opened: somebody saved it since.
    #[error("{name} changed on the drive since you opened it")]
    Conflict { name: String, current_version: Option<String>, modified: Option<String> },
    #[error("{what} is larger than the {limit} byte limit")]
    TooLarge { what: String, limit: u64 },
    #[error("cancelled")]
    Cancelled,
    #[error("could not reach the service: {0}")]
    Network(String),
    #[error("the service answered {status}: {message}")]
    Http { status: u16, message: String },
    #[error("unexpected answer from the service: {0}")]
    BadResponse(String),
    #[error("{0}")]
    Invalid(String),
    /// Signing in did not finish: refused in the browser, timed out, or the answer did not match.
    #[error("sign-in did not finish: {0}")]
    SignIn(String),
    #[error("the keychain refused the request: {0}")]
    Keychain(String),
    #[error("could not use the file: {0}")]
    Io(String),
}

impl CloudError {
    /// Whether signing in again is the fix.
    pub fn needs_sign_in(&self) -> bool {
        matches!(self, CloudError::NotConnected(_) | CloudError::Unauthenticated(_))
    }

    /// Whether the job was stopped by the person.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, CloudError::Cancelled)
    }
}

impl From<pdfcraft_assist::secret::KeyError> for CloudError {
    fn from(e: pdfcraft_assist::secret::KeyError) -> Self {
        CloudError::Keychain(e.to_string())
    }
}

impl From<CloudError> for craft_cloud::Error {
    fn from(e: CloudError) -> Self {
        use craft_cloud::Error as C;
        match e {
            CloudError::Network(m) => C::Unreachable(m),
            CloudError::NotConnected(_) | CloudError::Unauthenticated(_) | CloudError::NotConfigured(_) => C::Unauthenticated,
            CloudError::Forbidden(m) => C::Forbidden(m),
            CloudError::NotFound(id) => C::NotFound { what: "file", id },
            CloudError::Conflict { name, .. } => C::Conflict(name),
            CloudError::Unsupported => C::Unsupported("cloud drives on this build"),
            CloudError::BadResponse(detail) => C::Malformed { what: "response", detail },
            CloudError::Invalid(detail) => C::Invalid { what: "request", detail },
            other => C::Unreachable(other.to_string()),
        }
    }
}

/// The provider's display name, for messages.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn label(kind: ProviderKind) -> &'static str {
    kind.label()
}

/// One line of `text`, whitespace collapsed and at most `max` characters, for an error message.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn one_line(text: &str, max: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = joined.chars();
    let head: String = chars.by_ref().take(max).filter(|c| !c.is_control()).collect();
    if chars.next().is_some() { format!("{head}…") } else { head }
}
