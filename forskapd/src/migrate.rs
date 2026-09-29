//! One-time moves out of the locations used while the daemon was called
//! `gitlab-trackrd`. Best-effort: a failed move is logged and the daemon
//! starts on the new, empty location.

use std::path::Path;

use tracing::{info, warn};

const LEGACY_DIR: &str = "gitlab-trackrd";
const DIR: &str = "forskapd";

/// Move the config and data directories. Must run before either is read.
pub fn run() {
    for base in [dirs::config_dir(), dirs::data_local_dir()]
        .into_iter()
        .flatten()
    {
        move_dir(&base.join(LEGACY_DIR), &base.join(DIR));
    }
}

/// Rename `old` to `new` unless `new` already exists; never merges.
fn move_dir(old: &Path, new: &Path) {
    if new.exists() || !old.exists() {
        return;
    }
    match std::fs::rename(old, new) {
        Ok(()) => info!(from = %old.display(), to = %new.display(), "moved legacy directory"),
        Err(e) => warn!(error = %e, from = %old.display(), "moving legacy directory failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_the_old_directory_when_the_new_one_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let (old, new) = (tmp.path().join("old"), tmp.path().join("new"));
        std::fs::create_dir_all(old.join("db")).unwrap();
        std::fs::write(old.join("db/data"), "x").unwrap();

        move_dir(&old, &new);

        assert!(!old.exists());
        assert_eq!(std::fs::read_to_string(new.join("db/data")).unwrap(), "x");
    }

    #[test]
    fn leaves_both_alone_when_the_new_directory_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let (old, new) = (tmp.path().join("old"), tmp.path().join("new"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("data"), "old").unwrap();

        move_dir(&old, &new);

        assert!(old.join("data").exists());
        assert!(!new.join("data").exists());
    }

    #[test]
    fn does_nothing_without_an_old_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let (old, new) = (tmp.path().join("old"), tmp.path().join("new"));

        move_dir(&old, &new);

        assert!(!new.exists());
    }
}
