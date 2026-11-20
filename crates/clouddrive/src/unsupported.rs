//! The web build has no sockets or keychain: every call says so.

use std::time::Duration;

use crate::drive::Drive;
use crate::error::CloudError;
use crate::settings::ProviderSettings;
use crate::types::{Account, CancelToken, ProviderKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Disconnected {
    pub was_connected: bool,
    pub revoked: bool,
    pub can_revoke: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub kind: ProviderKind,
    pub configured: bool,
    pub account: Option<Account>,
}

pub struct Connection;

impl Connection {
    pub fn new(_kind: ProviderKind, _settings: &ProviderSettings) -> Result<Connection, CloudError> {
        Err(CloudError::Unsupported)
    }

    pub fn kind(&self) -> ProviderKind {
        ProviderKind::Google
    }

    pub fn status(kind: ProviderKind, _settings: &ProviderSettings) -> Result<Status, CloudError> {
        Ok(Status { kind, configured: false, account: None })
    }

    pub fn connect(
        &self,
        _open_browser: &dyn Fn(&str) -> Result<(), String>,
        _cancel: &CancelToken,
        _timeout: Duration,
    ) -> Result<Account, CloudError> {
        Err(CloudError::Unsupported)
    }

    pub fn disconnect(&self) -> Result<Disconnected, CloudError> {
        Err(CloudError::Unsupported)
    }

    pub fn drive(&self) -> Result<Box<dyn Drive>, CloudError> {
        Err(CloudError::Unsupported)
    }
}
