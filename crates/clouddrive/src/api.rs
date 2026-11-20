//! Authenticated calls: a request with the account's token, refreshed once if the provider says
//! it has expired.

use std::sync::Arc;

use serde_json::Value;

use crate::account::TokenSource;
use crate::error::{CloudError, label};
use crate::http::{Http, Reply, Spec, status_error};
use crate::types::ProviderKind;

pub(crate) struct Api {
    pub kind: ProviderKind,
    pub http: Arc<Http>,
    tokens: TokenSource,
}

impl Api {
    pub fn new(kind: ProviderKind, http: Arc<Http>, tokens: TokenSource) -> Api {
        Api { kind, http, tokens }
    }

    pub fn provider(&self) -> &'static str {
        label(self.kind)
    }

    /// Sends the request `make` builds from the access token. A 401 refreshes the token and sends
    /// it once more; any other 4xx or 5xx is the error it means. A redirect is returned as it is
    /// (the caller decides whether it may be followed, and never with the token).
    pub fn call(&self, what: &str, spec: &Spec<'_>) -> Result<Reply, CloudError> {
        let token = self.tokens.access()?;
        let mut reply = self.http.send(spec, Some(&token))?;
        if reply.status == 401 {
            let token = self.tokens.refresh()?;
            reply = self.http.send(spec, Some(&token))?;
        }
        if reply.status >= 400 {
            let status = reply.status;
            let body = reply.bytes_lossy(64 * 1024);
            return Err(status_error(self.provider(), status, &body, what));
        }
        Ok(reply)
    }

    /// [`Self::call`], with the answer read as JSON.
    pub fn json(&self, what: &str, spec: &Spec<'_>) -> Result<Value, CloudError> {
        let reply = self.call(what, spec)?;
        if reply.is_redirect() {
            return Err(CloudError::BadResponse("the service redirected, which PdfCraft does not follow with a token".into()));
        }
        reply.json()
    }

    /// Sends a request that carries no token: the address is a pre-authorized one the provider
    /// handed out (an upload session, a download link). Errors as [`Self::call`].
    pub fn plain(&self, what: &str, spec: &Spec<'_>) -> Result<Reply, CloudError> {
        let reply = self.http.send(spec, None)?;
        if reply.status >= 400 {
            let status = reply.status;
            let body = reply.bytes_lossy(64 * 1024);
            return Err(status_error(self.provider(), status, &body, what));
        }
        Ok(reply)
    }
}
