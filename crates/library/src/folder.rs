//! Where the library folder is by default.
//!
//! macOS: `~/Library/Mobile Documents/com~apple~CloudDocs/PdfCraft/` when iCloud Drive exists
//! (its `com~apple~CloudDocs` folder is there), else `PdfCraft` in the app's data folder. Other
//! systems: `PdfCraft` in the user's data folder. The person can choose another folder.

use std::path::{Path, PathBuf};

/// The library folder's name inside the sync or data folder.
pub const FOLDER_NAME: &str = "PdfCraft";

/// The default library folder for a user whose home folder is `home`, and whether it is in
/// iCloud Drive. `data_dir` is where the app keeps its data when iCloud Drive is absent.
pub fn default_for(home: &Path, data_dir: Option<&Path>) -> (PathBuf, bool) {
    let icloud = home.join("Library").join("Mobile Documents").join("com~apple~CloudDocs");
    if cfg!(target_os = "macos") && icloud.is_dir() {
        return (icloud.join(FOLDER_NAME), true);
    }
    let fallback = match data_dir {
        Some(dir) => dir.join(FOLDER_NAME),
        None => home.join(".pdfcraft").join(FOLDER_NAME),
    };
    (fallback, false)
}

/// The default library folder for this user, from `HOME` (or `USERPROFILE`) and the platform's
/// data folder. `None` when there is no home folder to start from.
pub fn default_library_dir() -> Option<(PathBuf, bool)> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).filter(|h| h.is_absolute())?;
    let data = if cfg!(target_os = "macos") {
        Some(home.join("Library").join("Application Support"))
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| Some(home.join(".local").join("share")))
    };
    Some(default_for(&home, data.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_icloud_the_data_folder_is_used() {
        let home = tempfile::tempdir().unwrap();
        let data = home.path().join("data");
        let (dir, icloud) = default_for(home.path(), Some(&data));
        assert_eq!(dir, data.join("PdfCraft"));
        assert!(!icloud);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn with_icloud_drive_the_library_goes_in_it() {
        let home = tempfile::tempdir().unwrap();
        let cloud = home.path().join("Library/Mobile Documents/com~apple~CloudDocs");
        std::fs::create_dir_all(&cloud).unwrap();
        let (dir, icloud) = default_for(home.path(), Some(&home.path().join("data")));
        assert_eq!(dir, cloud.join("PdfCraft"));
        assert!(icloud);
    }
}
