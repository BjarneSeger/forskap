//! `forskap integration search-provider install` — write the registration files with this
//! binary's path filled in. The same files ship verbatim in the package
//! (`forskap-cli/packaging/`, wired up in `.goreleaser.yaml`) for `/usr/bin/forskap`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Where the package installs to; gnome-shell reads providers only from
/// `$XDG_DATA_DIRS`, whose default is `/usr/local/share:/usr/share`.
const DEFAULT_PREFIX: &str = "/usr/local/share";
const PACKAGED_BIN: &str = "/usr/bin/forskap";

const FILES: &[(&str, &str)] = &[
    (
        "gnome-shell/search-providers/org.thehoster.forskap.search-provider.ini",
        include_str!(
            "../../../../packaging/gnome-shell/search-providers/org.thehoster.forskap.search-provider.ini"
        ),
    ),
    (
        "applications/org.thehoster.forskap.desktop",
        include_str!("../../../../packaging/applications/org.thehoster.forskap.desktop"),
    ),
    (
        "dbus-1/services/org.thehoster.forskap.SearchProvider.service",
        include_str!(
            "../../../../packaging/dbus-1/services/org.thehoster.forskap.SearchProvider.service"
        ),
    ),
    (
        "krunner/dbusplugins/org.thehoster.forskap.desktop",
        include_str!("../../../../packaging/krunner/dbusplugins/org.thehoster.forskap.desktop"),
    ),
];

/// Registrations written before the rename; they would answer alongside ours.
const LEGACY_FILES: &[&str] = &[
    "gnome-shell/search-providers/org.thehoster.gitlab.trackr.search-provider.ini",
    "applications/org.thehoster.gitlab.trackr.desktop",
    "dbus-1/services/org.thehoster.gitlab.trackr.SearchProvider.service",
    "krunner/dbusplugins/org.thehoster.gitlab.trackr.desktop",
];

pub fn run(prefix: Option<PathBuf>) -> Result<()> {
    let prefix = prefix.unwrap_or_else(|| PathBuf::from(DEFAULT_PREFIX));
    let exe = std::env::current_exe().context("resolving the path of this forskap binary")?;
    let exe = exe
        .to_str()
        .with_context(|| format!("forskap binary path {} is not valid UTF-8", exe.display()))?;

    for (rel, body) in FILES {
        let dst = prefix.join(rel);
        let dir = dst.parent().expect("packaged paths have a parent");
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        fs::write(&dst, body.replace(PACKAGED_BIN, exe))
            .with_context(|| format!("writing {}", dst.display()))?;
        println!("wrote {}", dst.display());
    }

    for rel in LEGACY_FILES {
        let old = prefix.join(rel);
        match fs::remove_file(&old) {
            Ok(()) => println!("removed {}", old.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", old.display())),
        }
    }

    let data_dirs = xdg_data_dirs();
    if !data_dirs.iter().any(|d| same_dir(d, &prefix)) {
        println!(
            "note: GNOME Shell loads search providers only from $XDG_DATA_DIRS ({}), \
             not from {}; KRunner and D-Bus activation work from there.",
            data_dirs
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(":"),
            prefix.display()
        );
    }
    println!(
        "Reload the bus with `busctl --user call org.freedesktop.DBus /org/freedesktop/DBus \
         org.freedesktop.DBus ReloadConfig`; GNOME Shell picks the provider up at the next \
         login, KRunner after `kquitapp6 krunner`."
    );
    Ok(())
}

fn xdg_data_dirs() -> Vec<PathBuf> {
    std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string())
        .split(':')
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Compare with trailing slashes and symlinks resolved where possible.
fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        p.canonicalize()
            .unwrap_or_else(|_| p.components().collect())
    };
    norm(a) == norm(b)
}
