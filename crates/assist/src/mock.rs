//! A tiny HTTP server on `127.0.0.1` for tests (feature `mock`, native only).
//!
//! It answers each request with whatever the handler returns, one connection per request, and
//! records what it received, so a test can check the path, the headers (the `Authorization` the
//! client sent) and the body. Tests never reach the real network.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// The most header bytes and body bytes the server reads from one request.
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 4 * 1024 * 1024;

/// A request the server received.
#[derive(Clone, Debug, Default)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    /// Header names lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    /// The first header named `name` (any letter case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// What the handler answers.
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Reply { status, headers: vec![("Content-Type".into(), content_type.into())], body: body.into() }
    }

    pub fn json(body: &str) -> Self {
        Self::new(200, "application/json", body)
    }

    pub fn html(body: &str) -> Self {
        Self::new(200, "text/html; charset=utf-8", body)
    }

    pub fn redirect(status: u16, location: &str) -> Self {
        Reply { status, headers: vec![("Location".into(), location.into())], body: Vec::new() }
    }
}

type Handler = dyn Fn(&Recorded) -> Reply + Send + Sync + 'static;

/// A running server. Dropping it stops it.
pub struct MockServer {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Recorded>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    /// Starts a server on a free loopback port.
    pub fn start(handler: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> std::io::Result<MockServer> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Arc<Handler> = Arc::new(handler);
        let (thread_seen, thread_stop) = (seen.clone(), stop.clone());
        let thread = std::thread::Builder::new().name("pdfcraft-mock-server".into()).spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (handler, seen) = (handler.clone(), thread_seen.clone());
                let _ = std::thread::Builder::new().name("pdfcraft-mock-conn".into()).spawn(move || {
                    let _ = serve(stream, &*handler, &seen);
                });
            }
        })?;
        Ok(MockServer { addr, seen, stop, thread: Some(thread) })
    }

    /// `http://127.0.0.1:<port>`, no trailing slash.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Everything received so far, oldest first.
    pub fn requests(&self) -> Vec<Recorded> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(500));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn serve(mut stream: TcpStream, handler: &Handler, seen: &Mutex<Vec<Recorded>>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(i) = find(&buffer, b"\r\n\r\n") {
            break i;
        }
        if buffer.len() > MAX_HEAD {
            return Ok(());
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(chunk.get(..n).unwrap_or_default());
    };
    let head = String::from_utf8_lossy(buffer.get(..head_end).unwrap_or_default()).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let mut request = Recorded {
        method: request_line.next().unwrap_or_default().to_string(),
        path: request_line.next().unwrap_or_default().to_string(),
        ..Recorded::default()
    };
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            request.headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let wanted = request.header("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0).min(MAX_BODY);
    let mut body = buffer.get(head_end + 4..).unwrap_or_default().to_vec();
    while body.len() < wanted {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(chunk.get(..n).unwrap_or_default());
    }
    body.truncate(wanted);
    request.body = body;
    let reply = handler(&request);
    seen.lock().unwrap_or_else(PoisonError::into_inner).push(request);
    let mut out = format!("HTTP/1.1 {} Mock\r\nContent-Length: {}\r\nConnection: close\r\n", reply.status, reply.body.len()).into_bytes();
    for (name, value) in &reply.headers {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&reply.body);
    stream.write_all(&out)?;
    stream.flush()
}
