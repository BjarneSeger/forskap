use std::path::{Path, PathBuf};

use clap::CommandFactory;
use clap_complete::env::{Bash, EnvCompleter, Fish, Zsh};
use clap_complete::generate_to;

// Share the CLI definition without duplicating it.
mod cli {
    include!("src/cli.rs");
}

fn main() {
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("completions");
    std::fs::create_dir_all(&out_dir).unwrap();

    // bash, zsh and fish complete dynamically: these scripts only make the
    // shell ask `forskap` itself (see `src/complete.rs`), and are what
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

    // carapace has no such hook; its spec stays static.
    let mut cmd = cli::Cli::command();
    generate_to(carapace_spec_clap::Spec, &mut cmd, "forskap", &out_dir).unwrap();
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
