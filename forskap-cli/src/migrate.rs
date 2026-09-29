//! One-time moves of the config and state files written while the CLI was
//! called `tt`. Best-effort and silent: `forskap tick` runs on every prompt.

use std::path::{Path, PathBuf};

use crate::{config, state};

pub fn run() {
    if let Some(dir) = dirs::config_dir() {
        move_file(
            &dir.join("gitlab-trackr-cli/config.toml"),
            &config::config_path(),
        );
    }
    if let Some(dir) = dirs::state_dir().or_else(dirs::data_local_dir) {
        move_file(&dir.join("gitlab_trackr/state.json"), &state::state_path());
    }
}

/// Rename `old` to `new` unless `new` already exists, then drop the emptied
/// old directory.
fn move_file(old: &Path, new: &Path) {
    if new.exists() || !old.exists() {
        return;
    }
    let Some(parent) = new.parent() else { return };
    if std::fs::create_dir_all(parent).is_ok() && std::fs::rename(old, new).is_ok() {
        let _ = old.parent().map(PathBuf::from).map(std::fs::remove_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("forskap-migrate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn moves_the_old_file_and_removes_its_directory() {
        let tmp = scratch("move");
        let (old, new) = (tmp.join("old/state.json"), tmp.join("new/state.json"));
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, "x").unwrap();

        move_file(&old, &new);

        assert_eq!(std::fs::read_to_string(&new).unwrap(), "x");
        assert!(!tmp.join("old").exists());
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn keeps_an_existing_new_file() {
        let tmp = scratch("keep");
        let (old, new) = (tmp.join("old/state.json"), tmp.join("new/state.json"));
        for (path, body) in [(&old, "old"), (&new, "new")] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }

        move_file(&old, &new);

        assert_eq!(std::fs::read_to_string(&new).unwrap(), "new");
        assert!(old.exists());
        std::fs::remove_dir_all(tmp).unwrap();
    }
}
