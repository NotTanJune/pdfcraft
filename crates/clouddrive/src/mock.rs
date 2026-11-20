//! A server on `127.0.0.1` that speaks the endpoints this crate uses, for tests (feature `mock`,
//! native only): the OAuth authorize, token and revoke endpoints, the Google Drive v3 calls and the
//! Microsoft Graph `/me/drive` calls, over one in-memory set of files.
//!
//! It is not a model of either service: it answers the requests this crate makes, and checks what
//! the real ones check that matters here (the PKCE verifier against the challenge, the redirect
//! address, the client ID, a valid bearer token on every API call, a ranged upload in order). Tests
//! never reach the Internet.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use pdfcraft_assist::mock::{MockServer, Recorded, Reply};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::settings::{Endpoints, ProviderSettings};
use crate::url;

const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

/// A file or folder in the fake drive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeFile {
    pub id: String,
    pub name: String,
    pub parent: Option<String>,
    pub mime: String,
    pub bytes: Vec<u8>,
    /// Counts content changes; Google's `version` and part of Graph's `eTag`.
    pub version: u64,
    pub is_folder: bool,
}

enum Target {
    New { name: String, parent: Option<String>, mime: String },
    Replace(String),
}

struct Session {
    target: Target,
    graph: bool,
    total: usize,
    received: Vec<u8>,
}

#[derive(Default)]
struct State {
    files: BTreeMap<String, FakeFile>,
    next: u64,
    codes: HashMap<String, Grant>,
    refresh: HashSet<String>,
    access: HashSet<String>,
    revoked: Vec<String>,
    sessions: HashMap<String, Session>,
    email: String,
    name: String,
    rotate_refresh: bool,
    client_secret: Option<String>,
    token_requests: usize,
    withhold_refresh_token: bool,
    page_size_cap: Option<usize>,
    foreign_next_link: Option<String>,
}

struct Grant {
    challenge: String,
    redirect_uri: String,
    client_id: String,
}

/// The server and its files.
pub struct FakeCloud {
    server: MockServer,
    state: Arc<Mutex<State>>,
    delay_ms: Arc<std::sync::atomic::AtomicU64>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FakeCloud {
    pub fn start() -> std::io::Result<FakeCloud> {
        let state = Arc::new(Mutex::new(State { email: "tester@example.com".into(), name: "Test User".into(), ..State::default() }));
        let handler_state = state.clone();
        let delay_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let handler_delay = delay_ms.clone();
        let server = MockServer::start(move |r| {
            // A slow transfer, so a test can cancel it: file bytes and exports wait, nothing else does.
            let wait = handler_delay.load(std::sync::atomic::Ordering::Relaxed);
            if wait > 0 && (r.path.contains("alt=media") || r.path.contains("/blob/") || r.path.contains("/export")) {
                std::thread::sleep(std::time::Duration::from_millis(wait));
            }
            route(&handler_state, r)
        })?;
        Ok(FakeCloud { server, state, delay_ms })
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base(&self) -> String {
        self.server.url()
    }

    /// Settings for Google Drive against this server. `profile` keeps the test's saved account apart
    /// from other tests'.
    pub fn google_settings(&self, profile: &str) -> ProviderSettings {
        ProviderSettings {
            client_id: format!("google-client-{profile}"),
            client_secret: "google-secret-1".into(),
            profile: profile.into(),
            endpoints: Some(Endpoints::local_google(&self.base())),
            ..ProviderSettings::default()
        }
    }

    /// Settings for OneDrive against this server (a public client: no secret).
    pub fn microsoft_settings(&self, profile: &str) -> ProviderSettings {
        ProviderSettings {
            client_id: format!("ms-client-{profile}"),
            profile: profile.into(),
            endpoints: Some(Endpoints::local_microsoft(&self.base())),
            ..ProviderSettings::default()
        }
    }

    /// Makes the token endpoint demand this client secret (Google desktop clients send one).
    pub fn require_client_secret(&self, secret: &str) {
        lock(&self.state).client_secret = Some(secret.to_string());
    }

    /// Makes refreshing replace the refresh token, as Microsoft does.
    pub fn rotate_refresh_tokens(&self, on: bool) {
        lock(&self.state).rotate_refresh = on;
    }

    /// Makes the code exchange return no refresh token, as Google does for a second consent
    /// without `prompt=consent`.
    pub fn withhold_refresh_token(&self, on: bool) {
        lock(&self.state).withhold_refresh_token = on;
    }

    /// Caps every listing page at `n` items, so paging happens with a few files.
    pub fn cap_page_size(&self, n: usize) {
        lock(&self.state).page_size_cap = Some(n.max(1));
    }

    /// Makes every download of file bytes take `ms` more milliseconds.
    pub fn slow_down_downloads(&self, ms: u64) {
        self.delay_ms.store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// Makes every next-page link a listing returns point at `address` instead of this server.
    pub fn poison_next_links(&self, address: &str) {
        lock(&self.state).foreign_next_link = Some(address.to_string());
    }

    pub fn set_user(&self, email: &str, name: &str) {
        let mut s = lock(&self.state);
        s.email = email.into();
        s.name = name.into();
    }

    pub fn add_folder(&self, parent: Option<&str>, name: &str) -> String {
        self.add(parent, name, FOLDER_MIME, Vec::new(), true)
    }

    pub fn add_file(&self, parent: Option<&str>, name: &str, mime: &str, bytes: &[u8]) -> String {
        self.add(parent, name, mime, bytes.to_vec(), false)
    }

    fn add(&self, parent: Option<&str>, name: &str, mime: &str, bytes: Vec<u8>, is_folder: bool) -> String {
        let mut s = lock(&self.state);
        create(&mut s, parent.map(str::to_string), name, mime, bytes, is_folder)
    }

    pub fn file(&self, id: &str) -> Option<FakeFile> {
        lock(&self.state).files.get(id).cloned()
    }

    /// The first file called `name`.
    pub fn find(&self, name: &str) -> Option<FakeFile> {
        lock(&self.state).files.values().find(|f| f.name == name).cloned()
    }

    /// Every file called `name`.
    pub fn find_all(&self, name: &str) -> Vec<FakeFile> {
        lock(&self.state).files.values().filter(|f| f.name == name).cloned().collect()
    }

    /// Another writer saves new content: the version changes.
    pub fn edit(&self, id: &str, bytes: &[u8]) {
        if let Some(f) = lock(&self.state).files.get_mut(id) {
            f.bytes = bytes.to_vec();
            f.version += 1;
        }
    }

    /// Every access token issued so far stops working; the refresh tokens still do.
    pub fn expire_access_tokens(&self) {
        lock(&self.state).access.clear();
    }

    /// Every refresh token stops working, as if the person revoked PdfCraft's access.
    pub fn revoke_all_refresh_tokens(&self) {
        lock(&self.state).refresh.clear();
    }

    /// The tokens a client asked the revoke endpoint to forget.
    pub fn revoked(&self) -> Vec<String> {
        lock(&self.state).revoked.clone()
    }

    /// How many requests the token endpoint has had.
    pub fn token_requests(&self) -> usize {
        lock(&self.state).token_requests
    }

    /// Everything received, oldest first.
    pub fn requests(&self) -> Vec<Recorded> {
        self.server.requests()
    }

    /// The requests whose path starts with `prefix`.
    pub fn requests_to(&self, prefix: &str) -> Vec<Recorded> {
        self.requests().into_iter().filter(|r| r.path.starts_with(prefix)).collect()
    }

    /// A stand-in for the person's browser: given the sign-in address, it visits it on another
    /// thread (agreeing at once) and follows the redirect back to PdfCraft's listener.
    pub fn browser(&self) -> impl Fn(&str) -> Result<(), String> + Send + Sync + 'static {
        |address: &str| {
            let address = address.to_string();
            std::thread::spawn(move || {
                let agent = plain_agent();
                let Ok(first) = agent.get(&address).call() else { return };
                if let Some(back) = first.headers().get("location").and_then(|v| v.to_str().ok()) {
                    let _ = agent.get(back).call();
                }
            });
            Ok(())
        }
    }

    /// A browser that refuses: it visits the redirect with `error=access_denied` and the state.
    pub fn refusing_browser(&self) -> impl Fn(&str) -> Result<(), String> + Send + Sync + 'static {
        |address: &str| {
            let query = url::parse_query(address.split_once('?').map_or("", |(_, q)| q));
            let get = |k: &str| query.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
            let back =
                format!("{}/?error=access_denied&error_description=The+user+said+no&state={}", get("redirect_uri"), url::encode(&get("state")));
            std::thread::spawn(move || {
                let _ = plain_agent().get(&back).call();
            });
            Ok(())
        }
    }
}

fn plain_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .new_agent()
}

fn create(s: &mut State, parent: Option<String>, name: &str, mime: &str, bytes: Vec<u8>, is_folder: bool) -> String {
    s.next += 1;
    let id = format!("id{:04}", s.next);
    s.files.insert(id.clone(), FakeFile { id: id.clone(), name: name.into(), parent, mime: mime.into(), bytes, version: 1, is_folder });
    id
}

/// A one-page PDF that says `text`.
pub fn sample_pdf(text: &str) -> Vec<u8> {
    let text: String = text.chars().filter(|c| c.is_ascii_alphanumeric() || *c == ' ').collect();
    let content = format!("BT /F1 18 Tf 40 340 Td ({text}) Tj ET");
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 400] /Resources << /Font << /F1 4 0 R >> >> >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /Contents 5 0 R >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
    ];
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
    out
}

fn json_reply(status: u16, v: &Value) -> Reply {
    Reply::new(status, "application/json", v.to_string())
}

fn error(status: u16, message: &str) -> Reply {
    json_reply(status, &json!({ "error": { "code": status, "message": message, "errors": [{ "reason": "mock" }] } }))
}

fn host_base(r: &Recorded) -> String {
    format!("http://{}", r.header("host").unwrap_or("127.0.0.1"))
}

fn bearer(r: &Recorded) -> Option<&str> {
    r.header("authorization").and_then(|v| v.strip_prefix("Bearer "))
}

fn authorized(s: &State, r: &Recorded) -> bool {
    bearer(r).is_some_and(|t| s.access.contains(t))
}

fn route(state: &Mutex<State>, r: &Recorded) -> Reply {
    let (path, query) = r.path.split_once('?').unwrap_or((r.path.as_str(), ""));
    let q = url::parse_query(query);
    let mut s = lock(state);
    match path {
        "/authorize" => return authorize(&mut s, &q),
        "/token" if r.method == "POST" => return token(&mut s, r),
        "/revoke" if r.method == "POST" => {
            let form = url::parse_query(&String::from_utf8_lossy(&r.body));
            if let Some((_, t)) = form.iter().find(|(k, _)| k == "token") {
                s.refresh.remove(t);
                s.revoked.push(t.clone());
            }
            return json_reply(200, &json!({}));
        }
        _ => {}
    }
    if path.starts_with("/blob/") {
        // A pre-authorized download link: needs no token, and gets none.
        let id = path.trim_start_matches("/blob/").split('-').next().unwrap_or_default();
        return match s.files.get(id) {
            Some(f) => Reply::new(200, &f.mime, f.bytes.clone()),
            None => error(404, "gone"),
        };
    }
    if path.starts_with("/upload/session/") {
        return session_put(&mut s, path.trim_start_matches("/upload/session/"), r);
    }
    if !authorized(&s, r) {
        return error(401, "Invalid Credentials");
    }
    if let Some(rest) = path.strip_prefix("/drive/v3") {
        return google(&mut s, r, rest, &q);
    }
    if let Some(rest) = path.strip_prefix("/upload/drive/v3") {
        return google_upload(&mut s, r, rest, &q);
    }
    if let Some(rest) = path.strip_prefix("/v1.0") {
        return graph(&mut s, r, rest, &q);
    }
    error(404, "no such endpoint")
}

fn get<'a>(q: &'a [(String, String)], name: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn authorize(s: &mut State, q: &[(String, String)]) -> Reply {
    let (Some(client_id), Some(redirect), Some(state), Some(challenge)) =
        (get(q, "client_id"), get(q, "redirect_uri"), get(q, "state"), get(q, "code_challenge"))
    else {
        return Reply::new(400, "text/plain", "missing parameter");
    };
    if get(q, "code_challenge_method") != Some("S256") || get(q, "response_type") != Some("code") {
        return Reply::new(400, "text/plain", "PKCE S256 and response_type=code are required");
    }
    s.next += 1;
    let code = format!("code-{}", s.next);
    s.codes.insert(code.clone(), Grant { challenge: challenge.into(), redirect_uri: redirect.into(), client_id: client_id.into() });
    Reply::redirect(302, &format!("{redirect}/?code={}&state={}", url::encode(&code), url::encode(state)))
}

fn issue(s: &mut State, with_refresh: bool) -> Value {
    s.next += 1;
    let access = format!("at-{}", s.next);
    s.access.insert(access.clone());
    let mut v = json!({ "access_token": access, "token_type": "Bearer", "expires_in": 3600 });
    if with_refresh {
        s.next += 1;
        let refresh = format!("rt-{}", s.next);
        s.refresh.insert(refresh.clone());
        v["refresh_token"] = json!(refresh);
    }
    v
}

fn oauth_error(error: &str, description: &str) -> Reply {
    json_reply(400, &json!({ "error": error, "error_description": description }))
}

fn token(s: &mut State, r: &Recorded) -> Reply {
    s.token_requests += 1;
    let form = url::parse_query(&String::from_utf8_lossy(&r.body));
    let get = |k: &str| get(&form, k).map(str::to_string);
    if let Some(secret) = s.client_secret.clone()
        && get("client_secret").as_deref() != Some(secret.as_str())
    {
        return oauth_error("invalid_client", "client_secret is missing.");
    }
    match get("grant_type").as_deref() {
        Some("authorization_code") => {
            let Some(grant) = get("code").and_then(|c| s.codes.remove(&c)) else { return oauth_error("invalid_grant", "Bad Request") };
            let verifier = get("code_verifier").unwrap_or_default();
            let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            if challenge != grant.challenge {
                return oauth_error("invalid_grant", "Invalid code verifier.");
            }
            if get("redirect_uri").as_deref() != Some(grant.redirect_uri.as_str()) || get("client_id").as_deref() != Some(grant.client_id.as_str()) {
                return oauth_error("invalid_grant", "redirect_uri or client_id mismatch.");
            }
            let with_refresh = !s.withhold_refresh_token;
            json_reply(200, &issue(s, with_refresh))
        }
        Some("refresh_token") => {
            let Some(old) = get("refresh_token").filter(|t| s.refresh.contains(t)) else {
                return oauth_error("invalid_grant", "Token has been expired or revoked.");
            };
            let rotate = s.rotate_refresh;
            if rotate {
                s.refresh.remove(&old);
            }
            json_reply(200, &issue(s, rotate))
        }
        _ => oauth_error("unsupported_grant_type", "unsupported"),
    }
}

// ---- Google ---------------------------------------------------------------------------------

fn google_json(f: &FakeFile, base: &str) -> Value {
    let native = f.mime.starts_with("application/vnd.google-apps.") && !f.is_folder;
    let mut v = json!({
        "id": f.id, "name": f.name, "mimeType": f.mime,
        "modifiedTime": format!("2026-01-01T00:00:{:02}Z", f.version.min(59)),
        "version": f.version.to_string(),
        "capabilities": { "canEdit": true },
        "webViewLink": format!("{base}/view/{}", f.id),
    });
    if let Some(p) = &f.parent {
        v["parents"] = json!([p]);
    }
    if !f.is_folder && !native {
        v["size"] = json!(f.bytes.len().to_string());
    }
    v
}

/// Splits `text` at ` and ` outside quotes and parentheses.
fn split_and(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let (mut depth, mut quoted, mut escaped, mut current) = (0i32, false, false, String::new());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        if quoted {
            current.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '\'' {
                quoted = false;
            }
        } else if c == '\'' {
            quoted = true;
            current.push(c);
        } else if c == '(' || c == ')' {
            depth += if c == '(' { 1 } else { -1 };
            current.push(c);
        } else if depth == 0 && chars.get(i..i + 5).is_some_and(|w| w.iter().collect::<String>() == " and ") {
            parts.push(std::mem::take(&mut current));
            i += 4;
        } else {
            current.push(c);
        }
        i += 1;
    }
    parts.push(current);
    parts
}

/// The string literals of `text` ('...' with backslash escapes), unescaped.
fn literals(text: &str) -> Vec<String> {
    let (mut out, mut inside, mut escaped, mut current) = (Vec::new(), false, false, String::new());
    for c in text.chars() {
        if inside {
            if escaped {
                current.push(c);
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '\'' {
                inside = false;
                out.push(std::mem::take(&mut current));
            } else {
                current.push(c);
            }
        } else if c == '\'' {
            inside = true;
        }
    }
    out
}

fn google_matches(q: &str, f: &FakeFile) -> bool {
    split_and(q).iter().all(|clause| {
        let clause = clause.trim();
        let lits = literals(clause);
        if clause == "trashed = false" {
            true
        } else if clause.ends_with(" in parents") {
            let parent = lits.first().map(String::as_str).unwrap_or_default();
            if parent == "root" { f.parent.is_none() } else { f.parent.as_deref() == Some(parent) }
        } else if clause.starts_with("name contains ") {
            lits.first().is_some_and(|t| f.name.to_lowercase().contains(&t.to_lowercase()))
        } else if clause.starts_with('(') && clause.contains("mimeType = ") {
            lits.contains(&f.mime)
        } else {
            false
        }
    })
}

fn google(s: &mut State, r: &Recorded, rest: &str, q: &[(String, String)]) -> Reply {
    let base = host_base(r);
    if rest == "/about" {
        return json_reply(200, &json!({ "user": { "emailAddress": s.email, "displayName": s.name } }));
    }
    if rest == "/files" && r.method == "GET" {
        let mut hits: Vec<&FakeFile> = s.files.values().filter(|f| google_matches(get(q, "q").unwrap_or_default(), f)).collect();
        hits.sort_by(|a, b| b.is_folder.cmp(&a.is_folder).then_with(|| a.name.cmp(&b.name)));
        let size = get(q, "pageSize").and_then(|v| v.parse::<usize>().ok()).unwrap_or(100).min(s.page_size_cap.unwrap_or(usize::MAX));
        let start = get(q, "pageToken").and_then(|t| t.strip_prefix("tok").and_then(|n| n.parse::<usize>().ok())).unwrap_or(0);
        let page: Vec<Value> = hits.iter().skip(start).take(size).map(|f| google_json(f, &base)).collect();
        let mut out = json!({ "files": page });
        if start + size < hits.len() {
            out["nextPageToken"] = json!(format!("tok{}", start + size));
        }
        return json_reply(200, &out);
    }
    if let Some(tail) = rest.strip_prefix("/files/") {
        let (id, export) = match tail.strip_suffix("/export") {
            Some(id) => (id, true),
            None => (tail, false),
        };
        let id = url::decode(id);
        let Some(f) = s.files.get(&id) else { return error(404, "File not found") };
        if export {
            return Reply::new(200, "application/pdf", sample_pdf(&f.name));
        }
        if get(q, "alt") == Some("media") {
            return Reply::new(200, &f.mime, f.bytes.clone());
        }
        return json_reply(200, &google_json(f, &base));
    }
    error(404, "no such Drive call")
}

/// The parts of a `multipart/related` body.
fn multipart_parts(content_type: &str, body: &[u8]) -> Vec<Vec<u8>> {
    let Some(boundary) = content_type.split("boundary=").nth(1).map(|b| b.trim().trim_matches('"').to_string()) else { return Vec::new() };
    let marker = format!("--{boundary}");
    let mut positions = Vec::new();
    let mut i = 0;
    while let Some(p) = body.get(i..).and_then(|rest| rest.windows(marker.len()).position(|w| w == marker.as_bytes())) {
        positions.push(i + p);
        i += p + marker.len();
    }
    let mut parts = Vec::new();
    for pair in positions.windows(2) {
        let (Some(&a), Some(&b)) = (pair.first(), pair.get(1)) else { continue };
        let chunk = body.get(a + marker.len()..b).unwrap_or_default();
        let chunk = chunk.strip_prefix(b"\r\n").unwrap_or(chunk);
        let chunk = chunk.strip_suffix(b"\r\n").unwrap_or(chunk);
        if let Some(split) = chunk.windows(4).position(|w| w == b"\r\n\r\n") {
            parts.push(chunk.get(split + 4..).unwrap_or_default().to_vec());
        }
    }
    parts
}

fn google_upload(s: &mut State, r: &Recorded, rest: &str, q: &[(String, String)]) -> Reply {
    let base = host_base(r);
    let kind = get(q, "uploadType").unwrap_or_default();
    let body_json = |bytes: &[u8]| serde_json::from_slice::<Value>(bytes).unwrap_or(Value::Null);
    match (r.method.as_str(), rest, kind) {
        ("POST", "/files", "multipart") => {
            let parts = multipart_parts(r.header("content-type").unwrap_or_default(), &r.body);
            let (Some(meta), Some(content)) = (parts.first(), parts.get(1)) else { return error(400, "bad multipart body") };
            let meta = body_json(meta);
            let name = meta["name"].as_str().unwrap_or("untitled");
            let parent = meta.pointer("/parents/0").and_then(Value::as_str).map(str::to_string).filter(|p| p != "root");
            let mime = meta["mimeType"].as_str().unwrap_or("application/octet-stream");
            let id = create(s, parent, name, mime, content.clone(), false);
            json_reply(200, &google_json(&s.files[&id], &base))
        }
        ("POST", "/files", "resumable") | ("PATCH", _, "resumable") => {
            let meta = body_json(&r.body);
            let target = if r.method == "POST" {
                Target::New {
                    name: meta["name"].as_str().unwrap_or("untitled").into(),
                    parent: meta.pointer("/parents/0").and_then(Value::as_str).map(str::to_string).filter(|p| p != "root"),
                    mime: meta["mimeType"].as_str().unwrap_or("application/octet-stream").into(),
                }
            } else {
                Target::Replace(url::decode(rest.trim_start_matches("/files/")))
            };
            let total = r.header("x-upload-content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            s.next += 1;
            let sid = format!("s{}", s.next);
            s.sessions.insert(sid.clone(), Session { target, graph: false, total, received: Vec::new() });
            let mut reply = Reply::new(200, "application/json", "{}");
            reply.headers.push(("Location".into(), format!("{base}/upload/session/{sid}")));
            reply
        }
        ("PATCH", _, "media") => {
            let id = url::decode(rest.trim_start_matches("/files/"));
            let Some(f) = s.files.get_mut(&id) else { return error(404, "File not found") };
            f.bytes = r.body.clone();
            f.version += 1;
            json_reply(200, &google_json(f, &base))
        }
        _ => error(400, "unsupported upload"),
    }
}

/// A chunk of a resumable (Google) or upload-session (Graph) upload.
fn session_put(s: &mut State, sid: &str, r: &Recorded) -> Reply {
    if r.header("authorization").is_some() {
        return error(400, "a pre-authorized upload address must not get a token");
    }
    let base = host_base(r);
    let Some(range) = r.header("content-range").and_then(|v| v.strip_prefix("bytes ")) else { return error(400, "no Content-Range") };
    let Some((span, total)) = range.split_once('/') else { return error(400, "bad Content-Range") };
    let (Some((start, end)), Ok(total)) = (span.split_once('-'), total.parse::<usize>()) else { return error(400, "bad Content-Range") };
    let (Ok(start), Ok(end)) = (start.parse::<usize>(), end.parse::<usize>()) else { return error(400, "bad Content-Range") };
    let Some(session) = s.sessions.get_mut(sid) else { return error(404, "unknown upload session") };
    if session.total == 0 {
        session.total = total;
    }
    if start != session.received.len() || end + 1 != start + r.body.len() || total != session.total {
        return error(416, "chunk out of order");
    }
    session.received.extend_from_slice(&r.body);
    if session.received.len() < total {
        return if session.graph {
            json_reply(202, &json!({ "nextExpectedRanges": [format!("{}-", session.received.len())] }))
        } else {
            let mut reply = Reply::new(308, "text/plain", "");
            reply.headers.push(("Range".into(), format!("bytes=0-{}", session.received.len() - 1)));
            reply
        };
    }
    let Some(done) = s.sessions.remove(sid) else { return error(404, "unknown upload session") };
    let graph = done.graph;
    let id = match done.target {
        Target::New { name, parent, mime } => create(s, parent, &name, &mime, done.received, false),
        Target::Replace(id) => {
            let Some(f) = s.files.get_mut(&id) else { return error(404, "File not found") };
            f.bytes = done.received;
            f.version += 1;
            id
        }
    };
    let Some(f) = s.files.get(&id) else { return error(500, "lost the file") };
    json_reply(if graph { 201 } else { 200 }, &if graph { graph_json(f, &base) } else { google_json(f, &base) })
}

// ---- Graph ----------------------------------------------------------------------------------

fn graph_json(f: &FakeFile, base: &str) -> Value {
    let mut v = json!({
        "id": f.id, "name": f.name, "size": f.bytes.len(),
        "eTag": format!("\"{{{}}},{}\"", f.id, f.version),
        "lastModifiedDateTime": "2026-01-01T00:00:00Z",
        "webUrl": format!("{base}/view/{}", f.id),
        "parentReference": { "id": f.parent.clone().unwrap_or_else(|| "root".into()) },
    });
    if f.is_folder {
        v["folder"] = json!({ "childCount": 0 });
    } else {
        v["file"] = json!({ "mimeType": f.mime });
    }
    v
}

/// `name`, or `stem (n).ext` for the first free n among the siblings.
fn unique_name(s: &State, parent: &Option<String>, name: &str) -> String {
    let taken = |n: &str| s.files.values().any(|f| &f.parent == parent && f.name == n);
    if !taken(name) {
        return name.to_string();
    }
    let (stem, ext) = name.rsplit_once('.').map_or((name, String::new()), |(a, b)| (a, format!(".{b}")));
    (1..).map(|n| format!("{stem} ({n}){ext}")).find(|n| !taken(n)).unwrap_or_else(|| name.to_string())
}

/// `:/name:` addressed parents: `("root" | "items/<id>", name)`.
fn graph_parent_and_name(rest: &str) -> Option<(Option<String>, String, &str)> {
    let rest = rest.strip_prefix("/me/drive/")?;
    let (parent_part, tail) = rest.split_once(":/")?;
    let (name, after) = tail.split_once(':')?;
    let parent = match parent_part {
        "root" => None,
        other => Some(url::decode(other.strip_prefix("items/")?)),
    };
    Some((parent, url::decode(name), after))
}

fn graph(s: &mut State, r: &Recorded, rest: &str, q: &[(String, String)]) -> Reply {
    let base = host_base(r);
    if rest == "/me" {
        return json_reply(200, &json!({ "displayName": s.name, "mail": s.email, "userPrincipalName": s.email }));
    }
    if let Some((parent, name, after)) = graph_parent_and_name(rest) {
        let conflict = get(q, "@microsoft.graph.conflictBehavior").unwrap_or("fail");
        return match (r.method.as_str(), after) {
            ("PUT", "/content") => {
                let name = if conflict == "rename" { unique_name(s, &parent, &name) } else { name };
                let id = create(s, parent, &name, "application/octet-stream", r.body.clone(), false);
                json_reply(201, &graph_json(&s.files[&id], &base))
            }
            ("POST", "/createUploadSession") => {
                let name = if conflict == "rename" || r.body.windows(6).any(|w| w == b"rename") { unique_name(s, &parent, &name) } else { name };
                s.next += 1;
                let sid = format!("g{}", s.next);
                s.sessions.insert(
                    sid.clone(),
                    Session {
                        target: Target::New { name, parent, mime: "application/octet-stream".into() },
                        graph: true,
                        total: 0,
                        received: Vec::new(),
                    },
                );
                json_reply(200, &json!({ "uploadUrl": format!("{base}/upload/session/{sid}"), "expirationDateTime": "2099-01-01T00:00:00Z" }))
            }
            _ => error(400, "unsupported call"),
        };
    }
    if rest == "/me/drive/root/children" || (rest.starts_with("/me/drive/items/") && rest.ends_with("/children")) {
        let parent = if rest == "/me/drive/root/children" {
            None
        } else {
            Some(url::decode(rest.trim_start_matches("/me/drive/items/").trim_end_matches("/children")))
        };
        let hits: Vec<&FakeFile> = s.files.values().filter(|f| f.parent == parent).collect();
        return graph_page(s, &hits, q, &base, rest);
    }
    if let Some(term) = rest.strip_prefix("/me/drive/root/search(q=%27").and_then(|t| t.strip_suffix("%27)")) {
        let term = url::decode(term).replace("''", "'").to_lowercase();
        let hits: Vec<&FakeFile> = s.files.values().filter(|f| f.name.to_lowercase().contains(&term)).collect();
        return graph_page(s, &hits, q, &base, rest);
    }
    if let Some(tail) = rest.strip_prefix("/me/drive/items/") {
        let (id, action) = tail.split_once('/').map_or((tail, ""), |(a, b)| (a, b));
        let id = url::decode(id);
        return match (r.method.as_str(), action) {
            ("GET", "") => match s.files.get(&id) {
                Some(f) => json_reply(200, &graph_json(f, &base)),
                None => json_error_graph(404, "itemNotFound", "The resource could not be found."),
            },
            ("GET", "content") => match s.files.get(&id) {
                Some(f) => Reply::redirect(302, &format!("{base}/blob/{}-{}", f.id, f.version)),
                None => json_error_graph(404, "itemNotFound", "The resource could not be found."),
            },
            ("PUT", "content") => match s.files.get_mut(&id) {
                Some(f) => {
                    f.bytes = r.body.clone();
                    f.version += 1;
                    json_reply(200, &graph_json(f, &base))
                }
                None => json_error_graph(404, "itemNotFound", "The resource could not be found."),
            },
            ("POST", "createUploadSession") => {
                if !s.files.contains_key(&id) {
                    return json_error_graph(404, "itemNotFound", "The resource could not be found.");
                }
                s.next += 1;
                let sid = format!("g{}", s.next);
                s.sessions.insert(sid.clone(), Session { target: Target::Replace(id), graph: true, total: 0, received: Vec::new() });
                json_reply(200, &json!({ "uploadUrl": format!("{base}/upload/session/{sid}"), "expirationDateTime": "2099-01-01T00:00:00Z" }))
            }
            _ => error(400, "unsupported call"),
        };
    }
    error(404, "no such Graph call")
}

fn json_error_graph(status: u16, code: &str, message: &str) -> Reply {
    json_reply(status, &json!({ "error": { "code": code, "message": message } }))
}

fn graph_page(s: &State, hits: &[&FakeFile], q: &[(String, String)], base: &str, rest: &str) -> Reply {
    let top = get(q, "$top").and_then(|v| v.parse::<usize>().ok()).unwrap_or(100).min(s.page_size_cap.unwrap_or(usize::MAX));
    let start = get(q, "$skip").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    let page: Vec<Value> = hits.iter().skip(start).take(top).map(|f| graph_json(f, base)).collect();
    let mut out = json!({ "value": page });
    if start + top < hits.len() {
        let carried: String = q.iter().filter(|(k, _)| k != "$skip").map(|(k, v)| format!("{}={}", k, url::encode(v))).collect::<Vec<_>>().join("&");
        out["@odata.nextLink"] = json!(s.foreign_next_link.clone().unwrap_or_else(|| format!("{base}/v1.0{rest}?{carried}&$skip={}", start + top)));
    }
    json_reply(200, &out)
}
