//! What a connection needs to know: the owner's own OAuth client, the scopes, and where the
//! provider's endpoints are.
//!
//! The client ID (and, for a Google desktop client, its secret, which Google documents as not
//! confidential) belong to the person's own developer project; PdfCraft ships none. They are kept
//! in the system keychain ([`save_client`]) so the app and the command line see the same ones, and
//! `PDFCRAFT_GOOGLE_CLIENT_ID`, `PDFCRAFT_GOOGLE_CLIENT_SECRET`, `PDFCRAFT_GOOGLE_SCOPES`,
//! `PDFCRAFT_MICROSOFT_CLIENT_ID`, `PDFCRAFT_MICROSOFT_AUTHORITY` and `PDFCRAFT_MICROSOFT_SCOPES` override
//! them for one run.

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::CloudError;
use crate::types::ProviderKind;

/// Google's default scope: read and write the whole drive, so a file the person did not create
/// with PdfCraft can be opened and saved back. A narrower scope (`drive.file`) cannot see files
/// the app did not make, which is the point of this feature.
pub const GOOGLE_DEFAULT_SCOPE: &str = "https://www.googleapis.com/auth/drive";

/// Microsoft's default scopes: files, a refresh token, and the profile name for the account label.
pub const MICROSOFT_DEFAULT_SCOPES: &str = "Files.ReadWrite offline_access User.Read";

/// The scopes asked for when Settings names none.
pub fn default_scopes(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Google => GOOGLE_DEFAULT_SCOPE,
        ProviderKind::Microsoft => MICROSOFT_DEFAULT_SCOPES,
    }
}

/// The default Microsoft sign-in authority: work, school and personal accounts.
pub const MICROSOFT_DEFAULT_AUTHORITY: &str = "https://login.microsoftonline.com/common";

/// The longest client ID, secret, scope list or authority accepted.
const MAX_FIELD: usize = 2048;

/// The addresses of one provider's services. [`Endpoints::of`] gives the real ones; tests point
/// them at a server on this computer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// Where the browser goes to sign in.
    pub authorize: String,
    /// Where the code and the refresh token are exchanged.
    pub token: String,
    /// Where a token is revoked, when the provider has such an endpoint.
    pub revoke: Option<String>,
    /// The files API: `https://www.googleapis.com/drive/v3` or `https://graph.microsoft.com/v1.0`.
    pub api: String,
    /// Google's upload API root; Microsoft uploads through the files API.
    pub upload: String,
    /// The host the redirect address names. Google accepts the loopback address, Microsoft's
    /// registration for a desktop app is `http://localhost`.
    pub redirect_host: String,
}

impl Endpoints {
    /// The real services' addresses. `authority` replaces Microsoft's `/common` (a tenant, or
    /// `consumers`, `organizations`).
    pub fn of(kind: ProviderKind, authority: &str) -> Endpoints {
        match kind {
            ProviderKind::Google => Endpoints {
                authorize: "https://accounts.google.com/o/oauth2/v2/auth".into(),
                token: "https://oauth2.googleapis.com/token".into(),
                revoke: Some("https://oauth2.googleapis.com/revoke".into()),
                api: "https://www.googleapis.com/drive/v3".into(),
                upload: "https://www.googleapis.com/upload/drive/v3".into(),
                redirect_host: "127.0.0.1".into(),
            },
            ProviderKind::Microsoft => {
                let authority = authority.trim().trim_end_matches('/');
                let authority = if authority.is_empty() { MICROSOFT_DEFAULT_AUTHORITY } else { authority };
                Endpoints {
                    authorize: format!("{authority}/oauth2/v2.0/authorize"),
                    token: format!("{authority}/oauth2/v2.0/token"),
                    revoke: None,
                    api: "https://graph.microsoft.com/v1.0".into(),
                    upload: String::new(),
                    redirect_host: "localhost".into(),
                }
            }
        }
    }

    /// Every address on one server: `base` is `http://127.0.0.1:<port>` of the test server.
    pub fn local(base: &str) -> Endpoints {
        let base = base.trim_end_matches('/');
        Endpoints {
            authorize: format!("{base}/authorize"),
            token: format!("{base}/token"),
            revoke: Some(format!("{base}/revoke")),
            api: String::new(),
            upload: String::new(),
            redirect_host: "127.0.0.1".into(),
        }
    }

    /// The same server as the Google files API.
    pub fn local_google(base: &str) -> Endpoints {
        let base = base.trim_end_matches('/');
        Endpoints { api: format!("{base}/drive/v3"), upload: format!("{base}/upload/drive/v3"), ..Endpoints::local(base) }
    }

    /// The same server as the Graph files API (no token revocation, as the real service).
    pub fn local_microsoft(base: &str) -> Endpoints {
        let base = base.trim_end_matches('/');
        Endpoints { api: format!("{base}/v1.0"), revoke: None, ..Endpoints::local(base) }
    }

    /// Checks every address is one a token may be sent to.
    pub(crate) fn validate(&self) -> Result<(), CloudError> {
        let mut all = vec![&self.authorize, &self.token, &self.api];
        all.extend(self.revoke.iter());
        if !self.upload.is_empty() {
            all.push(&self.upload);
        }
        for url in all {
            crate::url::check_endpoint(url)?;
        }
        Ok(())
    }
}

/// One provider's settings.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProviderSettings {
    pub client_id: String,
    /// A Google desktop client's secret; empty for none (Microsoft needs none).
    pub client_secret: String,
    /// Space-separated scopes; empty for the defaults.
    pub scopes: String,
    /// Microsoft only: the sign-in authority; empty for [`MICROSOFT_DEFAULT_AUTHORITY`].
    pub authority: String,
    /// Names the stored account when more than one is wanted at once (tests); empty otherwise.
    pub profile: String,
    /// Replaces the real services' addresses (tests).
    pub endpoints: Option<Endpoints>,
}

impl std::fmt::Debug for ProviderSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSettings")
            .field("client_id", &self.client_id)
            .field("client_secret", &if self.client_secret.is_empty() { "<none>" } else { "<hidden>" })
            .field("scopes", &self.scopes)
            .field("authority", &self.authority)
            .finish_non_exhaustive()
    }
}

impl ProviderSettings {
    /// Whether a client ID is set.
    pub fn is_configured(&self) -> bool {
        !self.client_id.trim().is_empty()
    }

    /// The scopes to ask for.
    pub fn scope_string(&self, kind: ProviderKind) -> String {
        let own = self.scopes.split_whitespace().collect::<Vec<_>>().join(" ");
        if own.is_empty() { default_scopes(kind).to_string() } else { own }
    }

    /// The addresses to use.
    pub fn endpoints_for(&self, kind: ProviderKind) -> Endpoints {
        self.endpoints.clone().unwrap_or_else(|| Endpoints::of(kind, &self.authority))
    }

    /// Checks the fields are usable and not absurd; returns the settings trimmed.
    pub fn checked(&self, kind: ProviderKind) -> Result<ProviderSettings, CloudError> {
        if !self.is_configured() {
            return Err(CloudError::NotConfigured(kind.label()));
        }
        let mut out = self.clone();
        out.client_id = self.client_id.trim().to_string();
        out.client_secret = self.client_secret.trim().to_string();
        out.scopes = self.scope_string(kind);
        out.authority = self.authority.trim().to_string();
        let plain = |s: &str| s.chars().all(|c| !c.is_control());
        for field in [&out.client_id, &out.client_secret, &out.scopes, &out.authority] {
            if field.len() > MAX_FIELD || !plain(field) {
                return Err(CloudError::Invalid("a client setting is too long or has control characters".into()));
            }
        }
        if out.client_id.contains(char::is_whitespace) || out.client_secret.contains(char::is_whitespace) {
            return Err(CloudError::Invalid("a client ID or secret cannot contain spaces".into()));
        }
        out.endpoints_for(kind).validate()?;
        Ok(out)
    }
}

/// The saved client of one provider, as the keychain holds it.
#[derive(Serialize, Deserialize, Default)]
struct StoredClient {
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    client_secret: String,
    #[serde(default)]
    scopes: String,
    #[serde(default)]
    authority: String,
}

fn client_account(kind: ProviderKind) -> String {
    format!("cloud/{}-client", kind.id())
}

/// Saves the client ID (and secret, scopes, authority) the person typed for `kind`.
pub fn save_client(kind: ProviderKind, settings: &ProviderSettings) -> Result<(), CloudError> {
    let stored = StoredClient {
        client_id: settings.client_id.trim().to_string(),
        client_secret: settings.client_secret.trim().to_string(),
        scopes: settings.scopes.split_whitespace().collect::<Vec<_>>().join(" "),
        authority: settings.authority.trim().to_string(),
    };
    if stored.client_id.len() > MAX_FIELD
        || stored.client_secret.len() > MAX_FIELD
        || stored.scopes.len() > MAX_FIELD
        || stored.authority.len() > MAX_FIELD
    {
        return Err(CloudError::Invalid("a client setting is too long".into()));
    }
    if stored.client_id.is_empty() {
        return Ok(pdfcraft_assist::secret::delete_named(&client_account(kind))?);
    }
    let json = Zeroizing::new(serde_json::to_string(&stored).map_err(|e| CloudError::Invalid(e.to_string()))?);
    Ok(pdfcraft_assist::secret::save_named(&client_account(kind), &json)?)
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// The settings for `kind`: the saved client, with the environment's values over it.
pub fn load_client(kind: ProviderKind) -> Result<ProviderSettings, CloudError> {
    let stored: StoredClient = match pdfcraft_assist::secret::load_named(&client_account(kind))? {
        Some(json) => serde_json::from_str(&json).unwrap_or_default(),
        None => StoredClient::default(),
    };
    let mut settings = ProviderSettings {
        client_id: stored.client_id,
        client_secret: stored.client_secret,
        scopes: stored.scopes,
        authority: stored.authority,
        ..ProviderSettings::default()
    };
    match kind {
        ProviderKind::Google => {
            if let Some(id) = env("PDFCRAFT_GOOGLE_CLIENT_ID") {
                settings.client_id = id;
            }
            if let Some(secret) = env("PDFCRAFT_GOOGLE_CLIENT_SECRET") {
                settings.client_secret = secret;
            }
            if let Some(scopes) = env("PDFCRAFT_GOOGLE_SCOPES") {
                settings.scopes = scopes;
            }
        }
        ProviderKind::Microsoft => {
            if let Some(id) = env("PDFCRAFT_MICROSOFT_CLIENT_ID") {
                settings.client_id = id;
            }
            if let Some(authority) = env("PDFCRAFT_MICROSOFT_AUTHORITY") {
                settings.authority = authority;
            }
            if let Some(scopes) = env("PDFCRAFT_MICROSOFT_SCOPES") {
                settings.scopes = scopes;
            }
        }
    }
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_endpoints_are_https_and_the_defaults_apply() {
        for kind in ProviderKind::ALL {
            let settings = ProviderSettings { client_id: "id".into(), ..ProviderSettings::default() }.checked(kind).unwrap();
            let endpoints = settings.endpoints_for(kind);
            assert!(endpoints.authorize.starts_with("https://") && endpoints.token.starts_with("https://") && endpoints.api.starts_with("https://"));
        }
        let ms = Endpoints::of(ProviderKind::Microsoft, "https://login.microsoftonline.com/consumers/");
        assert_eq!(ms.token, "https://login.microsoftonline.com/consumers/oauth2/v2.0/token");
        assert_eq!(ms.redirect_host, "localhost");
        assert_eq!(Endpoints::of(ProviderKind::Google, "").redirect_host, "127.0.0.1");
    }

    #[test]
    fn a_missing_or_odd_client_is_refused() {
        assert_eq!(ProviderSettings::default().checked(ProviderKind::Google), Err(CloudError::NotConfigured("Google Drive")));
        let spaced = ProviderSettings { client_id: "a b".into(), ..ProviderSettings::default() };
        assert!(matches!(spaced.checked(ProviderKind::Google), Err(CloudError::Invalid(_))));
        let http = ProviderSettings { client_id: "id".into(), authority: "http://login.example.com/common".into(), ..ProviderSettings::default() };
        assert!(http.checked(ProviderKind::Microsoft).is_err(), "a cleartext authority off this computer is refused");
    }

    #[test]
    fn scopes_are_configurable_and_default_to_the_drive_scopes() {
        let own = ProviderSettings {
            client_id: "id".into(),
            scopes: "  https://www.googleapis.com/auth/drive.file   openid ".into(),
            ..ProviderSettings::default()
        };
        assert_eq!(own.scope_string(ProviderKind::Google), "https://www.googleapis.com/auth/drive.file openid");
        let default = ProviderSettings { client_id: "id".into(), ..ProviderSettings::default() };
        assert_eq!(default.scope_string(ProviderKind::Google), "https://www.googleapis.com/auth/drive");
        assert_eq!(default.scope_string(ProviderKind::Microsoft), "Files.ReadWrite offline_access User.Read");
    }

    #[test]
    fn the_secret_does_not_print() {
        let s = ProviderSettings { client_id: "id".into(), client_secret: "GOCSPX-very-secret".into(), ..ProviderSettings::default() };
        assert!(!format!("{s:?}").contains("GOCSPX"));
    }

    #[test]
    fn a_saved_client_is_read_back() {
        pdfcraft_assist::secret::keep_in_memory_only();
        let saved =
            ProviderSettings { client_id: "client-1".into(), client_secret: "sec".into(), scopes: "a  b".into(), ..ProviderSettings::default() };
        save_client(ProviderKind::Microsoft, &saved).unwrap();
        let loaded = load_client(ProviderKind::Microsoft).unwrap();
        assert_eq!((loaded.client_id.as_str(), loaded.scopes.as_str()), ("client-1", "a b"));
        save_client(ProviderKind::Microsoft, &ProviderSettings::default()).unwrap();
        assert!(!load_client(ProviderKind::Microsoft).unwrap().is_configured());
    }
}
