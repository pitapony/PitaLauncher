//! The activation socket: how a second launch reaches the running launcher.
//!
//! [`InstanceLock`](crate::InstanceLock) already stops two copies of PitaGames
//! from scanning and writing at once. This module is the other half: when the
//! lock is busy, the second process asks the running instance to come to the
//! front and exits, instead of leaving the user clicking a launcher that appears
//! to do nothing.
//!
//! The channel is a Unix-domain socket next to the lock:
//!
//! ```text
//! ~/.local/share/pitagames/instance.sock
//! ```
//!
//! One line of text, one request ("focus"), one acknowledgement ("ok"). It
//! carries no data and no credentials, so a plain socket in the user's data
//! directory (`0600`) is enough. A stale socket file left by a crash is
//! detected by probing it and removed before binding; the file is removed again
//! on drop.
//!
//! This lives in `core` rather than in the GUI because it is part of the
//! single-instance contract both binaries share. It is Linux-shaped like the
//! rest of the workspace and only compiled on Unix.

use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The socket's file name inside the data directory.
pub const ACTIVATION_FILE: &str = "instance.sock";

/// The one request a client can make.
pub const REQUEST_FOCUS: &str = "focus";

/// The server's reply to a handled request.
pub const ACK: &str = "ok";

/// Longest request body accepted, including the trailing newline.
pub const MAX_REQUEST_BYTES: usize = 32;

/// Conservative socket-path limit: Linux allows 108 bytes, macOS 104, and both
/// count the terminating NUL. A longer `XDG_DATA_HOME` cannot host the socket;
/// the caller degrades to "no focus forwarding" and says so.
pub const MAX_SOCKET_PATH: usize = 100;

/// How long a client waits for the acknowledgement before giving up.
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// A request read from the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation {
    /// Bring the running window to the front.
    Focus,
    /// Anything else: ignored by the launcher, acknowledged for the client's
    /// benefit so it knows the message arrived.
    Unknown(String),
}

/// A bound activation socket, listening for one-line requests.
///
/// Drop it to stop listening and remove the socket file. The accept loop is the
/// caller's: `accept` blocks, so run it on a dedicated thread.
#[derive(Debug)]
pub struct ActivationServer {
    listener: UnixListener,
    path: PathBuf,
}

impl ActivationServer {
    /// Bind the socket at `path`, clearing a stale file from a crashed run.
    ///
    /// Returns [`ActivationError::AlreadyRunning`] when something is already
    /// listening there — that means the lock and the socket disagree, which the
    /// caller should report rather than silently steal.
    pub fn bind(path: impl AsRef<Path>) -> Result<Self, ActivationError> {
        let path = path.as_ref().to_path_buf();

        let length = path.as_os_str().len();
        if length > MAX_SOCKET_PATH {
            return Err(ActivationError::PathTooLong {
                path,
                length,
                max: MAX_SOCKET_PATH,
            });
        }

        if let Some(parent) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|source| ActivationError::Io {
                action: "create the parent directory of",
                path: path.clone(),
                source,
            })?;
        }

        // A file may be left over from a crash (or a `SIGKILL`). Probe it: a
        // listener answers, a corpse refuses the connection and can go.
        if path.exists() {
            match UnixStream::connect(&path) {
                Ok(_) => return Err(ActivationError::AlreadyRunning { path }),
                Err(_) => {
                    let _ = fs::remove_file(&path);
                }
            }
        }

        let listener = UnixListener::bind(&path).map_err(|source| ActivationError::Io {
            action: "bind",
            path: path.clone(),
            source,
        })?;

        // Only this user can ask; the request is harmless but the socket is ours.
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));

        Ok(Self { listener, path })
    }

    /// The socket file this server bound.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Block until one request arrives, acknowledge it, and return it.
    ///
    /// A client that connects and says nothing is dropped after [`IO_TIMEOUT`];
    /// that is a timeout, not a server failure.
    pub fn accept(&self) -> Result<Activation, ActivationError> {
        let (mut stream, _address) =
            self.listener
                .accept()
                .map_err(|source| ActivationError::Io {
                    action: "accept a connection on",
                    path: self.path.clone(),
                    source,
                })?;

        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

        let request = read_request(&mut stream)?;
        let reply = match request.as_str() {
            REQUEST_FOCUS => {
                let _ = stream.write_all(format!("{ACK}\n").as_bytes());
                Activation::Focus
            }
            other => {
                let _ = stream.write_all(b"unknown\n");
                Activation::Unknown(other.to_string())
            }
        };
        let _ = stream.shutdown(std::net::Shutdown::Both);

        Ok(reply)
    }
}

impl Drop for ActivationServer {
    fn drop(&mut self) {
        // Best effort: the socket file is ours, and a stale one is cleaned on the
        // next bind anyway.
        let _ = fs::remove_file(&self.path);
    }
}

/// Read one newline-terminated request, bounded by [`MAX_REQUEST_BYTES`].
fn read_request(stream: &mut UnixStream) -> Result<String, ActivationError> {
    let mut buffer = [0u8; MAX_REQUEST_BYTES];
    let mut filled = 0usize;

    loop {
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => {
                filled += count;
                if buffer[..filled].contains(&b'\n') || filled == buffer.len() {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(source) => {
                return Err(ActivationError::Io {
                    action: "read a request from",
                    path: PathBuf::new(),
                    source,
                });
            }
        }
    }

    Ok(String::from_utf8_lossy(&buffer[..filled])
        .trim()
        .to_string())
}

/// Ask the running launcher to come to the front.
///
/// `Ok(())` means the request was delivered *and* acknowledged, so the caller
/// can honestly say it raised the window. A refused connection means no
/// launcher is listening (nothing bound the socket), which is a normal state for
/// an older build or a crashed instance.
pub fn request_focus(path: impl AsRef<Path>) -> Result<(), ActivationError> {
    let path = path.as_ref();
    let mut stream = UnixStream::connect(path).map_err(|source| ActivationError::Io {
        action: "connect to",
        path: path.to_path_buf(),
        source,
    })?;
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    stream
        .write_all(format!("{REQUEST_FOCUS}\n").as_bytes())
        .map_err(|source| ActivationError::Io {
            action: "write a request to",
            path: path.to_path_buf(),
            source,
        })?;
    let _ = stream.shutdown(std::net::Shutdown::Write);

    let mut reply = [0u8; 16];
    let read = stream
        .read(&mut reply)
        .map_err(|source| ActivationError::Io {
            action: "read the reply from",
            path: path.to_path_buf(),
            source,
        })?;
    let reply = String::from_utf8_lossy(&reply[..read]).trim().to_string();

    if reply == ACK {
        Ok(())
    } else {
        Err(ActivationError::Rejected { reply })
    }
}

/// Activation-channel failures.
#[derive(Debug, thiserror::Error)]
pub enum ActivationError {
    /// The socket path is longer than the platform's `sockaddr_un` allows.
    #[error("the activation socket path {path} is {length} bytes; the limit is {max}")]
    PathTooLong {
        path: PathBuf,
        length: usize,
        max: usize,
    },

    /// Another process is already listening on the socket.
    #[error("another PitaGames instance is already listening on {path}")]
    AlreadyRunning { path: PathBuf },

    /// The instance answered, but not with an acknowledgement.
    #[error("the running instance did not acknowledge the request (reply: `{reply}`)")]
    Rejected { reply: String },

    #[error("failed to {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    /// Serve exactly one request on its own thread, then drop the server (which
    /// removes the socket file) and return what it read.
    fn serve_one(path: &Path) -> thread::JoinHandle<Activation> {
        let server = ActivationServer::bind(path).unwrap();
        thread::spawn(move || server.accept().unwrap())
    }

    #[test]
    fn a_focus_request_round_trips_and_is_acknowledged() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(ACTIVATION_FILE);
        let handle = serve_one(&path);
        assert!(path.exists());

        request_focus(&path).unwrap();
        assert_eq!(handle.join().unwrap(), Activation::Focus);
    }

    #[test]
    fn an_unknown_request_is_reported_and_does_not_raise_the_window() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(ACTIVATION_FILE);
        let handle = serve_one(&path);

        let mut stream = UnixStream::connect(&path).unwrap();
        stream.write_all(b"shutdown\n").unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();

        assert_eq!(
            handle.join().unwrap(),
            Activation::Unknown("shutdown".to_string())
        );
    }

    #[test]
    fn a_second_bind_reports_the_listening_instance() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(ACTIVATION_FILE);
        let _server = ActivationServer::bind(&path).unwrap();

        let error = ActivationServer::bind(&path).unwrap_err();
        assert!(
            matches!(error, ActivationError::AlreadyRunning { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_stale_socket_file_is_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(ACTIVATION_FILE);
        // A bound socket whose listener is gone is exactly what a crash leaves.
        {
            let listener = UnixListener::bind(&path).unwrap();
            drop(listener);
        }
        assert!(path.exists());

        // Binding succeeds (the corpse is removed) and the new socket really
        // serves requests.
        let handle = serve_one(&path);
        request_focus(&path).unwrap();
        assert_eq!(handle.join().unwrap(), Activation::Focus);
    }

    #[test]
    fn a_path_longer_than_the_socket_limit_is_refused() {
        let long = PathBuf::from(format!("/tmp/{}", "a".repeat(MAX_SOCKET_PATH)));
        let error = ActivationServer::bind(&long).unwrap_err();
        match error {
            ActivationError::PathTooLong { length, max, .. } => {
                assert!(length > max);
                assert_eq!(max, MAX_SOCKET_PATH);
            }
            other => panic!("expected PathTooLong, got {other}"),
        }
    }

    #[test]
    fn requesting_focus_without_a_listener_fails_without_hanging() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(ACTIVATION_FILE);

        let started = Instant::now();
        let error = request_focus(&path).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(error, ActivationError::Io { .. }), "{error}");
    }

    #[test]
    fn a_silent_client_is_dropped_after_the_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(ACTIVATION_FILE);
        let server = ActivationServer::bind(&path).unwrap();

        let handle = thread::spawn(move || server.accept().unwrap());
        // Connect and say nothing, then close: the server must not block forever.
        let stream = UnixStream::connect(&path).unwrap();
        stream.shutdown(std::net::Shutdown::Both).unwrap();

        assert_eq!(handle.join().unwrap(), Activation::Unknown(String::new()));
    }
}
