//! The vault's cryptography: AES-256-GCM with a random 96-bit nonce per encryption, a versioned
//! header bound into the authentication, and Argon2id for the key that wraps the vault key.
//!
//! File layout (documents `PCVD`, metadata `PCVM`), all fields fixed:
//!
//! ```text
//! 0   4  magic        "PCVD" (a document) or "PCVM" (its metadata)
//! 4   1  version      1
//! 5   1  cipher       1 = AES-256-GCM
//! 6   2  reserved     0, 0 (refused when not zero)
//! 8  12  nonce        random, new for every write
//! 20  .  ciphertext, then the 16-byte GCM tag
//! ```
//!
//! The authenticated associated data is the first 8 header bytes, the vault's id, a zero byte and
//! the item's id: a file moved to another name, another vault or another kind of slot fails to
//! open instead of decrypting to something else.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use zeroize::Zeroizing;

use crate::error::{LibraryError, Result};
use crate::util::random_bytes;

pub const MAGIC_DOC: &[u8; 4] = b"PCVD";
pub const MAGIC_META: &[u8; 4] = b"PCVM";
/// The vault key wrapped by the passphrase's key, and the check blob under the vault key itself.
pub const MAGIC_WRAP: &[u8; 4] = b"PCVK";
pub const MAGIC_CHECK: &[u8; 4] = b"PCVC";
pub const VERSION: u8 = 1;
pub const CIPHER_AES256GCM: u8 = 1;
const HEADER_LEN: usize = 20;
const TAG_LEN: usize = 16;

/// The 256-bit vault key. Wiped from memory when dropped; never printed.
#[derive(Clone)]
pub struct VaultKey(Zeroizing<[u8; 32]>);

impl VaultKey {
    /// A fresh random key.
    pub fn generate() -> Result<VaultKey> {
        Ok(VaultKey(Zeroizing::new(random_bytes::<32>()?)))
    }

    /// A key from 32 bytes (read back from the key cache).
    pub fn from_bytes(bytes: &[u8]) -> Option<VaultKey> {
        let array: [u8; 32] = bytes.try_into().ok()?;
        Some(VaultKey(Zeroizing::new(array)))
    }

    /// The key's bytes, for the key cache only.
    pub fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for VaultKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VaultKey(<hidden>)")
    }
}

/// Argon2id settings, stored in `vault.json`. Read from a file that may be hostile, so
/// [`KdfParams::check`] bounds them before they are used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KdfParams {
    /// Memory in KiB.
    pub m_kib: u32,
    /// Passes.
    pub t: u32,
    /// Lanes.
    pub p: u32,
}

impl KdfParams {
    /// 64 MiB, 3 passes, 1 lane: about half a second on a laptop.
    pub const DEFAULT: KdfParams = KdfParams { m_kib: 64 * 1024, t: 3, p: 1 };
    /// The least a vault may be created with. Tests use it so they do not wait on 64 MiB of
    /// hashing; an app never does.
    pub const MIN_STRENGTH: KdfParams = KdfParams { m_kib: 8, t: 1, p: 1 };
    /// Most memory accepted from a file (512 MiB), passes (12) and lanes (8): a vault file from
    /// another Mac must not be able to make this one hash for minutes or exhaust memory.
    const MAX: KdfParams = KdfParams { m_kib: 512 * 1024, t: 12, p: 8 };

    pub fn check(&self) -> Result<()> {
        let ok =
            self.m_kib >= 8 * self.p && self.t >= 1 && self.p >= 1 && self.m_kib <= Self::MAX.m_kib && self.t <= Self::MAX.t && self.p <= Self::MAX.p;
        if ok { Ok(()) } else { Err(LibraryError::Damaged("the vault file asks for key-derivation settings that are out of range".to_string())) }
    }
}

/// The 32-byte key derived from `passphrase` and `salt`.
pub fn derive(passphrase: &str, salt: &[u8], params: KdfParams) -> Result<VaultKey> {
    params.check()?;
    let argon_params = argon2::Params::new(params.m_kib, params.t, params.p, Some(32))
        .map_err(|e| LibraryError::Damaged(format!("key derivation settings: {e}")))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, argon_params);
    let mut out = Zeroizing::new([0u8; 32]);
    argon.hash_password_into(passphrase.as_bytes(), salt, &mut *out).map_err(|e| LibraryError::Damaged(format!("key derivation failed: {e}")))?;
    Ok(VaultKey(out))
}

fn cipher(key: &VaultKey) -> Result<Aes256Gcm> {
    Aes256Gcm::new_from_slice(key.expose()).map_err(|_| LibraryError::Damaged("the vault key has the wrong length".to_string()))
}

fn associated(header: &[u8], vault_id: &str, item_id: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(8 + vault_id.len() + 1 + item_id.len());
    aad.extend_from_slice(header.get(..8).unwrap_or(header));
    aad.extend_from_slice(vault_id.as_bytes());
    aad.push(0);
    aad.extend_from_slice(item_id.as_bytes());
    aad
}

/// Encrypts `plaintext` into a file image of kind `magic`.
pub fn seal(key: &VaultKey, magic: &[u8; 4], vault_id: &str, item_id: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
    let nonce_bytes = random_bytes::<12>()?;
    let mut out = Vec::with_capacity(HEADER_LEN + plaintext.len() + TAG_LEN);
    out.extend_from_slice(magic);
    out.extend_from_slice(&[VERSION, CIPHER_AES256GCM, 0, 0]);
    out.extend_from_slice(&nonce_bytes);
    let aad = associated(&out, vault_id, item_id);
    let nonce = Nonce::try_from(&nonce_bytes[..]).map_err(|_| LibraryError::Damaged("nonce".to_string()))?;
    let sealed = cipher(key)?
        .encrypt(&nonce, Payload { msg: plaintext, aad: &aad })
        .map_err(|_| LibraryError::Damaged("the encryption failed".to_string()))?;
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Decrypts a file image of kind `magic`. A wrong key, a changed byte, or a file moved to another
/// name all fail the same way.
pub fn open(key: &VaultKey, magic: &[u8; 4], vault_id: &str, item_id: &str, file: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let damaged = || LibraryError::Damaged("this vault file is damaged, was changed, or belongs to another vault".to_string());
    if file.len() < HEADER_LEN + TAG_LEN || file.get(..4) != Some(&magic[..]) {
        return Err(damaged());
    }
    let (header, body) = file.split_at(HEADER_LEN);
    let (version, cipher_id, reserved) = (header[4], header[5], &header[6..8]);
    if version > VERSION {
        return Err(LibraryError::TooNew);
    }
    if version != VERSION || cipher_id != CIPHER_AES256GCM || reserved != [0, 0] {
        return Err(damaged());
    }
    let aad = associated(header, vault_id, item_id);
    let nonce = Nonce::try_from(&header[8..HEADER_LEN]).map_err(|_| damaged())?;
    let plain = cipher(key)?.decrypt(&nonce, Payload { msg: body, aad: &aad }).map_err(|_| damaged())?;
    Ok(Zeroizing::new(plain))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> VaultKey {
        VaultKey::from_bytes(&[7u8; 32]).unwrap()
    }

    #[test]
    fn a_sealed_file_opens_with_the_same_key_and_names() {
        let file = seal(&key(), MAGIC_DOC, "vault1", "doc1", b"%PDF-1.7 secret").unwrap();
        assert_eq!(&file[..4], MAGIC_DOC);
        assert!(!file.windows(6).any(|w| w == b"secret"), "the plaintext is not in the file");
        assert_eq!(&*open(&key(), MAGIC_DOC, "vault1", "doc1", &file).unwrap(), b"%PDF-1.7 secret");
    }

    #[test]
    fn every_write_uses_a_new_nonce() {
        let a = seal(&key(), MAGIC_DOC, "v", "d", b"same").unwrap();
        let b = seal(&key(), MAGIC_DOC, "v", "d", b"same").unwrap();
        assert_ne!(a[8..20], b[8..20]);
        assert_ne!(a, b);
    }

    #[test]
    fn a_wrong_key_a_changed_byte_or_a_moved_file_does_not_open() {
        let file = seal(&key(), MAGIC_DOC, "v", "d", b"text").unwrap();
        let other = VaultKey::from_bytes(&[8u8; 32]).unwrap();
        assert!(open(&other, MAGIC_DOC, "v", "d", &file).is_err());
        assert!(open(&key(), MAGIC_DOC, "v", "other-doc", &file).is_err(), "moved to another id");
        assert!(open(&key(), MAGIC_DOC, "other-vault", "d", &file).is_err(), "moved to another vault");
        assert!(open(&key(), MAGIC_META, "v", "d", &file).is_err(), "read as metadata");
        for i in 0..file.len() {
            let mut bad = file.clone();
            bad[i] ^= 1;
            assert!(open(&key(), MAGIC_DOC, "v", "d", &bad).is_err(), "byte {i} changed");
        }
        assert!(open(&key(), MAGIC_DOC, "v", "d", &file[..file.len() - 1]).is_err(), "truncated");
        assert!(open(&key(), MAGIC_DOC, "v", "d", &[]).is_err());
    }

    #[test]
    fn a_newer_version_says_so() {
        let mut file = seal(&key(), MAGIC_DOC, "v", "d", b"text").unwrap();
        file[4] = 9;
        let why = open(&key(), MAGIC_DOC, "v", "d", &file).unwrap_err().to_string();
        assert!(why.contains("newer"), "{why}");
    }

    #[test]
    fn the_derived_key_depends_on_passphrase_salt_and_settings() {
        let p = KdfParams::MIN_STRENGTH;
        let a = derive("correct horse", b"salt-salt-salt-1", p).unwrap();
        assert_eq!(a.expose(), derive("correct horse", b"salt-salt-salt-1", p).unwrap().expose());
        assert_ne!(a.expose(), derive("correct horsf", b"salt-salt-salt-1", p).unwrap().expose());
        assert_ne!(a.expose(), derive("correct horse", b"salt-salt-salt-2", p).unwrap().expose());
        assert_ne!(a.expose(), derive("correct horse", b"salt-salt-salt-1", KdfParams { t: 2, ..p }).unwrap().expose());
    }

    #[test]
    fn settings_from_a_hostile_file_are_bounded() {
        for bad in [
            KdfParams { m_kib: 0, t: 1, p: 1 },
            KdfParams { m_kib: 8, t: 0, p: 1 },
            KdfParams { m_kib: u32::MAX, t: 1, p: 1 },
            KdfParams { m_kib: 64, t: 1_000_000, p: 1 },
            KdfParams { m_kib: 64, t: 1, p: 1_000 },
            KdfParams { m_kib: 8, t: 1, p: 8 },
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
            assert!(derive("x", b"0123456789abcdef", bad).is_err());
        }
        assert!(KdfParams::DEFAULT.check().is_ok() && KdfParams::MIN_STRENGTH.check().is_ok());
    }

    #[test]
    fn a_key_does_not_print() {
        assert_eq!(format!("{:?}", key()), "VaultKey(<hidden>)");
    }

    #[test]
    fn known_answer_for_aes_256_gcm_through_the_same_calls() {
        // NIST SP 800-38D / GCM spec test case 16 style check done through this module's
        // primitives: a zero key and nonce with an empty message give the well-known tag.
        let zero = VaultKey::from_bytes(&[0u8; 32]).unwrap();
        let nonce = Nonce::try_from(&[0u8; 12][..]).unwrap();
        let out = cipher(&zero).unwrap().encrypt(&nonce, Payload { msg: b"", aad: b"" }).unwrap();
        let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "530f8afbc74536b9a963b4f1c4cb738b", "AES-256-GCM, zero key, zero nonce, empty message (McGrew-Viega test case 13)");
    }
}
