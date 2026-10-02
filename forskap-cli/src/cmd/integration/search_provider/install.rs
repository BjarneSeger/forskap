//! `forskap integration search-provider install` — write the registration files with this
//! binary's path filled in. The same files ship verbatim in the package
//! (`forskap-cli/packaging/`, wired up in `.goreleaser.yaml`) for `/usr/bin/forskap`.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config;

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
    (
        "icons/hicolor/scalable/apps/org.thehoster.forskap.svg",
        include_str!("../../../../packaging/icons/hicolor/scalable/apps/org.thehoster.forskap.svg"),
    ),
    (
        "icons/hicolor/symbolic/apps/org.thehoster.forskap-symbolic.svg",
        include_str!(
            "../../../../packaging/icons/hicolor/symbolic/apps/org.thehoster.forskap-symbolic.svg"
        ),
    ),
];

/// Registrations written before the rename; they would answer alongside ours.
const LEGACY_FILES: &[&str] = &[
    "gnome-shell/search-providers/org.thehoster.gitlab.trackr.search-provider.ini",
    "applications/org.thehoster.gitlab.trackr.desktop",
    "dbus-1/services/org.thehoster.gitlab.trackr.SearchProvider.service",
    "krunner/dbusplugins/org.thehoster.gitlab.trackr.desktop",
];

/// pop-launcher, the backend of COSMIC's launcher, reads plugins from
/// `~/.local/share/pop-launcher/plugins`, this directory and a distribution
/// directory that differs per distro (`/usr/lib` upstream, `/usr/libexec` on
/// Fedora) — never from `$XDG_DATA_DIRS`.
const POP_LAUNCHER_SYSTEM_PLUGINS: &str = "/etc/pop-launcher/plugins";
/// The plugin's directory under those; the first `plugin.ron` of a `name` wins,
/// so a user install shadows the packaged one.
const POP_LAUNCHER_PLUGIN: &str = "forskap";
/// pop-launcher runs a plugin's executable without arguments, hence a script
/// calling `forskap integration search-provider cosmic`.
const POP_LAUNCHER_SCRIPT: &str = "forskap-cosmic-launcher";
const POP_LAUNCHER_SCRIPT_BODY: &str =
    include_str!("../../../../packaging/pop-launcher/plugins/forskap/forskap-cosmic-launcher");

pub fn run(prefix: Option<PathBuf>) -> Result<()> {
    let prefix = prefix.unwrap_or_else(|| PathBuf::from(DEFAULT_PREFIX));
    let exe = std::env::current_exe().context("resolving the path of this forskap binary")?;
    let exe = exe
        .to_str()
        .with_context(|| format!("forskap binary path {} is not valid UTF-8", exe.display()))?;
    let trigger_word = super::trigger_word(&config::load()?);

    for (rel, body) in FILES {
        let dst = prefix.join(rel);
        let dir = dst.parent().expect("packaged paths have a parent");
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        fs::write(&dst, body.replace(PACKAGED_BIN, exe))
            .with_context(|| format!("writing {}", dst.display()))?;
        outln!("wrote {}", dst.display())?;
    }

    let plugin_dir = pop_launcher_plugin_dir(&prefix, dirs::home_dir().as_deref());
    fs::create_dir_all(&plugin_dir)
        .with_context(|| format!("creating {}", plugin_dir.display()))?;
    let ron = plugin_dir.join("plugin.ron");
    fs::write(&ron, plugin_ron(trigger_word.as_deref()))
        .with_context(|| format!("writing {}", ron.display()))?;
    outln!("wrote {}", ron.display())?;
    let script = plugin_dir.join(POP_LAUNCHER_SCRIPT);
    fs::write(&script, POP_LAUNCHER_SCRIPT_BODY.replace(PACKAGED_BIN, exe))
        .with_context(|| format!("writing {}", script.display()))?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("making {} executable", script.display()))?;
    outln!("wrote {}", script.display())?;

    for rel in LEGACY_FILES {
        let old = prefix.join(rel);
        match fs::remove_file(&old) {
            Ok(()) => outln!("removed {}", old.display())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", old.display())),
        }
    }

    let data_dirs = xdg_data_dirs();
    if !data_dirs.iter().any(|d| same_dir(d, &prefix)) {
        outln!(
            "note: GNOME Shell loads search providers only from $XDG_DATA_DIRS ({}), \
             not from {}; KRunner and D-Bus activation work from there.",
            data_dirs
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(":"),
            prefix.display()
        )?;
    }
    outln!(
        "Reload the bus with `busctl --user call org.freedesktop.DBus /org/freedesktop/DBus \
         org.freedesktop.DBus ReloadConfig`; GNOME Shell picks the provider up at the next \
         login, KRunner after `kquitapp6 krunner`, COSMIC's launcher after \
         `pkill cosmic-launcher` (its session restarts it)."
    )?;
    Ok(())
}

/// The launcher's user directory when `prefix` is the user's `~/.local/share`
/// (pop-launcher hardcodes `~`, not `$XDG_DATA_HOME`), else its system one:
/// `<prefix>/pop-launcher/plugins` would be searched nowhere.
fn pop_launcher_plugin_dir(prefix: &Path, home: Option<&Path>) -> PathBuf {
    match home {
        Some(home) if same_dir(&home.join(".local/share"), prefix) => {
            prefix.join("pop-launcher/plugins")
        }
        _ => PathBuf::from(POP_LAUNCHER_SYSTEM_PLUGINS),
    }
    .join(POP_LAUNCHER_PLUGIN)
}

/// The plugin's `plugin.ron`. A trigger word becomes a prefix only this plugin
/// answers (`isolate`), listed by the launcher's `?` help; the provider still
/// strips it itself.
fn plugin_ron(trigger_word: Option<&str>) -> String {
    let query = trigger_word.map_or_else(String::new, |word| {
        format!(
            "    query: (regex: \"^{}( |$)\", help: \"{} \", isolate: true),\n",
            ron_escape(&regex_literal(word)),
            ron_escape(word)
        )
    });
    [
        "(\n",
        "    name: \"GitLab\",\n",
        "    description: \"Syntax: [issue|mr|epic|project|group] <text>, #42, !42, &42\\nExample: mr oauth\",\n",
        &format!("    bin: (path: \"{POP_LAUNCHER_SCRIPT}\"),\n"),
        "    icon: Name(\"org.thehoster.forskap\"),\n",
        &query,
        ")\n",
    ]
    .concat()
}

/// `word` as a regex matching only itself; the `regex` crate pop-launcher
/// uses takes any ASCII punctuation escaped.
fn regex_literal(word: &str) -> String {
    let mut out = String::with_capacity(word.len() * 2);
    for c in word.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `s` inside a RON string literal.
fn ron_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The package ships the no-trigger variant as a file.
    #[test]
    fn packaged_plugin_ron_is_the_generated_one() {
        assert_eq!(
            plugin_ron(None),
            include_str!("../../../../packaging/pop-launcher/plugins/forskap/plugin.ron")
        );
    }

    #[test]
    fn trigger_word_isolates_the_plugin() {
        let ron = plugin_ron(Some("gl"));
        assert!(
            ron.contains("    query: (regex: \"^gl( |$)\", help: \"gl \", isolate: true),\n"),
            "{ron}"
        );
        let ron = plugin_ron(Some("gl+"));
        assert!(
            ron.contains("regex: \"^gl\\\\+( |$)\", help: \"gl+ \""),
            "{ron}"
        );
    }

    #[test]
    fn plugin_dir_follows_the_prefix() {
        let home = Some(Path::new("/nonexistent/me"));
        let user = PathBuf::from("/nonexistent/me/.local/share/pop-launcher/plugins/forskap");
        assert_eq!(
            pop_launcher_plugin_dir(Path::new("/nonexistent/me/.local/share"), home),
            user
        );
        assert_eq!(
            pop_launcher_plugin_dir(Path::new("/nonexistent/me/.local/share/"), home),
            user
        );
        let system = PathBuf::from("/etc/pop-launcher/plugins/forskap");
        assert_eq!(
            pop_launcher_plugin_dir(Path::new("/usr/local/share"), home),
            system
        );
        assert_eq!(
            pop_launcher_plugin_dir(Path::new("/nonexistent/me/.local/share"), None),
            system
        );
    }
}
