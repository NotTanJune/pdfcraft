//! The HTTP client both features share (native builds only).
//!
//! No cookie jar (ureq's `cookies` feature is not built), no automatic redirects (the callers
//! follow them one hop at a time so that every hop is checked and `Authorization` is never sent to
//! another place), and a status of 4xx or 5xx is a response, not an error, so its message can be
//! read. The certificate chain is checked by the operating system, as in craft-update.

use std::io::Read;
use std::time::Duration;

/// `PdfCraft/<version>`, the `User-Agent` of every request.
pub(crate) fn user_agent() -> String {
    format!("PdfCraft/{}", env!("CARGO_PKG_VERSION"))
}

/// An agent that follows no redirects and gives up after `timeout` for the whole request.
pub(crate) fn agent(timeout: Duration) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .https_only(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .user_agent(user_agent())
        .build()
        .new_agent()
}

/// Reads at most `max` bytes of the decoded body. The flag says there was more: the rest is not
/// read. A transport error that arrives after some bytes keeps those bytes (a page cut off by the
/// time limit still has its head).
pub(crate) fn read_capped(response: &mut ureq::http::Response<ureq::Body>, max: usize) -> (Vec<u8>, bool) {
    // One byte past the cap tells "exactly max" from "more than max".
    let mut reader = response.body_mut().with_config().reader().take((max as u64).saturating_add(1));
    let mut bytes = Vec::new();
    let _ = reader.read_to_end(&mut bytes);
    let more = bytes.len() > max;
    bytes.truncate(max);
    (bytes, more)
}
