//! Print the annotated default `forskapd` config to stdout.
//!
//! Packaging tool, not shipped to `/usr/bin`. The output is installed as the
//! package-provided default at `/usr/share/forskapd/config.toml`:
//!
//! ```sh
//! cargo run -p forskapd --bin gen-config-template > packaging/config.toml
//! ```

fn main() {
    print!("{}", forskapd::config::template());
}
