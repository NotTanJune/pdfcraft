//! OAuth 2.0 authorization code flow with PKCE (RFC 7636, S256) and a loopback redirect (RFC 8252).
//!
//! 1. [`Loopback::bind`] listens on `127.0.0.1` at a port the system picks.
//! 2. [`authorize_url`] is opened in the person's browser. The person signs in and agrees there;
//!    PdfCraft never sees a password.
//! 3. [`Loopback::wait`] accepts the redirect back: one `GET /?code=...&state=...` of at most
//!    8 KiB, from this computer, with a Host header naming this computer. A request whose `state`
//!    differs is answered and ignored (something else on this computer, or a stale tab), and the
//!    wait continues until its time runs out.
//! 4. [`exchange_code`] trades the code and the PKCE verifier for tokens at the provider's token
//!    endpoint, over https.
//!
//! The authorization code and every token are wiped from memory when dropped and never appear in
//! an error message or a log.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use serde_json::Value;
use zeroize::Zeroizing;

use crate::error::{CloudError, label, one_line};
use crate::http::{Http, Spec, error_message, error_reason, status_error};
use crate::pkce::same_secret;
use crate::settings::{Endpoints, ProviderSettings};
use crate::types::{CancelToken, ProviderKind};
use crate::url;

/// The longest request line and headers the redirect may have.
const MAX_REQUEST_BYTES: usize = 8 * 1024;
/// The most connections accepted while waiting for the redirect.
const MAX_CONNECTIONS: usize = 32;
/// How long one connection may take to send its request.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(3);
/// The longest token accepted from a provider.
const MAX_TOKEN_BYTES: usize = 8 * 1024;

/// A token response.
pub(crate) struct TokenResponse {
    pub access_token: Zeroizing<String>,
    pub refresh_token: Option<Zeroizing<String>>,
    pub expires_in: Duration,
    /// Microsoft's and Google's identity token, when the scopes asked for one. Read for the
    /// account label only; never used to authorize anything.
    pub id_token: Option<Zeroizing<String>>,
}

/// A token that fits in a header and is not empty.
fn token_ok(t: &str) -> bool {
    !t.is_empty() && t.len() <= MAX_TOKEN_BYTES && t.chars().all(|c| c.is_ascii_graphic())
}

/// The listener that takes the redirect.
pub struct Loopback {
    listener: TcpListener,
    port: u16,
}

impl Loopback {
    /// Listens on `127.0.0.1`, at a port the system chooses.
    pub fn bind() -> Result<Loopback, CloudError> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .map_err(|e| CloudError::SignIn(format!("could not listen on this computer: {e}")))?;
        listener.set_nonblocking(true).map_err(|e| CloudError::SignIn(e.to_string()))?;
        let port = listener.local_addr().map_err(|e| CloudError::SignIn(e.to_string()))?.port();
        Ok(Loopback { listener, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The address the provider sends the browser back to: `http://<host>:<port>`.
    pub fn redirect_uri(&self, host: &str) -> String {
        format!("http://{host}:{}", self.port)
    }

    /// Waits for the redirect and returns its authorization code. `Err` when the person refused,
    /// the time ran out, or `cancel` was set.
    pub fn wait(&self, state: &str, cancel: &CancelToken, timeout: Duration) -> Result<Zeroizing<String>, CloudError> {
        let deadline = Instant::now() + timeout;
        let mut connections = 0usize;
        loop {
            cancel.check()?;
            if Instant::now() >= deadline {
                return Err(CloudError::SignIn("timed out waiting for you to sign in in the browser".into()));
            }
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    connections += 1;
                    if !peer.ip().is_loopback() {
                        continue;
                    }
                    match self.handle(stream, state) {
                        Handled::Code(code) => return Ok(code),
                        Handled::Denied(why) => return Err(CloudError::SignIn(why)),
                        Handled::Ignored => {}
                    }
                    if connections >= MAX_CONNECTIONS {
                        return Err(CloudError::SignIn("too many unrelated requests arrived while waiting for the sign-in".into()));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(CloudError::SignIn(format!("the listener failed: {e}"))),
            }
        }
    }

    fn handle(&self, mut stream: TcpStream, state: &str) -> Handled {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(CONNECTION_TIMEOUT));
        let _ = stream.set_write_timeout(Some(CONNECTION_TIMEOUT));
        let Some(head) = read_head(&mut stream) else {
            respond(&mut stream, 400, "Bad request", "This request could not be read.");
            return Handled::Ignored;
        };
        let mut lines = head.split("\r\n");
        let mut request_line = lines.next().unwrap_or_default().split(' ');
        let (method, target) = (request_line.next().unwrap_or_default(), request_line.next().unwrap_or_default());
        let host =
            lines.find_map(|l| l.split_once(':').filter(|(n, _)| n.trim().eq_ignore_ascii_case("host")).map(|(_, v)| v.trim().to_ascii_lowercase()));
        let port = self.port;
        let host_ok = host
            .as_deref()
            .is_some_and(|h| [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")].iter().any(|ok| ok == h));
        if method != "GET" || !host_ok {
            respond(&mut stream, 400, "Bad request", "PdfCraft only accepts the sign-in redirect.");
            return Handled::Ignored;
        }
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let params = url::parse_query(query);
        let get = |name: &str| params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        // Only a redirect that carries our own state is the sign-in coming back.
        if path != "/" || !get("state").is_some_and(|s| same_secret(s, state)) {
            respond(&mut stream, 404, "Not found", "Nothing here. If you are signing in, return to the tab PdfCraft opened.");
            return Handled::Ignored;
        }
        if let Some(error) = get("error") {
            let detail = get("error_description").map(|d| format!(": {}", one_line(d, 200))).unwrap_or_default();
            respond(&mut stream, 200, "Sign-in cancelled", "PdfCraft was not connected. You can close this tab.");
            return Handled::Denied(format!("the provider reported {}{detail}", one_line(error, 60)));
        }
        match get("code").filter(|c| token_ok(c)) {
            Some(code) => {
                respond(&mut stream, 200, "Connected", "PdfCraft is connected. You can close this tab and go back to PdfCraft.");
                Handled::Code(Zeroizing::new(code.to_string()))
            }
            None => {
                respond(&mut stream, 400, "Bad request", "The sign-in answer had no code.");
                Handled::Ignored
            }
        }
    }
}

enum Handled {
    Code(Zeroizing<String>),
    Denied(String),
    Ignored,
}

/// The request line and headers, up to a blank line.
fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            buffer.truncate(end);
            return String::from_utf8(buffer).ok();
        }
        if buffer.len() > MAX_REQUEST_BYTES {
            return None;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(chunk.get(..n)?);
    }
}

/// A small page. It loads nothing, and the code and state are never echoed.
fn respond(stream: &mut TcpStream, status: u16, title: &str, message: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let body = format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>PdfCraft: {title}</title><body style=\"font-family:system-ui,sans-serif;margin:3rem\"><h1>{title}</h1><p>{message}</p></body></html>"
    );
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// The address to open in the browser.
pub fn authorize_url(
    kind: ProviderKind,
    settings: &ProviderSettings,
    endpoints: &Endpoints,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> String {
    let scope = settings.scope_string(kind);
    let mut pairs = vec![
        ("client_id", settings.client_id.as_str()),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("scope", scope.as_str()),
        ("state", state),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
    ];
    match kind {
        // Without `prompt=consent` Google sends a refresh token only the first time a person
        // agrees, and a person who connects again would get none.
        ProviderKind::Google => {
            pairs.push(("access_type", "offline"));
            pairs.push(("prompt", "consent"));
        }
        ProviderKind::Microsoft => pairs.push(("response_mode", "query")),
    }
    let sep = if endpoints.authorize.contains('?') { '&' } else { '?' };
    format!("{}{sep}{}", endpoints.authorize, url::form(&pairs))
}

fn parse_tokens(provider: &'static str, reply: crate::http::Reply) -> Result<TokenResponse, CloudError> {
    let status = reply.status;
    if !(200..300).contains(&status) {
        let body = reply.bytes_lossy(64 * 1024);
        return Err(token_error(provider, status, &body));
    }
    let json: Value = reply.json()?;
    let text = |name: &str| json.get(name).and_then(Value::as_str).map(str::to_string);
    let access =
        text("access_token").filter(|t| token_ok(t)).ok_or_else(|| CloudError::BadResponse("the token answer has no access token".into()))?;
    if text("token_type").is_some_and(|t| !t.eq_ignore_ascii_case("bearer")) {
        return Err(CloudError::BadResponse("the token answer is not a bearer token".into()));
    }
    let expires = json.get("expires_in").and_then(Value::as_u64).unwrap_or(3600).clamp(30, 24 * 3600);
    Ok(TokenResponse {
        access_token: Zeroizing::new(access),
        refresh_token: text("refresh_token").filter(|t| token_ok(t)).map(Zeroizing::new),
        expires_in: Duration::from_secs(expires),
        id_token: text("id_token").filter(|t| t.len() <= 16 * 1024).map(Zeroizing::new),
    })
}

/// What the token endpoint refusing means, for people.
fn token_error(provider: &'static str, status: u16, body: &[u8]) -> CloudError {
    match error_reason(body).as_deref() {
        Some("invalid_grant") => CloudError::Unauthenticated(provider),
        Some("invalid_client") | Some("unauthorized_client") => CloudError::SignIn(format!(
            "{provider} refused the client ID or secret ({}). Check them in Cloud drives, and that the app is registered as a desktop app",
            error_message(body)
        )),
        Some("invalid_request") | Some("invalid_scope") | Some("redirect_uri_mismatch") => {
            CloudError::SignIn(format!("{provider} refused the request: {}", error_message(body)))
        }
        _ => status_error(provider, status, body, "the sign-in"),
    }
}

fn post_form(http: &Http, endpoints: &Endpoints, kind: ProviderKind, body: &str) -> Result<TokenResponse, CloudError> {
    let reply = http.send(&Spec::new("POST", &endpoints.token).header("Accept", "application/json").form(body), None)?;
    if reply.is_redirect() {
        return Err(CloudError::BadResponse("the token endpoint redirected, which PdfCraft does not follow".into()));
    }
    parse_tokens(label(kind), reply)
}

/// Trades an authorization code for tokens.
pub(crate) fn exchange_code(
    http: &Http,
    kind: ProviderKind,
    settings: &ProviderSettings,
    endpoints: &Endpoints,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenResponse, CloudError> {
    let mut pairs = vec![
        ("client_id", settings.client_id.as_str()),
        ("code", code),
        ("code_verifier", verifier),
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect_uri),
    ];
    if !settings.client_secret.is_empty() {
        pairs.push(("client_secret", settings.client_secret.as_str()));
    }
    let body = Zeroizing::new(url::form(&pairs));
    post_form(http, endpoints, kind, &body)
}

/// Trades a refresh token for a new access token (and, for Microsoft, a new refresh token).
pub(crate) fn refresh(
    http: &Http,
    kind: ProviderKind,
    settings: &ProviderSettings,
    endpoints: &Endpoints,
    refresh_token: &str,
) -> Result<TokenResponse, CloudError> {
    let mut pairs = vec![("client_id", settings.client_id.as_str()), ("grant_type", "refresh_token"), ("refresh_token", refresh_token)];
    if !settings.client_secret.is_empty() {
        pairs.push(("client_secret", settings.client_secret.as_str()));
    }
    let scope = settings.scope_string(kind);
    if kind == ProviderKind::Microsoft {
        pairs.push(("scope", scope.as_str()));
    }
    let body = Zeroizing::new(url::form(&pairs));
    post_form(http, endpoints, kind, &body)
}

/// Asks the provider to forget `token`. Returns whether the provider accepted it.
pub(crate) fn revoke(http: &Http, endpoints: &Endpoints, token: &str) -> Result<bool, CloudError> {
    let Some(address) = &endpoints.revoke else { return Ok(false) };
    let body = Zeroizing::new(url::form(&[("token", token)]));
    let reply = http.send(&Spec::new("POST", address).form(&body), None)?;
    Ok((200..300).contains(&reply.status))
}

/// The email and name an identity token claims, for the account label. The token came straight
/// from the token endpoint over https, so it is read, not verified, and used for nothing but
/// display.
pub(crate) fn id_token_label(id_token: &str) -> (Option<String>, Option<String>) {
    use base64::Engine as _;
    let payload = id_token.split('.').nth(1).and_then(|p| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok());
    let Some(json) = payload.and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return (None, None) };
    let text = |n: &str| json.get(n).and_then(Value::as_str).map(|s| one_line(s, 120)).filter(|s| !s.is_empty());
    (text("email").or_else(|| text("preferred_username")), text("name"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn get(port: u16, target: &str, host: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    #[test]
    fn the_redirect_with_the_right_state_gives_its_code() {
        let loopback = Loopback::bind().unwrap();
        let port = loopback.port();
        assert_eq!(loopback.redirect_uri("localhost"), format!("http://localhost:{port}"));
        let browser = std::thread::spawn(move || get(port, "/?state=S1&code=4%2Fcode-1&scope=x", &format!("127.0.0.1:{port}")));
        let code = loopback.wait("S1", &CancelToken::new(), Duration::from_secs(10)).unwrap();
        assert_eq!(&*code, "4/code-1");
        let page = browser.join().unwrap();
        assert!(page.starts_with("HTTP/1.1 200") && page.contains("is connected"));
        assert!(!page.contains("code-1"), "the page never echoes the code");
    }

    #[test]
    fn a_wrong_state_a_stray_request_and_a_foreign_host_are_ignored_until_the_right_one_comes() {
        let loopback = Loopback::bind().unwrap();
        let port = loopback.port();
        let own = format!("127.0.0.1:{port}");
        let browser = std::thread::spawn(move || {
            let mut pages = vec![
                get(port, "/?state=WRONG&code=evil", &own),
                get(port, "/favicon.ico", &own),
                get(port, "/?state=S2&code=evil2", "attacker.example"),
                get(port, "/elsewhere?state=S2&code=evil3", &own),
            ];
            pages.push(get(port, "/?state=S2&code=good", &own));
            pages
        });
        let code = loopback.wait("S2", &CancelToken::new(), Duration::from_secs(10)).unwrap();
        assert_eq!(&*code, "good");
        let pages = browser.join().unwrap();
        assert!(pages[0].starts_with("HTTP/1.1 404") && pages[1].starts_with("HTTP/1.1 404"));
        assert!(pages[2].starts_with("HTTP/1.1 400") && pages[3].starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn a_refusal_in_the_browser_is_an_error_and_a_silent_browser_times_out() {
        let loopback = Loopback::bind().unwrap();
        let port = loopback.port();
        let own = format!("127.0.0.1:{port}");
        let browser = std::thread::spawn(move || get(port, "/?state=S3&error=access_denied&error_description=User+said+no", &own));
        let error = loopback.wait("S3", &CancelToken::new(), Duration::from_secs(10)).unwrap_err();
        browser.join().unwrap();
        assert!(matches!(&error, CloudError::SignIn(m) if m.contains("access_denied") && m.contains("User said no")), "{error:?}");
        let started = Instant::now();
        assert!(
            matches!(Loopback::bind().unwrap().wait("S", &CancelToken::new(), Duration::from_millis(150)), Err(CloudError::SignIn(m)) if m.contains("timed out"))
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn cancelling_stops_the_wait_and_a_huge_request_is_refused() {
        let loopback = Loopback::bind().unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        assert_eq!(loopback.wait("S", &cancel, Duration::from_secs(10)).unwrap_err(), CloudError::Cancelled);
        let port = loopback.port();
        let big = format!("/?state=S4&code=x&pad={}", "a".repeat(20_000));
        let browser = std::thread::spawn(move || get(port, &big, &format!("127.0.0.1:{port}")));
        assert!(matches!(loopback.wait("S4", &CancelToken::new(), Duration::from_millis(400)), Err(CloudError::SignIn(_))));
        assert!(browser.join().unwrap().starts_with("HTTP/1.1 400"));
    }

    #[test]
    fn the_sign_in_address_carries_pkce_and_state() {
        let settings = ProviderSettings { client_id: "cid".into(), ..ProviderSettings::default() }.checked(ProviderKind::Google).unwrap();
        let endpoints = settings.endpoints_for(ProviderKind::Google);
        let address = authorize_url(ProviderKind::Google, &settings, &endpoints, "http://127.0.0.1:5000", "st", "chal");
        let query = url::parse_query(address.split_once('?').unwrap().1);
        let get = |k: &str| query.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("code_challenge"), Some("chal"));
        assert_eq!(get("code_challenge_method"), Some("S256"));
        assert_eq!(get("state"), Some("st"));
        assert_eq!(get("redirect_uri"), Some("http://127.0.0.1:5000"));
        assert_eq!(get("scope"), Some("https://www.googleapis.com/auth/drive"));
        assert_eq!((get("access_type"), get("prompt")), (Some("offline"), Some("consent")));
        assert!(address.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(!address.contains("client_secret"));
    }

    #[test]
    fn an_identity_token_gives_a_label_and_a_bad_one_gives_none() {
        use base64::Engine as _;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"preferred_username":"me@example.com","name":"Me"}"#);
        assert_eq!(id_token_label(&format!("h.{payload}.s")), (Some("me@example.com".into()), Some("Me".into())));
        assert_eq!(id_token_label("nonsense"), (None, None));
    }
}
