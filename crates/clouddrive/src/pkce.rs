//! PKCE (RFC 7636) with the S256 method, and the random `state` of an authorization request.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::error::CloudError;

/// Random bytes in a code verifier: 32 bytes are 43 characters, the shortest RFC 7636 allows, with
/// 256 bits of entropy.
const VERIFIER_BYTES: usize = 32;

/// Random bytes in a `state` value.
const STATE_BYTES: usize = 24;

/// A code verifier and the challenge that goes in the sign-in address.
pub struct Pkce {
    verifier: Zeroizing<String>,
    pub challenge: String,
}

impl Pkce {
    /// A fresh verifier from the operating system's random source.
    pub fn new() -> Result<Pkce, CloudError> {
        let mut bytes = Zeroizing::new([0u8; VERIFIER_BYTES]);
        getrandom::fill(&mut *bytes).map_err(|e| CloudError::SignIn(format!("no randomness: {e}")))?;
        Ok(Pkce::from_verifier(URL_SAFE_NO_PAD.encode(*bytes)))
    }

    /// The pair for a given verifier (RFC 7636 appendix B uses this in its test vector).
    pub fn from_verifier(verifier: String) -> Pkce {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Pkce { verifier: Zeroizing::new(verifier), challenge }
    }

    /// The verifier, for the token request only.
    pub fn verifier(&self) -> &str {
        &self.verifier
    }
}

/// An unguessable `state` for one authorization request.
pub fn new_state() -> Result<String, CloudError> {
    let mut bytes = [0u8; STATE_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| CloudError::SignIn(format!("no randomness: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Compares two strings without stopping at the first difference.
pub fn same_secret(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_challenge_is_the_rfc_7636_test_vector() {
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_string());
        assert_eq!(pkce.challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn a_fresh_verifier_has_the_rfc_length_and_alphabet_and_never_repeats() {
        let a = Pkce::new().unwrap();
        let b = Pkce::new().unwrap();
        assert_eq!(a.verifier().len(), 43);
        assert!(a.verifier().chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(a.verifier(), b.verifier());
        assert_ne!(new_state().unwrap(), new_state().unwrap());
    }

    #[test]
    fn secrets_compare_by_value() {
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abc", "abd"));
        assert!(!same_secret("abc", "abcd"));
        assert!(!same_secret("", "a"));
        assert!(same_secret("", ""));
    }
}
