//! API for interacting with forskapd
#![allow(non_camel_case_types)]
// The generated code takes a method's arguments one by one.
#![allow(clippy::too_many_arguments)]

include!(concat!(env!("OUT_DIR"), "/org.thehoster.forskapd.rs"));

/// Raw varlink interface description, suitable for the daemon's
/// `org.varlink.service.GetInterfaceDescription` reply.
pub const VARLINK_INTERFACE_DESCRIPTION: &str =
    include_str!("../varlink/org.thehoster.forskapd.varlink");

const SOCKET_NAME: &str = "forskapd.socket";

/// The socket the daemon listens on unless it is told another one, and where
/// a client finds it: `$XDG_RUNTIME_DIR/forskapd.socket`, or without a
/// runtime directory (macOS has none) `forskapd/forskapd.socket` in the
/// user's data directory. Never a shared directory, where another user could
/// put a socket of their own in its place. `None` without a home directory.
pub fn default_socket() -> Option<std::path::PathBuf> {
    socket_in(dirs::runtime_dir(), dirs::data_local_dir())
}

fn socket_in(
    runtime: Option<std::path::PathBuf>,
    data: Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    runtime
        .map(|dir| dir.join(SOCKET_NAME))
        .or_else(|| data.map(|dir| dir.join("forskapd").join(SOCKET_NAME)))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::socket_in;

    #[test]
    fn the_default_socket_is_in_the_runtime_dir_or_else_the_data_dir() {
        let runtime = || Some(PathBuf::from("/run/user/1000"));
        let data = || Some(PathBuf::from("/home/me/.local/share"));
        assert_eq!(
            socket_in(runtime(), data()),
            Some("/run/user/1000/forskapd.socket".into())
        );
        assert_eq!(
            socket_in(None, data()),
            Some("/home/me/.local/share/forskapd/forskapd.socket".into())
        );
        assert_eq!(socket_in(None, None), None);
    }
}
