use std::path::{Path, PathBuf};

use clap_complete::env::{Bash, EnvCompleter, Fish, Zsh};

/// Makes carapace ask `forskap` as well: its `jj` bridge speaks the protocol
/// of clap's dynamic completion, which its `clap` bridge doesn't.
const CARAPACE_SPEC: &str = "\
name: forskap
description: Cached GitLab CLI
parsing: disabled
completion:
  positionalany: [\"$carapace.bridge.JJ([forskap])\"]
";

fn main() {
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("completions");
    std::fs::create_dir_all(&out_dir).unwrap();

    // Every shell completes dynamically: these scripts only make it ask
    // `forskap` itself (see `src/complete.rs`), and are what
    // `COMPLETE=<shell> forskap` prints. They go with the binary of the same
    // build, as clap_complete promises nothing across versions.
    registration(&Bash, &out_dir.join("forskap.bash"), "");
    registration(&Fish, &out_dir.join("forskap.fish"), "");
    // Installed as an autoloaded `_forskap`, the script is the body of that
    // function and first runs on the first Tab: complete that one as well.
    registration(
        &Zsh,
        &out_dir.join("_forskap"),
        "if [ \"$funcstack[1]\" = \"_forskap\" ]; then\n    \
         _clap_dynamic_completer_forskap \"$@\"\nfi\n",
    );
    // Nushell's completer is the crate's own (`src/complete/nushell.rs`).
    let nushell = include_str!("src/complete/nushell.nu").replace("{completer}", "forskap");
    std::fs::write(out_dir.join("forskap.nu"), nushell).unwrap();

    std::fs::write(out_dir.join("forskap.yaml"), CARAPACE_SPEC).unwrap();
}

/// Write the script registering `forskap` (from `$PATH`) as its own completer.
fn registration(shell: &dyn EnvCompleter, path: &Path, epilogue: &str) {
    let mut script = Vec::new();
    shell
        .write_registration("COMPLETE", "forskap", "forskap", "forskap", &mut script)
        .unwrap();
    script.extend_from_slice(epilogue.as_bytes());
    std::fs::write(path, script).unwrap();
}
