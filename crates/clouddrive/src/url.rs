//! Addresses: percent-encoding, query strings, and the one rule about where a token may go.
//!
//! A token is sent only to `https` addresses, or to `http` on this computer (the tests' server).
//! A redirect never carries one to another host.

use craft_http_url::Url;

use crate::error::CloudError;

/// Percent-encodes everything but RFC 3986's unreserved characters, for a query value, a form
/// value or one path segment.
pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Reverses [`encode`] (and `+` as a space, as a query string means it). Invalid escapes stay as
/// they are; invalid UTF-8 becomes U+FFFD.
pub fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'+' => out.push(b' '),
            b'%' => {
                let pair = bytes.get(i + 1).copied().and_then(hex).zip(bytes.get(i + 2).copied().and_then(hex));
                if let Some((hi, lo)) = pair {
                    out.push(hi * 16 + lo);
                    i += 2;
                } else {
                    out.push(b);
                }
            }
            _ => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `name=value` pairs of a query string (the text after `?`), decoded. At most 64 pairs.
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .take(64)
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (decode(k), decode(v)),
            None => (decode(pair), String::new()),
        })
        .collect()
}

/// `name=value&...`, encoded.
pub fn form(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", encode(k), encode(v))).collect::<Vec<_>>().join("&")
}

/// Whether `host` (lower case, no brackets) is this computer.
pub fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host.ends_with(".localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Checks `url` is an address a token may be sent to: `https`, or `http` on this computer.
pub fn check_endpoint(url: &str) -> Result<Url, CloudError> {
    let parsed = Url::parse(url).map_err(|e| CloudError::Invalid(format!("not a usable address: {e}")))?;
    if !parsed.tls && !is_loopback_host(&parsed.host) {
        return Err(CloudError::Invalid("use https: a sign-in or a token is never sent over plain http to another computer".into()));
    }
    if url.contains('#') {
        return Err(CloudError::Invalid("an address cannot carry a fragment".into()));
    }
    Ok(parsed)
}

/// Whether `a` and `b` name the same scheme, host and port.
pub fn same_origin(a: &Url, b: &Url) -> bool {
    a.tls == b.tls && a.host == b.host && a.port == b.port
}

/// `scheme://host[:port]` of a parsed address.
pub fn origin(url: &Url) -> String {
    format!("{}://{}", if url.tls { "https" } else { "http" }, url.host_header())
}

/// One path segment of an id or name: encoded, and never `.` or `..` or empty.
pub fn segment(text: &str) -> Result<String, CloudError> {
    if text.is_empty() || text == "." || text == ".." || text.len() > 1024 || text.chars().any(char::is_control) {
        return Err(CloudError::Invalid("that file id cannot be used in an address".into()));
    }
    Ok(encode(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_round_trips_and_escapes_separators() {
        let text = "a b/c?d=e&f#g'é+\"";
        let encoded = encode(text);
        assert!(!encoded.contains(['/', '?', '&', '#', ' ', '\'', '+', '"']));
        assert_eq!(decode(&encoded), text);
        assert_eq!(decode("a+b%20c%zz%4"), "a b c%zz%4");
    }

    #[test]
    fn a_query_is_split_and_decoded() {
        let q = parse_query("code=4%2FabC&state=s%20t&flag&=x&&scope=a+b");
        assert_eq!(q[0], ("code".to_string(), "4/abC".to_string()));
        assert_eq!(q[1], ("state".to_string(), "s t".to_string()));
        assert_eq!(q[2], ("flag".to_string(), String::new()));
        assert_eq!(q.last().unwrap(), &("scope".to_string(), "a b".to_string()));
    }

    #[test]
    fn a_token_goes_only_to_https_or_this_computer() {
        assert!(check_endpoint("https://www.googleapis.com/drive/v3").is_ok());
        assert!(check_endpoint("http://127.0.0.1:8080/x").is_ok());
        assert!(check_endpoint("http://localhost:8080/x").is_ok());
        assert!(check_endpoint("http://[::1]:8080/x").is_ok());
        assert!(check_endpoint("http://www.googleapis.com/drive/v3").is_err());
        assert!(check_endpoint("ftp://x/").is_err());
        assert!(check_endpoint("https://user:pw@x.example/").is_err());
        assert!(check_endpoint("https://x.example/#frag").is_err());
    }

    #[test]
    fn a_segment_cannot_climb() {
        assert!(segment("..").is_err() && segment(".").is_err() && segment("").is_err());
        assert!(segment("a\nb").is_err());
        assert_eq!(segment("a/b").unwrap(), "a%2Fb");
        assert_eq!(segment("01ABC!x").unwrap(), "01ABC%21x");
    }

    #[test]
    fn origins_compare_scheme_host_and_port() {
        let a = Url::parse("https://graph.microsoft.com/v1.0/x").unwrap();
        assert!(same_origin(&a, &Url::parse("https://graph.microsoft.com/other").unwrap()));
        assert!(!same_origin(&a, &Url::parse("https://graph.microsoft.com:444/x").unwrap()));
        assert!(!same_origin(&a, &Url::parse("https://evil.example/x").unwrap()));
        assert!(!same_origin(&a, &Url::parse("http://graph.microsoft.com/x").unwrap()));
    }
}
