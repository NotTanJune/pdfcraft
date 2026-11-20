//! A connected account: the refresh token in the keychain, the access token in memory, and the
//! steps that connect, refresh and disconnect.
//!
//! The keychain item `cloud/<provider>` holds one JSON object: the client ID the tokens belong to,
//! the refresh token, and the account's email and name for the label. If the client ID in
//! Settings is later changed, the saved tokens no longer match it and the account reads as not
//! connected.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::drive::Drive;
use crate::error::{CloudError, label};
use crate::http::Http;
use crate::oauth;
use crate::pkce::{self, Pkce};
use crate::settings::{Endpoints, ProviderSettings};
use crate::types::{Account, CancelToken, ProviderKind};

/// An access token is refreshed this long before it expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// What the keychain holds for one provider.
#[derive(Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    v: u32,
    client_id: String,
    refresh_token: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

impl Drop for Stored {
    fn drop(&mut self) {
        self.refresh_token.zeroize();
    }
}

/// The keychain item of `kind` (and `profile`, for tests).
fn secret_name(kind: ProviderKind, profile: &str) -> String {
    if profile.is_empty() { format!("cloud/{}", kind.id()) } else { format!("cloud/{}.{profile}", kind.id()) }
}

fn load_stored(name: &str, client_id: &str) -> Result<Option<Stored>, CloudError> {
    let Some(json) = pdfcraft_assist::secret::load_named(name)? else { return Ok(None) };
    let stored: Option<Stored> = serde_json::from_str(&json).ok();
    Ok(stored.filter(|s| s.client_id == client_id && !s.refresh_token.is_empty()))
}

/// Access tokens by account and token endpoint, shared by every job in the process so a listing
/// and a download do not each refresh.
static ACCESS: Mutex<Option<TokenCache>> = Mutex::new(None);

/// Cache key to (access token, valid until).
type TokenCache = HashMap<String, (Zeroizing<String>, Instant)>;

fn with_cache<T>(f: impl FnOnce(&mut TokenCache) -> T) -> T {
    let mut guard = ACCESS.lock().unwrap_or_else(PoisonError::into_inner);
    f(guard.get_or_insert_with(HashMap::new))
}

/// Gives drive calls their access token, refreshing it when it has expired or was refused.
pub(crate) struct TokenSource {
    kind: ProviderKind,
    settings: ProviderSettings,
    endpoints: Endpoints,
    http: Arc<Http>,
    name: String,
    key: String,
}

impl TokenSource {
    fn new(kind: ProviderKind, settings: &ProviderSettings, http: Arc<Http>) -> TokenSource {
        let endpoints = settings.endpoints_for(kind);
        let name = secret_name(kind, &settings.profile);
        let key = format!("{name}|{}|{}", settings.client_id, endpoints.token);
        TokenSource { kind, settings: settings.clone(), endpoints, http, name, key }
    }

    /// A usable access token.
    pub fn access(&self) -> Result<Zeroizing<String>, CloudError> {
        if let Some(token) = with_cache(|c| c.get(&self.key).filter(|(_, until)| *until > Instant::now()).map(|(t, _)| t.clone())) {
            return Ok(token);
        }
        self.refresh()
    }

    /// A new access token from the refresh token.
    pub fn refresh(&self) -> Result<Zeroizing<String>, CloudError> {
        let stored = load_stored(&self.name, &self.settings.client_id)?.ok_or(CloudError::NotConnected(label(self.kind)))?;
        let response = match oauth::refresh(&self.http, self.kind, &self.settings, &self.endpoints, &stored.refresh_token) {
            Ok(r) => r,
            Err(e) => {
                with_cache(|c| c.remove(&self.key));
                return Err(e);
            }
        };
        // Microsoft rotates the refresh token; the new one replaces the old.
        if let Some(new) = &response.refresh_token
            && **new != stored.refresh_token
        {
            self.save(new, stored.email.clone(), stored.name.clone())?;
        }
        self.remember(&response.access_token, response.expires_in);
        Ok(response.access_token.clone())
    }

    fn remember(&self, token: &Zeroizing<String>, lifetime: Duration) {
        let until = Instant::now() + lifetime.saturating_sub(EXPIRY_MARGIN).max(Duration::from_secs(5));
        with_cache(|c| {
            c.insert(self.key.clone(), (token.clone(), until));
        });
    }

    fn save(&self, refresh_token: &str, email: Option<String>, name: Option<String>) -> Result<(), CloudError> {
        let stored = Stored { v: 1, client_id: self.settings.client_id.clone(), refresh_token: refresh_token.to_string(), email, name };
        let json = Zeroizing::new(serde_json::to_string(&stored).map_err(|e| CloudError::Invalid(e.to_string()))?);
        Ok(pdfcraft_assist::secret::save_named(&self.name, &json)?)
    }

    fn forget_access(&self) {
        with_cache(|c| c.remove(&self.key));
    }
}

/// How disconnecting went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Disconnected {
    /// Whether an account was saved.
    pub was_connected: bool,
    /// Whether the provider was told to revoke the token and accepted it. Microsoft has no such
    /// endpoint for a desktop app: the saved tokens are deleted and the permission stays listed
    /// at account.microsoft.com until the person removes it there.
    pub revoked: bool,
    /// Whether this provider has a revocation endpoint at all.
    pub can_revoke: bool,
}

/// What a person can see without the network: the client is set, and who is connected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub kind: ProviderKind,
    pub configured: bool,
    pub account: Option<Account>,
}

/// One provider, one client, one account.
pub struct Connection {
    kind: ProviderKind,
    settings: ProviderSettings,
    http: Arc<Http>,
}

impl Connection {
    /// Checks `settings` and prepares a connection. Starts no network traffic.
    pub fn new(kind: ProviderKind, settings: &ProviderSettings) -> Result<Connection, CloudError> {
        Ok(Connection { kind, settings: settings.checked(kind)?, http: Arc::new(Http::default()) })
    }

    pub fn kind(&self) -> ProviderKind {
        self.kind
    }

    fn tokens(&self) -> TokenSource {
        TokenSource::new(self.kind, &self.settings, self.http.clone())
    }

    /// Whether the client is set and an account is saved. Reads the keychain; no network.
    pub fn status(kind: ProviderKind, settings: &ProviderSettings) -> Result<Status, CloudError> {
        if !settings.is_configured() {
            return Ok(Status { kind, configured: false, account: None });
        }
        let stored = load_stored(&secret_name(kind, &settings.profile), settings.client_id.trim())?;
        let account = stored.as_ref().map(|s| Account { provider: kind, email: s.email.clone(), name: s.name.clone() });
        Ok(Status { kind, configured: true, account })
    }

    /// Signs in through the browser. `open_browser` is given the address to open and returns an
    /// error text if it could not. Blocks until the person finishes, `timeout` passes or `cancel`
    /// is set: run it off the UI thread.
    pub fn connect(&self, open_browser: &dyn Fn(&str) -> Result<(), String>, cancel: &CancelToken, timeout: Duration) -> Result<Account, CloudError> {
        let endpoints = self.settings.endpoints_for(self.kind);
        let loopback = oauth::Loopback::bind()?;
        let redirect = loopback.redirect_uri(&endpoints.redirect_host);
        let pkce = Pkce::new()?;
        let state = pkce::new_state()?;
        let address = oauth::authorize_url(self.kind, &self.settings, &endpoints, &redirect, &state, &pkce.challenge);
        cancel.check()?;
        open_browser(&address).map_err(|why| CloudError::SignIn(format!("could not open your browser ({why})")))?;
        let code = loopback.wait(&state, cancel, timeout)?;
        drop(loopback);
        let response = oauth::exchange_code(&self.http, self.kind, &self.settings, &endpoints, &code, pkce.verifier(), &redirect)?;
        let refresh_token = response.refresh_token.clone().ok_or_else(|| {
            CloudError::SignIn(format!(
                "{} did not return a refresh token, so PdfCraft could not stay connected. Remove PdfCraft's access in your account's security settings and connect again",
                label(self.kind)
            ))
        })?;
        let tokens = self.tokens();
        tokens.remember(&response.access_token, response.expires_in);
        // Ask who signed in before saving anything: it also proves the files API is switched on.
        let drive = self.drive_with(tokens)?;
        let mut account = drive.account()?;
        if account.email.is_none()
            && let Some(id_token) = &response.id_token
        {
            let (email, name) = oauth::id_token_label(id_token);
            account.email = email;
            account.name = account.name.or(name);
        }
        let tokens = self.tokens();
        tokens.save(&refresh_token, account.email.clone(), account.name.clone())?;
        Ok(account)
    }

    /// Revokes the token where the provider allows it, then deletes the saved account.
    pub fn disconnect(&self) -> Result<Disconnected, CloudError> {
        let endpoints = self.settings.endpoints_for(self.kind);
        let name = secret_name(self.kind, &self.settings.profile);
        let stored = load_stored(&name, &self.settings.client_id)?;
        let can_revoke = endpoints.revoke.is_some();
        let mut revoked = false;
        if let Some(stored) = &stored {
            // The token is forgotten locally whatever the provider answers.
            revoked = oauth::revoke(&self.http, &endpoints, &stored.refresh_token).unwrap_or(false);
        }
        pdfcraft_assist::secret::delete_named(&name)?;
        self.tokens().forget_access();
        Ok(Disconnected { was_connected: stored.is_some(), revoked, can_revoke })
    }

    /// The drive of the connected account.
    pub fn drive(&self) -> Result<Box<dyn Drive>, CloudError> {
        let name = secret_name(self.kind, &self.settings.profile);
        if load_stored(&name, &self.settings.client_id)?.is_none() {
            return Err(CloudError::NotConnected(label(self.kind)));
        }
        self.drive_with(self.tokens())
    }

    fn drive_with(&self, tokens: TokenSource) -> Result<Box<dyn Drive>, CloudError> {
        let endpoints = self.settings.endpoints_for(self.kind);
        let api = crate::api::Api::new(self.kind, self.http.clone(), tokens);
        Ok(match self.kind {
            ProviderKind::Google => Box::new(crate::google::GoogleDrive::new(api, endpoints)),
            ProviderKind::Microsoft => Box::new(crate::graph::OneDrive::new(api, endpoints)),
        })
    }
}
