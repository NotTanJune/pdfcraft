# pdfcraft-library

The document library and the encrypted vault (layer 4). No egui, no sockets, no `unsafe`.

## Library

An index of documents kept as JSON in a library folder, with the text of each document in a second
file for full-text search.

| File | Holds |
| --- | --- |
| `library.json` | one entry per document: id, title, pages, tags, added / opened / modified times, size, SHA-256, location |
| `library-text.json` | the extracted text per document id, at most 200 KB each |
| `Documents/` | documents the library copied in |
| `Vault/` | the encrypted vault (below) |

The default folder is iCloud Drive's `PdfCraft/` (`~/Library/Mobile Documents/com~apple~CloudDocs/PdfCraft/`)
when iCloud Drive exists, else `PdfCraft` in the app's data folder; the person can choose another.

- **Locations.** Inside the folder: a relative path, so it resolves on every Mac. Elsewhere: an
  absolute path, marked missing on a Mac where the file is not. An index from another Mac is
  untrusted: ids, relative paths and counts are checked, and an entry that fails is skipped.
- **Writes** are temp file, flush, rename. A damaged `library.json` is set aside, never overwritten;
  an index the sync service has not finished downloading (`.library.json.icloud`) is an error, never
  replaced by an empty one.
- **Merging** (`index.rs`) is a join, per entry: the newest `modified_ms` wins (a tie goes to the
  removal, then to the greater hash, so every Mac picks the same winner), `opened_ms` is the later,
  `added_ms` the earlier. Opening a document changes only `opened_ms`. A removal is a tombstone kept
  for 90 days. Conflict copies (`library 2.json`, `library (conflicted copy ...).json`,
  `library.sync-conflict-....json`) are read and merged, then removed once the merged file is written.
  There is no lock between the read and the write: two changes in the same instant on one Mac can
  lose one, and a second try repairs it.
- **Text** comes from the page text layer `pdfcraft-render` already extracts, so recognised text from
  Scan & OCR is searchable. At most 5000 pages are read and 200 KB kept per document.

## Vault

Encrypted documents in `Vault/` of the library folder, so they sync.

- `vault.json`: Argon2id settings (default 64 MiB, 3 passes, 1 lane), a 16-byte salt, the vault key
  wrapped with AES-256-GCM by the passphrase's key, and a check blob under the vault key.
- `<id>.pcv`: one document. `<id>.meta.pcv`: its title, tags, page count, hash and text, encrypted.
  Ids are random, so file names say nothing.
- File format: `PCVD` or `PCVM`, version 1, cipher 1 (AES-256-GCM), two reserved zero bytes, a random
  96-bit nonce (new on every write), ciphertext, 16-byte tag. The authenticated data is the first 8
  header bytes, the vault id and the item id, so a file moved to another name or vault fails to open.
- The vault key is 256 random bits and exists only wrapped, or in memory while unlocked.
- On macOS, after a passphrase unlock, the key can be cached in the data protection keychain behind
  user presence (Touch ID or the login password), this device only, not synchronizable. That needs a
  signed build with a keychain access group; an unsigned build is refused by the system
  (`errSecMissingEntitlement`), which the app reports and then asks for the passphrase every time.
  Other systems have no cache.
- No plaintext is written by this crate. A document is decrypted into memory and saved by encrypting
  it again (new nonce, atomic replace, read back and compared).
- Locking drops the key from the session (wiped on drop). Idle time is measured by the caller, so the
  crate reads no clock.

### Threat model

Protects against: someone with the synced folder (iCloud, a backup, a stolen disk) and no passphrase
and no unlocked Mac; tampering with, swapping or truncating files (detected, not repaired).

Does not protect against: a weak passphrase (Argon2id slows guessing, it does not stop it); malware
or another process running as the user while the vault is unlocked (the key and the decrypted
documents are in memory; the process is not hardened against debuggers); a decrypted document you
save elsewhere on purpose (Save As, Export, Print and Share write what you ask for); the document
sizes and the number of documents (file sizes and counts are visible); the time files were written.
Decrypted bytes in the app's memory are wiped for the key and the buffer handed out by `Vault::read`,
not for the copy the viewer keeps while the document is open. A forgotten passphrase cannot be
recovered. Changing the passphrase is not offered yet.

## Dependencies added

| Crate | Version | Licence | Used for |
| --- | --- | --- | --- |
| `aes-gcm` | 0.11.1 | Apache-2.0 OR MIT (RustCrypto) | AES-256-GCM |
| `argon2` | 0.6.0 | MIT OR Apache-2.0 (RustCrypto) | Argon2id |
| `security-framework` (macOS only, `OSX_10_15`) | 3.7 | MIT OR Apache-2.0 | the keychain, `SecAccessControl` |
| `sha2` | 0.11 | MIT OR Apache-2.0 | content hashes |
| `base64` | 0.23 | MIT OR Apache-2.0 | binary fields in `vault.json` |
| `getrandom` | 0.4 | MIT OR Apache-2.0 | nonces, salts, keys |
