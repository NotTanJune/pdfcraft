use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::extract::fixtures::pdf;

const FAST: KdfParams = KdfParams::MIN_STRENGTH;
const PASS: &str = "correct horse battery";

fn folder() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn add(vault: &Vault, name: &str, pages: &[&str]) -> VaultAdded {
    vault.add(&pdf(pages), &VaultAddOptions { name: name.to_string(), ..Default::default() }).unwrap()
}

#[test]
fn a_vault_is_created_unlocked_again_and_refuses_a_wrong_passphrase() {
    let dir = folder();
    assert!(!exists(dir.path()));
    let vault = Vault::create(dir.path(), PASS, FAST).unwrap();
    assert!(exists(dir.path()));
    let id = vault.id().to_string();
    drop(vault);
    assert_eq!(Vault::unlock(dir.path(), PASS).unwrap().id(), id);
    assert!(matches!(Vault::unlock(dir.path(), "wrong passphrase"), Err(LibraryError::WrongPassphrase)));
    assert!(matches!(Vault::create(dir.path(), PASS, FAST), Err(LibraryError::VaultExists)), "a second create never replaces the key");
    assert!(matches!(Vault::unlock(folder().path(), PASS), Err(LibraryError::NoVault)));
}

#[test]
fn a_short_passphrase_is_refused() {
    let dir = folder();
    assert!(matches!(Vault::create(dir.path(), "short", FAST), Err(LibraryError::Invalid(_))));
    assert!(!exists(dir.path()));
}

#[test]
fn the_vault_opens_on_another_mac_from_the_synced_folder_with_the_passphrase() {
    let mac_a = folder();
    let vault = Vault::create(mac_a.path(), PASS, FAST).unwrap();
    let added = add(&vault, "Passport.pdf", &["Passport number 12345"]);
    // The synced folder arrives on another Mac as plain files.
    let mac_b = folder();
    copy_dir(mac_a.path(), mac_b.path());
    let there = Vault::unlock(mac_b.path(), PASS).unwrap();
    let bytes = there.read(&added.entry.id).unwrap();
    assert_eq!(&*bytes, &pdf(&["Passport number 12345"]));
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let target = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), target).unwrap();
        }
    }
}

#[test]
fn nothing_readable_is_on_disk_and_the_document_comes_back_verified() {
    let dir = folder();
    let vault = Vault::create(dir.path(), PASS, FAST).unwrap();
    let source = pdf(&["Top secret merger terms"]);
    let added = vault
        .add(&source, &VaultAddOptions { name: "Merger.pdf".into(), title: Some("Merger".into()), tags: vec!["deal".into()], password: None })
        .unwrap();
    assert!(added.verified);
    for e in std::fs::read_dir(dir.path().join("Vault")).unwrap() {
        let path = e.unwrap().path();
        let raw = std::fs::read(&path).unwrap();
        let shown = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!raw.windows(6).any(|w| w == b"merger" || w == b"Merger" || w == b"secret"), "{shown} holds readable text");
        assert!(!shown.contains("Merger") && !shown.contains("merger"), "the file name tells nothing: {shown}");
        assert!(!raw.starts_with(b"%PDF"), "{shown} is not a PDF in the clear");
        if shown.ends_with(".pcv") {
            assert!(raw.starts_with(b"PCVD") || raw.starts_with(b"PCVM"), "{shown}");
        }
    }
    assert_eq!(&*vault.read(&added.entry.id).unwrap(), &source);
    // The Vault folder and nothing else was written.
    let top: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(top, ["Vault"]);
}

#[test]
fn saving_encrypts_again_with_a_new_nonce_and_keeps_the_old_content_on_failure() {
    let dir = folder();
    let vault = Vault::create(dir.path(), PASS, FAST).unwrap();
    let added = add(&vault, "a.pdf", &["first version"]);
    let before = std::fs::read(vault.doc_path(&added.entry.id)).unwrap();
    let second = pdf(&["second version", "page two"]);
    let saved = vault.replace(&added.entry.id, &second, None).unwrap();
    assert_eq!(saved.entry.pages, 2);
    let after = std::fs::read(vault.doc_path(&added.entry.id)).unwrap();
    assert_ne!(before[8..20], after[8..20], "a new nonce for the new content");
    assert_eq!(&*vault.read(&added.entry.id).unwrap(), &second);
    // Not a PDF: refused, and the stored document is untouched.
    assert!(vault.replace(&added.entry.id, b"not a pdf", None).is_err());
    assert_eq!(&*vault.read(&added.entry.id).unwrap(), &second);
    let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("Vault"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn search_sees_vault_text_titles_and_tags_only_through_the_unlocked_vault() {
    let dir = folder();
    let vault = Vault::create(dir.path(), PASS, FAST).unwrap();
    vault
        .add(&pdf(&["Account number 99871"]), &VaultAddOptions { name: "Bank.pdf".into(), tags: vec!["finance".into()], ..Default::default() })
        .unwrap();
    let snap = vault.snapshot().unwrap();
    let hits = snap.search("99871", &[], 10);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].fields, [Field::Text]);
    assert_eq!(snap.search("bank", &[], 10).len(), 1);
    assert_eq!(snap.search("", &["Finance".to_string()], 10).len(), 1);
    // The metadata file does not hold the text in the clear.
    for e in std::fs::read_dir(dir.path().join("Vault")).unwrap() {
        assert!(!std::fs::read(e.unwrap().path()).unwrap().windows(5).any(|w| w == b"99871"));
    }
}

#[test]
fn removing_deletes_the_encrypted_file_and_the_removal_reaches_the_other_mac() {
    let mac_a = folder();
    let vault = Vault::create(mac_a.path(), PASS, FAST).unwrap();
    let added = add(&vault, "a.pdf", &["gone soon"]);
    let id = added.entry.id.clone();
    vault.remove(&id).unwrap();
    assert!(!vault.doc_path(&id).exists());
    assert!(vault.snapshot().unwrap().entries().is_empty());
    assert!(vault.snapshot().unwrap().entries[&id].deleted);
    assert!(vault.read(&id).is_err());
    assert!(vault.remove(&id).is_err());
}

#[test]
fn the_newer_edit_wins_when_two_macs_changed_the_same_document() {
    let dir = folder();
    let vault = Vault::create(dir.path(), PASS, FAST).unwrap();
    let id = add(&vault, "a.pdf", &["shared"]).entry.id;
    // The other Mac's copy of the metadata, made earlier and retagged there, arrives as a conflict copy.
    let theirs = dir.path().join("Vault").join(format!("{id} 2.meta.pcv"));
    std::fs::copy(vault.meta_path(&id), &theirs).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    vault.retag(&id, &["mine".into()], &[]).unwrap();
    // `retag` folded the copy in and retired it.
    assert!(!theirs.exists());
    assert_eq!(vault.snapshot().unwrap().get(&id).unwrap().tags, ["mine"]);
}

#[test]
fn a_conflict_copy_with_a_newer_edit_wins_over_the_local_one() {
    let dir = folder();
    let vault = Vault::create(dir.path(), PASS, FAST).unwrap();
    let id = add(&vault, "a.pdf", &["shared"]).entry.id;
    let mut theirs = vault.snapshot().unwrap().get(&id).unwrap().clone();
    theirs.title = "Renamed on the other Mac".into();
    theirs.modified_ms += 60_000;
    let json = serde_json::to_vec(&MetaFile { entry: theirs, text: "shared".into() }).unwrap();
    let sealed = crypto::seal(&vault.key, crypto::MAGIC_META, &vault.id, &id, &json).unwrap();
    std::fs::write(dir.path().join("Vault").join(format!("{id} 2.meta.pcv")), sealed).unwrap();
    assert_eq!(vault.snapshot().unwrap().get(&id).unwrap().title, "Renamed on the other Mac");
}

#[test]
fn a_conflict_copy_of_the_document_is_used_when_it_matches_the_newest_metadata() {
    let dir = folder();
    let vault = Vault::create(dir.path(), PASS, FAST).unwrap();
    let id = add(&vault, "a.pdf", &["version one"]).entry.id;
    let stale = std::fs::read(vault.doc_path(&id)).unwrap();
    vault.replace(&id, &pdf(&["version two"]), None).unwrap();
    // The sync service restored the old file under the real name and kept the new one as a copy.
    let current = std::fs::read(vault.doc_path(&id)).unwrap();
    std::fs::write(vault.doc_path(&id), stale).unwrap();
    std::fs::write(dir.path().join("Vault").join(format!("{id} 2.pcv")), current).unwrap();
    assert_eq!(&*vault.read(&id).unwrap(), &pdf(&["version two"]));
}

#[test]
fn a_tampered_or_swapped_file_does_not_open() {
    let dir = folder();
    let vault = Vault::create(dir.path(), PASS, FAST).unwrap();
    let a = add(&vault, "a.pdf", &["alpha"]).entry.id;
    let b = add(&vault, "b.pdf", &["bravo"]).entry.id;
    // Swapping two encrypted files is detected: each is bound to its own id.
    let (fa, fb) = (std::fs::read(vault.doc_path(&a)).unwrap(), std::fs::read(vault.doc_path(&b)).unwrap());
    std::fs::write(vault.doc_path(&a), fb).unwrap();
    std::fs::write(vault.doc_path(&b), fa).unwrap();
    assert!(matches!(vault.read(&a), Err(LibraryError::Damaged(_))));
    // A flipped byte too.
    let mut raw = std::fs::read(vault.doc_path(&b)).unwrap();
    let last = raw.len() - 1;
    raw[last] ^= 1;
    std::fs::write(vault.doc_path(&b), raw).unwrap();
    assert!(vault.read(&b).is_err());
    // A damaged key file is an error, not a panic.
    std::fs::write(dir.path().join("Vault").join(VAULT_FILE), b"{ nope").unwrap();
    assert!(matches!(Vault::unlock(dir.path(), PASS), Err(LibraryError::Damaged(_))));
}

#[test]
fn changed_key_derivation_settings_make_the_passphrase_fail() {
    let dir = folder();
    Vault::create(dir.path(), PASS, FAST).unwrap();
    let path = dir.path().join("Vault").join(VAULT_FILE);
    let mut file: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    file["kdf"]["t"] = 2.into();
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
    assert!(matches!(Vault::unlock(dir.path(), PASS), Err(LibraryError::WrongPassphrase)));
    file["kdf"]["m_kib"] = u32::MAX.into();
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
    assert!(matches!(Vault::unlock(dir.path(), PASS), Err(LibraryError::Damaged(_))), "an absurd memory request is refused before hashing");
}

#[test]
fn a_vault_from_a_newer_version_says_so() {
    let dir = folder();
    Vault::create(dir.path(), PASS, FAST).unwrap();
    let path = dir.path().join("Vault").join(VAULT_FILE);
    let mut file: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    file["format"] = 7.into();
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
    assert!(matches!(Vault::unlock(dir.path(), PASS), Err(LibraryError::TooNew)));
}

#[test]
fn a_vault_still_downloading_is_reported_not_treated_as_missing() {
    let dir = folder();
    std::fs::create_dir_all(dir.path().join("Vault")).unwrap();
    std::fs::write(dir.path().join("Vault").join(".vault.json.icloud"), b"x").unwrap();
    assert!(exists(dir.path()));
    assert!(matches!(Vault::unlock(dir.path(), PASS), Err(LibraryError::NotDownloaded(_))));
    assert!(matches!(Vault::create(dir.path(), PASS, FAST), Err(LibraryError::VaultExists)));
}

#[test]
fn file_names_are_split_into_id_and_kind() {
    assert_eq!(split_name("abc.pcv", ".pcv"), Some(("abc".into(), true)));
    assert_eq!(split_name("abc 2.pcv", ".pcv"), Some(("abc".into(), false)));
    assert_eq!(split_name("abc.meta.pcv", ".meta.pcv"), Some(("abc".into(), true)));
    assert_eq!(split_name("abc 2.meta.pcv", ".meta.pcv"), Some(("abc".into(), false)));
    assert_eq!(split_name("abc.meta.pcv", ".pcv"), None);
    assert_eq!(split_name("abc 2.meta.pcv", ".pcv"), None);
    assert_eq!(split_name(".abc.pcv.tmp", ".pcv"), None);
    assert_eq!(split_name("vault.json", ".pcv"), None);
    assert_eq!(split_name(".pcv", ".pcv"), None);
}

// ---- the session: lock, idle lock, the key cache ------------------------------------------

fn session(dir: &Path, cache: &Arc<MemoryCache>) -> VaultSession {
    VaultSession::new(dir, cache.clone())
}

#[test]
fn a_session_locks_on_demand_and_a_locked_session_gives_no_vault() {
    let dir = folder();
    let cache = Arc::new(MemoryCache::new());
    let mut s = session(dir.path(), &cache);
    let at = Duration::from_secs(1);
    assert!(matches!(s.vault(at), Err(LibraryError::Locked)));
    s.create(PASS, FAST, false, at).unwrap();
    assert!(s.is_unlocked() && s.vault(at).is_ok());
    s.lock();
    assert!(!s.is_unlocked() && matches!(s.vault(at), Err(LibraryError::Locked)));
}

#[test]
fn a_session_locks_after_the_idle_time_and_use_pushes_it_back() {
    let dir = folder();
    let cache = Arc::new(MemoryCache::new());
    let mut s = session(dir.path(), &cache);
    s.set_idle_timeout(Some(Duration::from_secs(900)));
    s.create(PASS, FAST, false, Duration::from_secs(0)).unwrap();
    assert!(!s.expire(Duration::from_secs(899)));
    s.touch(Duration::from_secs(899));
    assert!(!s.expire(Duration::from_secs(1_700)), "use at 899 s moved the deadline to 1799 s");
    assert!(s.expire(Duration::from_secs(1_799)));
    assert!(!s.is_unlocked());
    assert!(matches!(s.vault(Duration::from_secs(1_800)), Err(LibraryError::Locked)));
    // Never locking on idle is a choice.
    s.unlock_with_passphrase(PASS, false, Duration::from_secs(2_000)).unwrap();
    s.set_idle_timeout(None);
    assert!(!s.expire(Duration::from_secs(10_000_000)) && s.is_unlocked());
}

#[test]
fn after_one_passphrase_unlock_the_cached_key_unlocks_without_the_passphrase() {
    let dir = folder();
    let cache = Arc::new(MemoryCache::new());
    let mut s = session(dir.path(), &cache);
    s.create(PASS, FAST, false, Duration::ZERO).unwrap();
    s.lock();
    assert!(!s.status().cached);
    assert!(s.unlock_with_cache(Duration::ZERO).is_err(), "nothing remembered yet");
    // The first unlock on this Mac, asked to remember.
    assert!(s.unlock_with_passphrase(PASS, true, Duration::ZERO).unwrap().is_none());
    assert!(s.status().cached);
    s.lock();
    assert_eq!(cache.loads(), 1, "the failed attempt above asked once");
    s.unlock_with_cache(Duration::ZERO).unwrap();
    assert!(s.is_unlocked());
    s.lock();
    s.forget_cached_key().unwrap();
    assert!(!s.status().cached && s.unlock_with_cache(Duration::ZERO).is_err());
    assert!(s.unlock_with_passphrase("wrong passphrase", true, Duration::ZERO).is_err());
    assert!(!s.status().cached, "a wrong passphrase remembers nothing");
}

#[test]
fn a_stale_cached_key_is_dropped_and_the_passphrase_still_works() {
    let dir = folder();
    let cache = Arc::new(MemoryCache::new());
    let mut s = session(dir.path(), &cache);
    s.create(PASS, FAST, true, Duration::ZERO).unwrap();
    s.lock();
    // The vault is recreated elsewhere: the key this Mac remembers is the old one.
    let id = Vault::id_of(dir.path()).unwrap();
    cache.store(&id, &VaultKey::from_bytes(&[1u8; 32]).unwrap()).unwrap();
    let why = s.unlock_with_cache(Duration::ZERO).unwrap_err().to_string();
    assert!(why.contains("passphrase"), "{why}");
    assert!(!s.status().cached);
    s.unlock_with_passphrase(PASS, false, Duration::ZERO).unwrap();
}

#[test]
fn a_failing_cache_does_not_stop_the_unlock_and_says_why() {
    let dir = folder();
    let mut s = VaultSession::new(dir.path(), Arc::new(NoCache));
    let warn = s.create(PASS, FAST, true, Duration::ZERO).unwrap();
    assert!(matches!(warn, Some(KeyCacheError::Unavailable(_))));
    assert!(s.is_unlocked());
    s.lock();
    assert!(!s.status().cache_supported);
    assert!(s.unlock_with_passphrase(PASS, true, Duration::ZERO).is_ok());
}

#[test]
fn changing_the_library_folder_locks_the_vault() {
    let (a, b) = (folder(), folder());
    let cache = Arc::new(MemoryCache::new());
    let mut s = session(a.path(), &cache);
    s.create(PASS, FAST, false, Duration::ZERO).unwrap();
    s.set_root(b.path());
    assert!(!s.is_unlocked());
}

#[test]
fn documents_added_before_a_lock_are_there_after_the_next_unlock() {
    let dir = folder();
    let cache = Arc::new(MemoryCache::new());
    let mut s = session(dir.path(), &cache);
    s.create(PASS, FAST, false, Duration::ZERO).unwrap();
    let id = s.vault(Duration::ZERO).unwrap().add(&pdf(&["kept"]), &VaultAddOptions { name: "k.pdf".into(), ..Default::default() }).unwrap().entry.id;
    s.lock();
    s.unlock_with_passphrase(PASS, false, Duration::ZERO).unwrap();
    assert_eq!(&*s.vault(Duration::ZERO).unwrap().read(&id).unwrap(), &pdf(&["kept"]));
}
