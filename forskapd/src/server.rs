//! Socket-activated accept loop and varlink connection driver.

use std::io::ErrorKind;
use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use varlink::AsyncConnectionHandler;

use crate::error::Result;

/// The [`UnixListener`] systemd passed as FD 3 (socket activation). Only
/// for a process [`is_socket_activated`].
pub fn inherited_listener() -> Result<UnixListener> {
    // SAFETY: systemd guarantees FD 3 is a valid, bound, listening Unix socket.
    let std_listener = unsafe {
        use std::os::unix::io::FromRawFd;
        std::os::unix::net::UnixListener::from_raw_fd(3)
    };
    std_listener.set_nonblocking(true)?;
    Ok(UnixListener::from_std(std_listener)?)
}

/// A new socket bound at `socket_path`, for its owner alone whatever the
/// umask. A socket a killed daemon left there is replaced: the default one's
/// directory outlives a reboot. One that is listened on, or any other file,
/// is an error.
pub fn bind(socket_path: &str) -> Result<UnixListener> {
    let named = |e: std::io::Error| std::io::Error::new(e.kind(), format!("{socket_path}: {e}"));
    let listener = match UnixListener::bind(socket_path) {
        Err(e) if e.kind() == ErrorKind::AddrInUse && is_stale(socket_path) => {
            std::fs::remove_file(socket_path).map_err(named)?;
            UnixListener::bind(socket_path)
        }
        bound => bound,
    }
    .map_err(named)?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600)).map_err(named)?;
    Ok(listener)
}

/// Whether `path` is a socket nobody listens on.
fn is_stale(path: &str) -> bool {
    let is_socket = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket());
    is_socket
        && std::os::unix::net::UnixStream::connect(path)
            .is_err_and(|e| e.kind() == ErrorKind::ConnectionRefused)
}

/// Returns `true` when the process was socket-activated by systemd.
pub fn is_socket_activated() -> bool {
    std::env::var("LISTEN_FDS").as_deref() == Ok("1")
}

/// Accept loop — runs until the caller stops polling it (see
/// [`crate::daemon::Daemon::serve_until`]).
pub async fn serve<H: AsyncConnectionHandler + 'static>(
    handler: Arc<H>,
    listener: UnixListener,
) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, handler).await {
                match e.kind() {
                    varlink::ErrorKind::ConnectionClosed => {}
                    _ => tracing::warn!("connection error: {e:?}"),
                }
            }
        });
    }
}

/// Drive a single varlink connection to completion using the sans-IO state machine.
async fn handle_connection<H: AsyncConnectionHandler>(
    mut stream: UnixStream,
    handler: Arc<H>,
) -> varlink::Result<()> {
    let mut server = varlink::sansio::Server::new();
    let mut buf = vec![0u8; 8192];
    let mut upgraded_iface: Option<String> = None;

    loop {
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|_| varlink::Error(varlink::ErrorKind::ConnectionClosed, None, None))?;

        if n == 0 {
            return Ok(());
        }

        server.handle_input(&buf[..n])?;
        upgraded_iface = handler.handle(&mut server, upgraded_iface.clone()).await?;

        while let Some(transmit) = server.poll_transmit() {
            stream
                .write_all(&transmit.payload)
                .await
                .map_err(|_| varlink::Error(varlink::ErrorKind::ConnectionClosed, None, None))?;
            stream
                .flush()
                .await
                .map_err(|_| varlink::Error(varlink::ErrorKind::ConnectionClosed, None, None))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::error::Error;

    fn socket_in(dir: &tempfile::TempDir) -> String {
        dir.path().join("s.socket").to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn a_bound_socket_is_its_owners_alone() {
        let dir = tempfile::tempdir().unwrap();
        let socket = socket_in(&dir);
        let _listener = bind(&socket).unwrap();
        let mode = std::fs::metadata(&socket).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "whatever the umask");
    }

    #[tokio::test]
    async fn a_socket_nobody_listens_on_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let socket = socket_in(&dir);
        // What a killed daemon leaves: the file, without a listener.
        drop(bind(&socket).unwrap());
        assert!(Path::new(&socket).exists());

        let _listener = bind(&socket).unwrap();
        UnixStream::connect(&socket).await.unwrap();
    }

    #[tokio::test]
    async fn a_live_socket_and_another_file_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let socket = socket_in(&dir);
        let _listener = bind(&socket).unwrap();
        let Err(Error::Io(e)) = bind(&socket) else {
            panic!("a second daemon took a socket that is listened on");
        };
        assert_eq!(e.kind(), ErrorKind::AddrInUse);
        assert!(e.to_string().contains(&socket), "{e}");
        UnixStream::connect(&socket).await.unwrap();

        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "mine").unwrap();
        let Err(Error::Io(e)) = bind(file.to_str().unwrap()) else {
            panic!("a file that is no socket was replaced");
        };
        assert_eq!(e.kind(), ErrorKind::AddrInUse);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "mine");
    }
}
