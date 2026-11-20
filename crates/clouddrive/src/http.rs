//! The HTTP client every cloud request goes through (native builds only).
//!
//! - No cookie jar (ureq's `cookies` feature is not built) and no automatic redirects: a caller
//!   follows one by hand where a provider documents it ([`crate::graph`] does, without the token),
//!   so `Authorization` never reaches another host.
//! - A status of 4xx or 5xx is a reply, not an error, so the provider's message can be read.
//! - Two agents: metadata calls give up after [`META_TIMEOUT`]; a transfer after [`TRANSFER_TIMEOUT`]
//!   in all and [`IDLE_TIMEOUT`] without a byte moving.
//! - Every body read is capped, and a download is read in chunks so it can be cancelled and report
//!   progress.
//! - A token is only ever put in a header, never in an address, a log line or an error.

use std::io::Read;
use std::time::Duration;

use serde_json::Value;

use crate::error::{CloudError, one_line};
use crate::types::{CancelToken, Progress};

/// A metadata call, from connecting to the last byte.
pub const META_TIMEOUT: Duration = Duration::from_secs(30);
/// A whole upload or download.
pub const TRANSFER_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// A transfer that moves no byte for this long is given up on.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(45);
/// The most of a JSON answer that is read.
pub const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
/// The read size of a download: progress and cancellation are checked once per chunk.
const CHUNK: usize = 64 * 1024;

/// `PdfCraft/<version>`, the `User-Agent` of every request.
fn user_agent() -> String {
    format!("PdfCraft/{}", env!("CARGO_PKG_VERSION"))
}

fn agent(global: Duration, idle: Option<Duration>) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .https_only(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .timeout_global(Some(global))
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_per_read(idle)
        .user_agent(user_agent())
        .build()
        .new_agent()
}

/// What a request carries.
pub(crate) enum Payload<'a> {
    None,
    Bytes {
        data: &'a [u8],
        content_type: &'a str,
    },
    /// A `application/x-www-form-urlencoded` body, already encoded.
    Form(&'a str),
}

/// One request.
pub(crate) struct Spec<'a> {
    pub method: &'static str,
    pub url: &'a str,
    pub headers: Vec<(&'static str, String)>,
    pub payload: Payload<'a>,
    pub transfer: bool,
}

impl<'a> Spec<'a> {
    pub fn new(method: &'static str, url: &'a str) -> Self {
        Spec { method, url, headers: Vec::new(), payload: Payload::None, transfer: false }
    }

    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    pub fn bytes(mut self, data: &'a [u8], content_type: &'a str) -> Self {
        self.payload = Payload::Bytes { data, content_type };
        self
    }

    pub fn json(self, body: &'a [u8]) -> Self {
        self.bytes(body, "application/json; charset=UTF-8")
    }

    pub fn form(mut self, body: &'a str) -> Self {
        self.payload = Payload::Form(body);
        self
    }

    pub fn transfer(mut self) -> Self {
        self.transfer = true;
        self
    }
}

/// The two agents.
pub(crate) struct Http {
    meta: ureq::Agent,
    transfer: ureq::Agent,
}

impl Default for Http {
    fn default() -> Self {
        Http { meta: agent(META_TIMEOUT, None), transfer: agent(TRANSFER_TIMEOUT, Some(IDLE_TIMEOUT)) }
    }
}

/// A reply whose body has not been read yet.
pub(crate) struct Reply {
    pub status: u16,
    response: ureq::http::Response<ureq::Body>,
}

impl Http {
    /// Sends `spec`. A 4xx or 5xx is a [`Reply`]; only a transport failure is an error.
    pub fn send(&self, spec: &Spec<'_>, bearer: Option<&str>) -> Result<Reply, CloudError> {
        let agent = if spec.transfer { &self.transfer } else { &self.meta };
        let mut builder = ureq::http::Request::builder().method(spec.method).uri(spec.url);
        if let Some(token) = bearer {
            builder = builder.header("Authorization", format!("Bearer {token}"));
        }
        for (name, value) in &spec.headers {
            builder = builder.header(*name, value.as_str());
        }
        let bad = |e: ureq::http::Error| CloudError::Invalid(one_line(&e.to_string(), 160));
        let response = match &spec.payload {
            Payload::None => agent.run(builder.body(()).map_err(bad)?),
            Payload::Bytes { data, content_type } => agent.run(builder.header("Content-Type", *content_type).body(*data).map_err(bad)?),
            Payload::Form(body) => agent.run(builder.header("Content-Type", "application/x-www-form-urlencoded").body(body.as_bytes()).map_err(bad)?),
        }
        .map_err(network)?;
        Ok(Reply { status: response.status().as_u16(), response })
    }
}

/// What a transport error says for people: ureq's text names the host, never a header.
fn network(e: ureq::Error) -> CloudError {
    match e {
        ureq::Error::Timeout(_) => CloudError::Network("the request timed out".into()),
        other => CloudError::Network(one_line(&other.to_string(), 200)),
    }
}

impl Reply {
    /// The first header named `name` (any letter case), as text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.response.headers().get(name).and_then(|v| v.to_str().ok())
    }

    /// The declared length of the body.
    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length").and_then(|v| v.trim().parse().ok())
    }

    pub fn is_redirect(&self) -> bool {
        (300..400).contains(&self.status) && self.status != 304
    }

    /// At most `max` bytes of the body; more is an error.
    pub fn bytes(mut self, max: usize, what: &str) -> Result<Vec<u8>, CloudError> {
        let mut reader = self.response.body_mut().with_config().reader().take((max as u64).saturating_add(1));
        let mut out = Vec::new();
        reader.read_to_end(&mut out).map_err(|e| CloudError::Network(one_line(&e.to_string(), 160)))?;
        if out.len() > max {
            return Err(CloudError::TooLarge { what: what.to_string(), limit: max as u64 });
        }
        Ok(out)
    }

    /// The body, ignoring how it ends: for an error answer, where a short read is no loss.
    pub fn bytes_lossy(mut self, max: usize) -> Vec<u8> {
        let mut reader = self.response.body_mut().with_config().reader().take(max as u64);
        let mut out = Vec::new();
        let _ = reader.read_to_end(&mut out);
        out
    }

    /// The body as JSON, capped at [`MAX_JSON_BYTES`].
    pub fn json(self) -> Result<Value, CloudError> {
        let bytes = self.bytes(MAX_JSON_BYTES, "the answer")?;
        serde_json::from_slice(&bytes).map_err(|_| CloudError::BadResponse("the answer is not JSON".into()))
    }

    /// The body in chunks: at most `max` bytes, checking `cancel` and reporting progress after each.
    pub fn stream(mut self, max: u64, what: &str, cancel: &CancelToken, progress: &mut dyn FnMut(Progress)) -> Result<Vec<u8>, CloudError> {
        let total = self.content_length();
        if let Some(total) = total
            && total > max
        {
            return Err(CloudError::TooLarge { what: what.to_string(), limit: max });
        }
        let mut reader = self.response.body_mut().with_config().limit(max.saturating_add(1)).reader();
        let mut out: Vec<u8> = Vec::with_capacity(usize::try_from(total.unwrap_or(0).min(max).min(64 * 1024 * 1024)).unwrap_or(0));
        let mut chunk = vec![0u8; CHUNK];
        progress(Progress { done: 0, total });
        loop {
            cancel.check()?;
            let n = reader.read(&mut chunk).map_err(|e| CloudError::Network(one_line(&e.to_string(), 160)))?;
            if n == 0 {
                break;
            }
            if out.len() as u64 + n as u64 > max {
                return Err(CloudError::TooLarge { what: what.to_string(), limit: max });
            }
            out.extend_from_slice(chunk.get(..n).unwrap_or_default());
            progress(Progress { done: out.len() as u64, total });
        }
        if let Some(total) = total
            && (out.len() as u64) < total
        {
            return Err(CloudError::Network("the download ended early".into()));
        }
        Ok(out)
    }
}

/// The provider's message in an error answer: Google's and Graph's `error.message`, OAuth's
/// `error_description` or `error`, else the start of the text. One short line.
pub(crate) fn error_message(body: &[u8]) -> String {
    let json: Option<Value> = serde_json::from_slice(body).ok();
    let found = json.as_ref().and_then(|j| {
        j.pointer("/error/message")
            .or_else(|| j.get("error_description"))
            .or_else(|| j.pointer("/error"))
            .or_else(|| j.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let text = found.unwrap_or_else(|| String::from_utf8_lossy(body.get(..body.len().min(300)).unwrap_or_default()).into_owned());
    one_line(&text, 300)
}

/// The machine-readable reason of an error answer (`error.errors[0].reason` for Google,
/// `error.code` for Graph, `error` for OAuth).
pub(crate) fn error_reason(body: &[u8]) -> Option<String> {
    let json: Value = serde_json::from_slice(body).ok()?;
    json.pointer("/error/errors/0/reason")
        .or_else(|| json.pointer("/error/code"))
        .or_else(|| json.get("error"))
        .and_then(Value::as_str)
        .map(|s| one_line(s, 60))
}

/// Turns a non-2xx reply into the error a caller acts on. `provider` names the service for the
/// sign-in messages and `what` the thing asked for.
pub(crate) fn status_error(provider: &'static str, status: u16, body: &[u8], what: &str) -> CloudError {
    let message = error_message(body);
    match status {
        401 => CloudError::Unauthenticated(provider),
        403 => {
            let reason = error_reason(body).unwrap_or_default();
            if reason.to_ascii_lowercase().contains("ratelimit") || reason.contains("quotaExceeded") || reason.contains("activityLimitReached") {
                CloudError::Http { status, message: format!("{provider} is limiting requests; try again in a minute ({message})") }
            } else {
                CloudError::Forbidden(if message.is_empty() { format!("{provider} does not allow that on {what}") } else { message })
            }
        }
        404 => CloudError::NotFound(if message.is_empty() { what.to_string() } else { format!("{what} ({message})") }),
        409 | 412 => CloudError::Http { status, message: if message.is_empty() { "the file changed on the drive".into() } else { message } },
        429 => CloudError::Http { status, message: format!("{provider} is limiting requests; try again in a minute") },
        _ => CloudError::Http { status, message },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdfcraft_assist::mock::{MockServer, Reply as MockReply};

    #[test]
    fn a_token_is_a_header_and_a_redirect_is_not_followed() {
        let target = MockServer::start(|_| MockReply::json("{}")).unwrap();
        let location = format!("{}/steal", target.url());
        let server = MockServer::start(move |_| MockReply::redirect(302, &location)).unwrap();
        let reply = Http::default().send(&Spec::new("GET", &format!("{}/x", server.url())), Some("tok-secret-1")).unwrap();
        assert!(reply.is_redirect());
        assert_eq!(server.requests()[0].header("authorization"), Some("Bearer tok-secret-1"));
        assert!(target.requests().is_empty(), "the redirect target saw nothing");
        assert!(server.requests()[0].header("cookie").is_none());
        assert!(server.requests()[0].header("user-agent").is_some_and(|u| u.starts_with("PdfCraft/")));
    }

    #[test]
    fn a_body_over_the_cap_is_an_error_not_a_truncation() {
        let server = MockServer::start(|_| MockReply::new(200, "application/octet-stream", vec![7u8; 5000])).unwrap();
        let get = || Http::default().send(&Spec::new("GET", &format!("{}/x", server.url())), None).unwrap();
        assert!(matches!(get().bytes(4000, "the file"), Err(CloudError::TooLarge { limit: 4000, .. })));
        assert_eq!(get().bytes(5000, "the file").unwrap().len(), 5000);
        let mut seen = Vec::new();
        let out = get().stream(5000, "the file", &CancelToken::new(), &mut |p| seen.push(p)).unwrap();
        assert_eq!(out.len(), 5000);
        assert_eq!(seen.last().map(|p| (p.done, p.total)), Some((5000, Some(5000))));
        assert!(matches!(get().stream(100, "the file", &CancelToken::new(), &mut |_| {}), Err(CloudError::TooLarge { .. })));
    }

    #[test]
    fn a_cancelled_download_stops() {
        let server = MockServer::start(|_| MockReply::new(200, "application/octet-stream", vec![1u8; 300_000])).unwrap();
        let cancel = CancelToken::new();
        let reply = Http::default().send(&Spec::new("GET", &format!("{}/x", server.url())), None).unwrap();
        let c = cancel.clone();
        let result = reply.stream(1 << 30, "the file", &cancel, &mut |p| {
            if p.done > 100_000 {
                c.cancel();
            }
        });
        assert_eq!(result, Err(CloudError::Cancelled));
    }

    #[test]
    fn provider_messages_are_read_and_the_status_decides_the_error() {
        let google =
            br#"{"error":{"code":403,"message":"The user's Drive storage quota has been exceeded.","errors":[{"reason":"storageQuotaExceeded"}]}}"#;
        assert!(matches!(status_error("Google Drive", 403, google, "the file"), CloudError::Forbidden(m) if m.contains("quota")));
        let limit = br#"{"error":{"message":"Rate Limit Exceeded","errors":[{"reason":"userRateLimitExceeded"}]}}"#;
        assert!(matches!(status_error("Google Drive", 403, limit, "x"), CloudError::Http { status: 403, .. }));
        let graph = br#"{"error":{"code":"itemNotFound","message":"The resource could not be found."}}"#;
        assert!(matches!(status_error("OneDrive", 404, graph, "the file"), CloudError::NotFound(m) if m.contains("resource")));
        assert_eq!(status_error("OneDrive", 401, b"", "x"), CloudError::Unauthenticated("OneDrive"));
        assert_eq!(
            error_message(br#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#),
            "Token has been expired or revoked."
        );
    }
}
