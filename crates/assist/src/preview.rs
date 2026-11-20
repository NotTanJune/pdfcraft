//! A web link's hover card: the page's title and description.
//!
//! The fetch is a plain `GET` of the page's HTML and nothing else: no cookies, no scripts run, no
//! images, styles or frames loaded, no `Referer`. It is bounded at every step:
//!
//! - `http` and `https` only (`craft_http_url::Url` refuses everything else, and any redirect to
//!   another scheme or from `https` down to `http`);
//! - at most [`MAX_REDIRECTS`] redirects, each followed by hand so the next address is checked;
//! - [`TIMEOUT`] for the whole fetch, [`MAX_BODY_BYTES`] of the page read (the rest is ignored);
//! - unless the policy allows it, an address that is this computer or a private network (loopback,
//!   RFC 1918, link-local, carrier-grade NAT, unique-local) is refused before connecting, so a
//!   document cannot make PdfCraft probe the reader's own network. A name that resolves to a
//!   public address when checked and a private one when connecting (DNS rebinding) is not caught
//!   here; the answer is only ever shown as a title and a description, and never sent anywhere.
//!
//! [`parse`] reads the head of the HTML with `craft_html`'s tolerant tokenizer: `og:title`,
//! `og:description`, `og:site_name`, `<title>`, `<meta name="description">`, and Twitter's
//! equivalents. Text is unescaped, stripped of control and bidirectional-override characters,
//! collapsed to one line and clipped.

use std::time::Duration;

use craft_html::{Lexer, Options, Token, unescape};

/// The most of the page read.
pub const MAX_BODY_BYTES: usize = 512 * 1024;
/// The whole fetch, redirects included.
pub const TIMEOUT: Duration = Duration::from_secs(5);
/// The most redirects followed.
pub const MAX_REDIRECTS: usize = 3;
/// The longest title and description kept, in characters.
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_DESCRIPTION_CHARS: usize = 400;
/// The most tokens read while looking for the head's metadata.
const MAX_TOKENS: usize = 50_000;

/// What the fetch may reach.
#[derive(Clone, Copy, Debug, Default)]
pub struct FetchPolicy {
    /// Allow addresses on this computer or a private network. Off in the app; tests that serve a
    /// page from `127.0.0.1` turn it on.
    pub allow_private_hosts: bool,
}

/// What a page says about itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageMeta {
    pub title: Option<String>,
    pub description: Option<String>,
    pub site_name: Option<String>,
}

/// A fetched card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkCard {
    /// Where the page ended up after redirects, without a fragment.
    pub final_url: String,
    pub meta: PageMeta,
}

/// Why there is no card.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PreviewError {
    #[error("link previews need the desktop app; this build runs in a browser")]
    Unsupported,
    #[error("not a web address PdfCraft will fetch: {0}")]
    InvalidUrl(String),
    #[error("{0} is on this computer or a private network; PdfCraft does not fetch previews from there")]
    PrivateHost(String),
    #[error("could not reach the page: {0}")]
    Network(String),
    #[error("the page answered {0}")]
    Status(u16),
    #[error("the address is not a web page ({0})")]
    NotHtml(String),
    #[error("too many redirects")]
    TooManyRedirects,
}

/// Whether `ip` is an address a document may make PdfCraft connect to: not this computer, not a
/// private or link-local network, not reserved or multicast.
pub fn is_public_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                // 100.64.0.0/10 carrier-grade NAT
                || (a == 100 && (b & 0xC0) == 64)
                // 192.0.0.0/24 IETF protocol assignments
                || (a == 192 && b == 0 && c == 0)
                // 198.18.0.0/15 benchmarking
                || (a == 198 && (b & 0xFE) == 18)
                // 240.0.0.0/4 reserved
                || a >= 240
                // 0.0.0.0/8 "this network"
                || a == 0)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let first = v6.segments().first().copied().unwrap_or(0);
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // fc00::/7 unique local, fe80::/10 link local
                || (first & 0xFE00) == 0xFC00
                || (first & 0xFFC0) == 0xFE80
                // 2001:db8::/32 documentation
                || (first == 0x2001 && v6.segments().get(1) == Some(&0x0DB8)))
        }
    }
}

/// Removes control and bidirectional-override characters, collapses white space to single spaces
/// and clips to `max` characters. A page's title cannot reorder or hide what is drawn around it.
fn clean(text: &str, max: usize) -> Option<String> {
    let text = unescape(text);
    let kept: String = text
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !c.is_control() && !matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'))
        .collect();
    let joined = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.is_empty() {
        return None;
    }
    let mut chars = joined.chars();
    let head: String = chars.by_ref().take(max).collect();
    Some(if chars.next().is_some() { format!("{}…", head.trim_end()) } else { head })
}

/// The title, description and site name in `html`'s head. Reads no further than the body's start
/// or [`MAX_TOKENS`] tokens.
pub fn parse(html: &str) -> PageMeta {
    let options = Options { drop: &["script", "style", "noscript", "template", "svg"], raw_text: &["title", "textarea"], ..Options::default() };
    let lexer = Lexer::with(html, options);
    let (mut og_title, mut title, mut twitter_title) = (None, None, None);
    let (mut og_description, mut description, mut twitter_description) = (None, None, None);
    let mut site_name = None;
    let mut in_title = false;
    let mut tokens = 0usize;
    for token in lexer {
        tokens += 1;
        if tokens > MAX_TOKENS {
            break;
        }
        match token {
            Token::Start(tag) => match tag.name.as_str() {
                "title" => in_title = true,
                "body" => break,
                "meta" => {
                    let key = tag.attr("property").or_else(|| tag.attr("name")).map(|k| k.trim().to_ascii_lowercase());
                    let content = tag.attr("content");
                    let (Some(key), Some(content)) = (key, content) else { continue };
                    let slot = match key.as_str() {
                        "og:title" => &mut og_title,
                        "twitter:title" => &mut twitter_title,
                        "og:description" => &mut og_description,
                        "description" => &mut description,
                        "twitter:description" => &mut twitter_description,
                        "og:site_name" => &mut site_name,
                        _ => continue,
                    };
                    if slot.is_none() {
                        *slot = Some(content);
                    }
                }
                _ => {}
            },
            Token::Text(text) if in_title => {
                if title.is_none() {
                    title = Some(text.to_string());
                }
                in_title = false;
            }
            Token::End(name) if name == "title" => in_title = false,
            Token::End(name) if name == "head" => break,
            _ => {}
        }
    }
    let pick = |a: Option<String>, b: Option<String>, c: Option<String>, max: usize| [a, b, c].into_iter().flatten().find_map(|t| clean(&t, max));
    PageMeta {
        title: pick(og_title, title, twitter_title, MAX_TITLE_CHARS),
        description: pick(og_description, description, twitter_description, MAX_DESCRIPTION_CHARS),
        site_name: site_name.and_then(|s| clean(&s, MAX_TITLE_CHARS)),
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// `url` as the text of a request: scheme, authority, path and query, without a fragment.
fn request_url(url: &craft_http_url::Url) -> String {
    let scheme = if url.tls { "https" } else { "http" };
    format!("{scheme}://{}{}", url.host_header(), url.path)
}

/// Refuses an address on this computer or a private network, unless the policy allows it. Looks
/// the name up, so it blocks: callers run the whole fetch off the UI thread.
#[cfg(not(target_arch = "wasm32"))]
fn check_host(url: &craft_http_url::Url, policy: &FetchPolicy) -> Result<(), PreviewError> {
    use std::net::ToSocketAddrs;
    if policy.allow_private_hosts {
        return Ok(());
    }
    let private = || PreviewError::PrivateHost(url.host.clone());
    if let Ok(ip) = url.host.parse::<std::net::IpAddr>() {
        return if is_public_ip(ip) { Ok(()) } else { Err(private()) };
    }
    if url.host == "localhost" || url.host.ends_with(".localhost") {
        return Err(private());
    }
    let addrs: Vec<_> =
        (url.host.as_str(), url.port).to_socket_addrs().map_err(|e| PreviewError::Network(format!("could not find {}: {e}", url.host)))?.collect();
    if addrs.is_empty() {
        return Err(PreviewError::Network(format!("could not find {}", url.host)));
    }
    if addrs.iter().all(|a| is_public_ip(a.ip())) { Ok(()) } else { Err(private()) }
}

/// Fetches the card for `url`. Blocks for at most about [`TIMEOUT`]: run it off the UI thread.
#[cfg(not(target_arch = "wasm32"))]
pub fn fetch(url: &str, policy: &FetchPolicy) -> Result<LinkCard, PreviewError> {
    let started = std::time::Instant::now();
    let mut current = craft_http_url::Url::parse(url).map_err(|e| PreviewError::InvalidUrl(e.to_string()))?;
    let mut hops = 0usize;
    loop {
        check_host(&current, policy)?;
        let remaining =
            TIMEOUT.checked_sub(started.elapsed()).filter(|d| !d.is_zero()).ok_or_else(|| PreviewError::Network("the page took too long".into()))?;
        let agent = crate::net::agent(remaining);
        let target = request_url(&current);
        let mut response = agent.get(&target).header("Accept", "text/html,application/xhtml+xml;q=0.9,*/*;q=0.1").call().map_err(|e| {
            PreviewError::Network(match e {
                ureq::Error::Timeout(_) => "the page took too long".to_string(),
                other => other.to_string().chars().take(200).collect(),
            })
        })?;
        let status = response.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            hops += 1;
            if hops > MAX_REDIRECTS {
                return Err(PreviewError::TooManyRedirects);
            }
            let location = response.headers().get("location").and_then(|v| v.to_str().ok()).ok_or(PreviewError::Status(status))?;
            current = current.join(location).map_err(|e| PreviewError::InvalidUrl(e.to_string()))?;
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(PreviewError::Status(status));
        }
        let mime = response.body().mime_type().map(|m| m.to_ascii_lowercase());
        if let Some(mime) = mime.filter(|m| !(m == "text/html" || m == "application/xhtml+xml")) {
            return Err(PreviewError::NotHtml(mime));
        }
        let (bytes, _) = crate::net::read_capped(&mut response, MAX_BODY_BYTES);
        let html = String::from_utf8_lossy(&bytes);
        return Ok(LinkCard { final_url: request_url(&current).split('#').next().unwrap_or_default().to_string(), meta: parse(&html) });
    }
}

#[cfg(target_arch = "wasm32")]
pub fn fetch(_url: &str, _policy: &FetchPolicy) -> Result<LinkCard, PreviewError> {
    Err(PreviewError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn open_graph_beats_the_title_element() {
        let html = r#"<!doctype html><html><head><title>Plain title</title>
            <meta property="og:title" content="Shared &amp; loved"><meta name="description" content="Plain d">
            <meta property="og:description" content="OG description"><meta property="og:site_name" content="Example"></head>
            <body><h1>x</h1></body></html>"#;
        let meta = parse(html);
        assert_eq!(meta.title.as_deref(), Some("Shared & loved"));
        assert_eq!(meta.description.as_deref(), Some("OG description"));
        assert_eq!(meta.site_name.as_deref(), Some("Example"));
    }

    #[test]
    fn the_title_element_and_meta_description_stand_in() {
        let meta = parse("<title>\n  A   page\n title </title><META NAME=Description CONTENT='  Hello   there '>");
        assert_eq!(meta.title.as_deref(), Some("A page title"));
        assert_eq!(meta.description.as_deref(), Some("Hello there"));
        assert_eq!(meta.site_name, None);
    }

    #[test]
    fn scripts_and_styles_are_never_read_and_the_body_is_not_scanned() {
        let html = r#"<head><script>document.title = "<title>evil</title>"</script><style>title{}</style><title>Real</title></head>
            <body><meta property="og:title" content="from the body"><title>Late</title></body>"#;
        assert_eq!(parse(html).title.as_deref(), Some("Real"));
        assert_eq!(parse("<body><title>Late</title>").title, None);
    }

    #[test]
    fn text_is_cleaned_and_clipped() {
        let html = format!("<title>a\u{202E}b\u{0007}c\u{200B}d {}</title>", "x".repeat(500));
        let title = parse(&html).title.unwrap();
        assert!(title.starts_with("abcd xxx"), "{title:?}");
        assert!(title.chars().count() <= MAX_TITLE_CHARS + 1);
        assert!(title.ends_with('…'));
        assert_eq!(parse("<title>   </title>").title, None);
    }

    #[test]
    fn a_hostile_page_is_bounded() {
        let _ = parse(&"<meta ".repeat(100_000));
        let _ = parse(&format!("<title>{}", "é".repeat(1_000_000)));
        let _ = parse(&"<".repeat(1_000_000));
        let _ = parse("");
    }

    #[test]
    fn private_and_reserved_addresses_are_not_public() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "192.0.2.1",
            "198.18.0.1",
            "240.0.0.1",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "2001:db8::1",
        ] {
            assert!(!is_public_ip(ip.parse::<IpAddr>().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2606:4700:4700::1111", "::ffff:8.8.8.8"] {
            assert!(is_public_ip(ip.parse::<IpAddr>().unwrap()), "{ip}");
        }
    }

    #[test]
    fn local_addresses_are_refused_before_connecting() {
        let policy = FetchPolicy::default();
        for url in [
            "http://127.0.0.1/",
            "http://localhost:8080/x",
            "http://[::1]/",
            "http://192.168.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://a.localhost/",
        ] {
            assert!(matches!(fetch(url, &policy), Err(PreviewError::PrivateHost(_))), "{url}");
        }
    }

    #[test]
    fn other_schemes_are_not_addresses() {
        let policy = FetchPolicy::default();
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/",
            "javascript:alert(1)",
            "data:text/html,hi",
            "mailto:a@example.com",
            "https://user:pw@example.com/",
            "",
            "example.com",
        ] {
            assert!(matches!(fetch(url, &policy), Err(PreviewError::InvalidUrl(_))), "{url}");
        }
    }
}
