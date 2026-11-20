//! Opens an address in the person's default browser (native builds only).

use std::process::{Command, Stdio};

use crate::url;

/// Opens `address` with the system's opener (`open`, `xdg-open` or the Windows URL handler). Only
/// `https` addresses, or `http` on this computer, are opened, and the address is a single argument,
/// never a shell command line. Returns why it could not.
pub fn open_in_browser(address: &str) -> Result<(), String> {
    url::check_endpoint(address).map_err(|e| e.to_string())?;
    let mut command = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(address);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler").arg(address);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(address);
        c
    };
    let status =
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().map_err(|e| format!("could not start the browser: {e}"))?;
    if status.success() { Ok(()) } else { Err("the system could not open the browser".to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_addresses_are_handed_to_the_system() {
        for bad in ["file:///etc/passwd", "javascript:alert(1)", "http://example.com/", "-a Calculator", "https://u:p@example.com/"] {
            assert!(open_in_browser(bad).is_err(), "{bad}");
        }
    }
}
