//! [`Drive`] seen through the shared `craft_cloud` contract.
//!
//! `craft_cloud::Files` and `craft_cloud::Accounts` fit a drive's basics: a folder's contents, one
//! file's bytes, a conditional write, create, stat, and who is signed in. [`DriveFiles`] implements
//! them over any [`Drive`], so code written against the shared contract can use Google Drive or
//! OneDrive. The rest of `craft_cloud::Provider` (sharing, contacts, notifications, mail,
//! conversations) is not offered by this crate, so `Provider` itself is not implemented.

use craft_cloud::{Accounts, Entry, EntryKind, Error, Files, Identity, Result};

use crate::drive::{Drive, MAX_DOWNLOAD_BYTES, list_all};
use crate::types::{CancelToken, Item, Query};

/// Adapts a [`Drive`] to `craft_cloud::Files` and `craft_cloud::Accounts`.
pub struct DriveFiles<'a> {
    drive: &'a dyn Drive,
}

impl<'a> DriveFiles<'a> {
    pub fn new(drive: &'a dyn Drive) -> Self {
        DriveFiles { drive }
    }
}

fn entry(item: Item) -> Entry {
    Entry {
        room: item.id.clone(),
        id: item.id,
        path: item.name.clone(),
        name: item.name,
        kind: if item.is_folder { EntryKind::Folder } else { EntryKind::File },
        size: item.size,
        modified: item.modified,
        mime: item.mime,
        etag: item.version,
        writable: item.can_write,
        shared_by: None,
    }
}

impl Files for DriveFiles<'_> {
    /// `path` is a folder's id; empty is the drive's root. At most 20 pages are read.
    fn list(&self, path: &str) -> Result<Vec<Entry>> {
        let query = Query { folder: Some(path.to_string()).filter(|p| !p.is_empty()), ..Query::default() };
        let (items, _more) = list_all(self.drive, &query)?;
        Ok(items.into_iter().map(entry).collect())
    }

    /// Items other people shared are not listed by this crate yet: always empty.
    fn shared_with_me(&self) -> Result<Vec<Entry>> {
        Ok(Vec::new())
    }

    fn read(&self, id: &str) -> Result<Vec<u8>> {
        Ok(self.drive.download(id, MAX_DOWNLOAD_BYTES, &CancelToken::new(), &mut |_| {})?.bytes)
    }

    fn write(&self, id: &str, bytes: &[u8], if_match: Option<&str>) -> Result<Entry> {
        Ok(entry(self.drive.update(id, bytes, if_match, &CancelToken::new(), &mut |_| {})?))
    }

    fn create(&self, folder: &str, name: &str, bytes: &[u8]) -> Result<Entry> {
        let folder = Some(folder).filter(|f| !f.is_empty());
        Ok(entry(self.drive.upload_new(folder, name, bytes, &CancelToken::new(), &mut |_| {})?))
    }

    fn stat(&self, id: &str) -> Result<Entry> {
        Ok(entry(self.drive.stat(id)?))
    }
}

impl Accounts for DriveFiles<'_> {
    fn me(&self) -> Result<Identity> {
        let account = self.drive.account().map_err(Error::from)?;
        Ok(Identity {
            id: account.email.clone().unwrap_or_else(|| account.provider.id().to_string()),
            display: account.display(),
            email: account.email,
            avatar: None,
        })
    }
}
