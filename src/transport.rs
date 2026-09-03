// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Local IPC transport.
//!
//! Unix domain sockets on POSIX, named pipes on Windows. Both expose the same
//! three operations (bind, accept, connect) over streams that implement
//! `AsyncRead + AsyncWrite`, so the RPC framing and dispatch in
//! [`crate::ipc`] exists exactly once and is platform independent.

use std::io;
use std::path::Path;

#[cfg(unix)]
pub use unix::{ClientStream, Listener, ServerStream, cleanup, connect, describe, is_listening};

#[cfg(windows)]
pub use windows::{ClientStream, Listener, ServerStream, cleanup, connect, describe, is_listening};

#[cfg(not(any(unix, windows)))]
pub use unsupported::{
    ClientStream, Listener, ServerStream, cleanup, connect, describe, is_listening,
};

/// Error returned when an endpoint is already served by a live daemon.
#[must_use]
pub fn already_running(endpoint: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::AddrInUse,
        format!("daemon is already running at {}", endpoint.display()),
    )
}

// ---------------------------------------------------------------- unix

#[cfg(unix)]
mod unix {
    use super::{Path, already_running, io};
    use tokio::net::{UnixListener, UnixStream};

    /// Stream handed to the daemon for an accepted connection.
    pub type ServerStream = UnixStream;
    /// Stream used by a client to talk to the daemon.
    pub type ClientStream = UnixStream;

    /// Accepting side of a Unix domain socket.
    pub struct Listener {
        inner: UnixListener,
        /// Owner of the socket we created, which is our effective user.
        owner_uid: Option<u32>,
    }

    impl Listener {
        /// Bind the endpoint, removing a stale socket file if no daemon answers.
        ///
        /// # Errors
        /// Returns `AddrInUse` if another daemon is live, or an IO error if the
        /// socket cannot be created.
        pub async fn bind(endpoint: &Path) -> io::Result<Self> {
            use std::os::unix::fs::MetadataExt;

            if let Some(parent) = endpoint.parent()
                && !parent.as_os_str().is_empty()
            {
                crate::paths::create_private_dir(parent)?;
            }

            // The lock makes the stale-socket check atomic. Without it two
            // daemons starting at once can both decide the socket is stale, and
            // the loser then deletes the winner's live socket.
            let _lock = StartupLock::acquire(&endpoint.with_extension("lock")).await?;

            if endpoint.exists() {
                if is_listening(endpoint).await {
                    return Err(already_running(endpoint));
                }
                std::fs::remove_file(endpoint)?;
            }

            let inner = UnixListener::bind(endpoint)?;
            restrict_socket(endpoint)?;
            let owner_uid = std::fs::metadata(endpoint).map(|m| m.uid()).ok();

            Ok(Self { inner, owner_uid })
        }

        /// Wait for the next client connection.
        ///
        /// Connections from other users are refused and the loop continues.
        ///
        /// # Errors
        /// Returns an error if accepting fails.
        pub async fn accept(&mut self) -> io::Result<ServerStream> {
            loop {
                let (stream, _addr) = self.inner.accept().await?;
                let Some(owner) = self.owner_uid else {
                    return Ok(stream);
                };
                match stream.peer_cred() {
                    Ok(cred) if cred.uid() == owner => return Ok(stream),
                    Ok(cred) => tracing::warn!(
                        peer_uid = cred.uid(),
                        owner_uid = owner,
                        "refused IPC connection from another user"
                    ),
                    Err(e) => {
                        tracing::debug!(error = %e, "peer credentials unavailable, accepting");
                        return Ok(stream);
                    }
                }
            }
        }
    }

    /// Connect to a daemon.
    ///
    /// # Errors
    /// Returns an error if no daemon is listening.
    pub async fn connect(endpoint: &Path) -> io::Result<ClientStream> {
        UnixStream::connect(endpoint).await
    }

    /// Whether a live daemon answers on the endpoint.
    pub async fn is_listening(endpoint: &Path) -> bool {
        UnixStream::connect(endpoint).await.is_ok()
    }

    /// Remove the socket and lock file after shutdown.
    pub fn cleanup(endpoint: &Path) {
        let _ = std::fs::remove_file(endpoint);
        let _ = std::fs::remove_file(endpoint.with_extension("lock"));
    }

    /// Human-readable endpoint description.
    #[must_use]
    pub fn describe(endpoint: &Path) -> String {
        format!("unix socket {}", endpoint.display())
    }

    fn restrict_socket(endpoint: &Path) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(endpoint)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(endpoint, perms)
    }

    /// Exclusive startup lock, released on drop.
    struct StartupLock {
        path: std::path::PathBuf,
    }

    impl StartupLock {
        async fn acquire(path: &Path) -> io::Result<Self> {
            for _ in 0..50 {
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                {
                    Ok(mut file) => {
                        use std::io::Write as _;
                        let _ = write!(file, "{}", std::process::id());
                        return Ok(Self {
                            path: path.to_path_buf(),
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        if is_stale(path) {
                            let _ = std::fs::remove_file(path);
                            continue;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("could not acquire startup lock at {}", path.display()),
            ))
        }
    }

    /// A lock left behind by a killed process is ignored after 30 seconds.
    fn is_stale(path: &Path) -> bool {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .and_then(|t| t.elapsed().map_err(io::Error::other))
            .is_ok_and(|age| age > std::time::Duration::from_secs(30))
    }

    impl Drop for StartupLock {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

// ---------------------------------------------------------------- windows

#[cfg(windows)]
mod windows {
    use super::{Path, already_running, io};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    /// Windows error code for a pipe whose instances are all busy.
    const ERROR_PIPE_BUSY: i32 = 231;
    /// Windows error code returned when the first-instance flag is refused.
    const ERROR_ACCESS_DENIED: i32 = 5;

    /// Stream handed to the daemon for an accepted connection.
    pub type ServerStream = NamedPipeServer;
    /// Stream used by a client to talk to the daemon.
    pub type ClientStream = NamedPipeClient;

    /// Accepting side of a named pipe.
    pub struct Listener {
        name: String,
        pending: Option<NamedPipeServer>,
    }

    impl Listener {
        /// Create the first pipe instance for the endpoint.
        ///
        /// # Errors
        /// Returns `AddrInUse` if another daemon already owns the pipe.
        pub async fn bind(endpoint: &Path) -> io::Result<Self> {
            let name = pipe_name(endpoint);
            let pending = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)
                .map_err(|e| {
                    if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) {
                        already_running(endpoint)
                    } else {
                        e
                    }
                })?;
            Ok(Self {
                name,
                pending: Some(pending),
            })
        }

        /// Wait for the next client connection.
        ///
        /// # Errors
        /// Returns an error if the pipe cannot be served.
        pub async fn accept(&mut self) -> io::Result<ServerStream> {
            let server = self
                .pending
                .take()
                .ok_or_else(|| io::Error::other("named pipe listener is closed"))?;
            server.connect().await?;
            // Every instance serves exactly one client, so the next one has to
            // exist before this connection is handed off.
            self.pending = Some(
                ServerOptions::new()
                    .reject_remote_clients(true)
                    .create(&self.name)?,
            );
            Ok(server)
        }
    }

    /// Connect to a daemon, waiting briefly while all instances are busy.
    ///
    /// # Errors
    /// Returns an error if no daemon is listening.
    pub async fn connect(endpoint: &Path) -> io::Result<ClientStream> {
        let name = pipe_name(endpoint);
        for _ in 0..50 {
            match ClientOptions::new().open(&name) {
                Ok(client) => return Ok(client),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("named pipe {name} stayed busy"),
        ))
    }

    /// Whether a live daemon answers on the endpoint.
    pub async fn is_listening(endpoint: &Path) -> bool {
        let name = pipe_name(endpoint);
        match ClientOptions::new().open(&name) {
            Ok(_) => true,
            // Busy means a server exists but every instance is in use.
            Err(e) => e.raw_os_error() == Some(ERROR_PIPE_BUSY),
        }
    }

    /// Named pipes disappear with their owning process, so nothing to remove.
    pub fn cleanup(_endpoint: &Path) {}

    /// Human-readable endpoint description.
    #[must_use]
    pub fn describe(endpoint: &Path) -> String {
        format!("named pipe {}", pipe_name(endpoint))
    }

    /// Map an endpoint path to a named pipe name.
    ///
    /// Paths that already denote a pipe are used unchanged. Anything else, for
    /// example a `--socket` argument pointing into a temporary directory, is
    /// mapped into the devserial pipe namespace so the same argument works on
    /// every platform.
    fn pipe_name(endpoint: &Path) -> String {
        if crate::paths::is_pipe_path(endpoint) {
            return endpoint.to_string_lossy().into_owned();
        }
        let raw = endpoint.to_string_lossy();
        let slug: String = raw
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        format!(r"\\.\pipe\devserial-{slug}")
    }
}

// ---------------------------------------------------------------- fallback

#[cfg(not(any(unix, windows)))]
mod unsupported {
    use super::{Path, io};

    /// Placeholder stream type.
    pub type ServerStream = tokio::io::Empty;
    /// Placeholder stream type.
    pub type ClientStream = tokio::io::Empty;

    /// Placeholder listener.
    pub struct Listener;

    impl Listener {
        /// Always fails on unsupported platforms.
        ///
        /// # Errors
        /// Always returns `Unsupported`.
        pub async fn bind(_endpoint: &Path) -> io::Result<Self> {
            Err(unsupported())
        }

        /// Always fails on unsupported platforms.
        ///
        /// # Errors
        /// Always returns `Unsupported`.
        pub async fn accept(&mut self) -> io::Result<ServerStream> {
            Err(unsupported())
        }
    }

    /// Always fails on unsupported platforms.
    ///
    /// # Errors
    /// Always returns `Unsupported`.
    pub async fn connect(_endpoint: &Path) -> io::Result<ClientStream> {
        Err(unsupported())
    }

    /// No daemon can run on unsupported platforms.
    pub async fn is_listening(_endpoint: &Path) -> bool {
        false
    }

    /// Nothing to clean up.
    pub fn cleanup(_endpoint: &Path) {}

    /// Human-readable endpoint description.
    #[must_use]
    pub fn describe(endpoint: &Path) -> String {
        format!("unsupported endpoint {}", endpoint.display())
    }

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "local IPC is not supported on this platform",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_to_missing_endpoint_fails() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("absent.sock");
        assert!(!is_listening(&endpoint).await);
        assert!(connect(&endpoint).await.is_err());
    }

    #[tokio::test]
    async fn bind_accept_roundtrip() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("rt.sock");

        let mut listener = Listener::bind(&endpoint).await.unwrap();
        let server = tokio::spawn(async move {
            // Liveness probes connect and close again, exactly as the daemon's
            // accept loop has to tolerate.
            loop {
                let stream = listener.accept().await.unwrap();
                let (reader, mut writer) = tokio::io::split(stream);
                let mut lines = BufReader::new(reader).lines();
                let Some(received) = lines.next_line().await.unwrap() else {
                    continue;
                };
                writer
                    .write_all(format!("echo:{received}\n").as_bytes())
                    .await
                    .unwrap();
                writer.flush().await.unwrap();
                return;
            }
        });

        assert!(is_listening(&endpoint).await);

        let client = connect(&endpoint).await.unwrap();
        let (reader, mut writer) = tokio::io::split(client);
        writer.write_all(b"ping\n").await.unwrap();
        writer.flush().await.unwrap();
        let mut lines = BufReader::new(reader).lines();
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "echo:ping");

        server.await.unwrap();
        cleanup(&endpoint);
    }

    #[tokio::test]
    async fn second_bind_reports_address_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("dup.sock");
        let _first = Listener::bind(&endpoint).await.unwrap();
        let Err(err) = Listener::bind(&endpoint).await else {
            panic!("a second bind must be refused")
        };
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        cleanup(&endpoint);
    }

    #[test]
    fn describe_mentions_the_endpoint() {
        let text = describe(std::path::Path::new("/run/devserial/x.sock"));
        assert!(text.contains("x.sock") || text.contains("devserial"));
    }
}
