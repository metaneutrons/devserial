// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Newline-delimited JSON-RPC over the local [`crate::transport`].
//!
//! Framing and dispatch are written once and are platform independent; the
//! transport decides whether that is a Unix domain socket or a Windows named
//! pipe.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

use crate::engine::CommandEngine;
use crate::paths;
use crate::protocol::{RequestPayload, ResponsePayload, RpcRequest, RpcResponse};
use crate::transport;

/// Largest request line the daemon accepts, to bound memory per connection.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Default time a client waits for a response.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an auto-spawned daemon may take to become reachable.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);

/// Default location for the devserial daemon IPC endpoint.
#[must_use]
pub fn default_socket_path() -> PathBuf {
    paths::default_socket_path()
}

/// Default location for the devserial daemon PID file.
#[must_use]
pub fn default_pid_path() -> PathBuf {
    paths::pid_path(&paths::default_socket_path())
}

/// Default location for the devserial GUI multiplexer endpoint.
#[must_use]
pub fn default_gui_socket_path() -> PathBuf {
    paths::default_gui_socket_path()
}

/// Errors surfaced to clients of the daemon.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("cannot reach the devserial daemon on {endpoint}: {source}")]
    Connect {
        endpoint: String,
        #[source]
        source: std::io::Error,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("the daemon did not answer within {0:?}")]
    Timeout(Duration),
    #[error("malformed response from daemon: {0}")]
    Protocol(String),
    #[error("{0}")]
    Remote(String),
    #[error("could not start the daemon: {0}")]
    Spawn(String),
    #[error("the daemon is not available on this platform: {0}")]
    Unsupported(String),
}

/// The IPC server running in the daemon process.
pub struct IpcServer {
    engine: CommandEngine,
    endpoint: PathBuf,
    pid_path: PathBuf,
}

impl IpcServer {
    /// Create a new IPC server.
    #[must_use]
    pub const fn new(engine: CommandEngine, endpoint: PathBuf, pid_path: PathBuf) -> Self {
        Self {
            engine,
            endpoint,
            pid_path,
        }
    }

    /// Run the accept loop until the shutdown signal fires.
    ///
    /// # Errors
    /// Returns an error if the endpoint cannot be bound.
    pub async fn run(
        &self,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) -> Result<(), std::io::Error> {
        let mut listener = transport::Listener::bind(&self.endpoint).await?;

        if let Some(parent) = self.pid_path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = paths::create_private_dir(parent);
        }
        let pid = std::process::id();
        if let Err(e) = std::fs::write(&self.pid_path, pid.to_string()) {
            tracing::warn!(path = %self.pid_path.display(), error = %e, "could not write PID file");
        }

        tracing::info!(
            endpoint = %transport::describe(&self.endpoint),
            pid,
            "IPC server listening"
        );

        // Two ways to stop: the caller's signal, and a client asking for
        // shutdown over the wire. The second receiver has to be subscribed
        // here; without it a `Shutdown` request was acknowledged and then
        // silently ignored, leaving the daemon running.
        let (shutdown_tx, mut requested_rx) = broadcast::channel::<()>(1);

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    tracing::info!("IPC server received shutdown signal");
                    break;
                }
                _ = requested_rx.recv() => {
                    tracing::info!("client requested shutdown");
                    break;
                }
                accepted = listener.accept() => {
                    match accepted {
                        Ok(stream) => {
                            let engine = self.engine.clone();
                            let notifier = shutdown_tx.clone();
                            tokio::spawn(async move {
                                serve_connection(stream, engine, notifier).await;
                            });
                        }
                        Err(e) => tracing::warn!(error = %e, "IPC accept failed"),
                    }
                }
            }
        }

        transport::cleanup(&self.endpoint);
        let _ = std::fs::remove_file(&self.pid_path);
        Ok(())
    }
}

/// Serve one client connection until it closes or requests shutdown.
async fn serve_connection<S>(stream: S, engine: CommandEngine, shutdown: broadcast::Sender<()>)
where
    S: tokio::io::AsyncRead + AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    loop {
        let frame = match read_frame(&mut reader, MAX_FRAME_BYTES).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(error = %e, "IPC connection read error");
                break;
            }
        };

        let text = String::from_utf8_lossy(&frame);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: RpcRequest = match serde_json::from_str(trimmed) {
            Ok(request) => request,
            Err(e) => {
                let response = RpcResponse {
                    id: 0,
                    result: Err(format!("invalid JSON-RPC request: {e}")),
                };
                if write_frame(&mut writer, &response).await.is_err() {
                    break;
                }
                continue;
            }
        };

        let id = request.id;
        let is_shutdown = matches!(request.payload, RequestPayload::Shutdown);
        let result = engine
            .execute(request.payload)
            .await
            .map_err(|e| e.to_string());

        if write_frame(&mut writer, &RpcResponse { id, result })
            .await
            .is_err()
        {
            break;
        }

        if is_shutdown {
            let _ = shutdown.send(());
            break;
        }
    }
}

/// Read one newline-delimited frame, refusing oversized input.
async fn read_frame<R>(reader: &mut R, max_bytes: usize) -> Result<Option<Vec<u8>>, std::io::Error>
where
    R: AsyncBufRead + Unpin,
{
    let mut out = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if out.is_empty() { None } else { Some(out) });
        }
        if let Some(index) = available.iter().position(|b| *b == b'\n') {
            out.extend_from_slice(&available[..index]);
            reader.consume(index + 1);
            return Ok(Some(out));
        }
        let len = available.len();
        out.extend_from_slice(available);
        reader.consume(len);
        if out.len() > max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("frame exceeds {max_bytes} bytes"),
            ));
        }
    }
}

/// Write one newline-delimited JSON frame.
async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
    T: serde::Serialize + Sync,
{
    let mut json = serde_json::to_string(value).map_err(std::io::Error::other)?;
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;
    writer.flush().await
}

/// IPC client to communicate with a running devserial daemon.
#[derive(Debug, Clone)]
pub struct IpcClient {
    endpoint: PathBuf,
    next_id: Arc<AtomicU64>,
    timeout: Duration,
    config_path: Option<PathBuf>,
}

impl IpcClient {
    /// Create a new IPC client for the given endpoint.
    #[must_use]
    pub fn new(endpoint: PathBuf) -> Self {
        Self {
            endpoint,
            next_id: Arc::new(AtomicU64::new(1)),
            timeout: DEFAULT_REQUEST_TIMEOUT,
            config_path: None,
        }
    }

    /// Set the response timeout.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Pass a configuration file on to an auto-spawned daemon.
    #[must_use]
    pub fn with_config_path(mut self, path: Option<PathBuf>) -> Self {
        self.config_path = path;
        self
    }

    /// The endpoint this client talks to.
    #[must_use]
    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }

    /// Check whether a daemon is listening and answering.
    pub async fn is_alive(&self) -> bool {
        matches!(
            self.send(RequestPayload::Ping).await,
            Ok(ResponsePayload::Pong)
        )
    }

    /// Send a request payload to the daemon and receive the response.
    ///
    /// # Errors
    /// Returns an error if the daemon is unreachable, times out, or reports a
    /// failure.
    pub async fn send(&self, payload: RequestPayload) -> Result<ResponsePayload, IpcError> {
        tokio::time::timeout(self.timeout, self.exchange(payload))
            .await
            .unwrap_or(Err(IpcError::Timeout(self.timeout)))
    }

    async fn exchange(&self, payload: RequestPayload) -> Result<ResponsePayload, IpcError> {
        let stream =
            transport::connect(&self.endpoint)
                .await
                .map_err(|source| IpcError::Connect {
                    endpoint: transport::describe(&self.endpoint),
                    source,
                })?;

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (reader, mut writer) = tokio::io::split(stream);
        write_frame(&mut writer, &RpcRequest { id, payload }).await?;

        let mut reader = BufReader::new(reader);
        let frame = read_frame(&mut reader, MAX_FRAME_BYTES)
            .await?
            .ok_or_else(|| IpcError::Protocol("daemon closed the connection".to_string()))?;

        let text = String::from_utf8_lossy(&frame);
        let response: RpcResponse = serde_json::from_str(text.trim())
            .map_err(|e| IpcError::Protocol(format!("{e} (raw: {text:?})")))?;

        response.result.map_err(IpcError::Remote)
    }

    /// Ensure the daemon is running, spawning it in the background if needed.
    ///
    /// # Errors
    /// Returns an error if the daemon cannot be spawned or does not become
    /// reachable in time.
    pub async fn ensure_daemon(&self) -> Result<(), IpcError> {
        if self.is_alive().await {
            return Ok(());
        }

        let exe = std::env::current_exe()
            .map_err(|e| IpcError::Spawn(format!("cannot locate the devserial binary: {e}")))?;

        tracing::info!(
            endpoint = %transport::describe(&self.endpoint),
            "starting devserial daemon in the background"
        );

        let mut cmd = std::process::Command::new(exe);
        cmd.arg("daemon")
            .arg("--socket")
            .arg(&self.endpoint)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(config) = &self.config_path {
            cmd.env(paths::ENV_CONFIG, config);
        }
        cmd.spawn()
            .map_err(|e| IpcError::Spawn(format!("could not spawn the daemon: {e}")))?;

        let deadline = tokio::time::Instant::now() + DAEMON_START_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if self.is_alive().await {
                tracing::info!("daemon is ready");
                return Ok(());
            }
        }

        Err(IpcError::Spawn(format!(
            "the daemon did not become reachable within {DAEMON_START_TIMEOUT:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::port_manager::PortManagerHandle;
    use crate::state::StateDb;
    use std::sync::Mutex;

    struct Harness {
        client: IpcClient,
        shutdown: broadcast::Sender<()>,
        _dir: tempfile::TempDir,
    }

    async fn harness(name: &str) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join(format!("{name}.sock"));
        let pid_path = dir.path().join(format!("{name}.pid"));

        let mut config = Config::default();
        config.global.data_dir = dir.path().to_path_buf();
        config.global.archive_dir = dir.path().join("archive");

        let engine = CommandEngine::new(
            PortManagerHandle::new(),
            Arc::new(Mutex::new(StateDb::open_memory().unwrap())),
            Arc::new(config),
        );

        let server = IpcServer::new(engine, endpoint.clone(), pid_path);
        let (shutdown, shutdown_rx) = broadcast::channel::<()>(1);
        tokio::spawn(async move {
            let _ = server.run(shutdown_rx).await;
        });

        let client = IpcClient::new(endpoint).with_timeout(Duration::from_secs(5));
        for _ in 0..100 {
            if client.is_alive().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        Harness {
            client,
            shutdown,
            _dir: dir,
        }
    }

    #[tokio::test]
    async fn test_ipc_ping_pong() {
        let h = harness("ping").await;
        let resp = h.client.send(RequestPayload::Ping).await.unwrap();
        assert_eq!(resp, ResponsePayload::Pong);
        let _ = h.shutdown.send(());
    }

    #[tokio::test]
    async fn test_ipc_list_ports_empty() {
        let h = harness("list").await;
        let resp = h.client.send(RequestPayload::ListPorts).await.unwrap();
        assert_eq!(resp, ResponsePayload::PortList(vec![]));
        let _ = h.shutdown.send(());
    }

    #[tokio::test]
    async fn engine_errors_reach_the_client_as_messages() {
        let h = harness("err").await;
        let err = h
            .client
            .send(RequestPayload::GetStats {
                port: "/dev/absent".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, IpcError::Remote(_)));
        assert!(err.to_string().contains("not found"));
        let _ = h.shutdown.send(());
    }

    #[tokio::test]
    async fn a_shutdown_request_actually_stops_the_server() {
        let h = harness("stop").await;
        assert!(h.client.is_alive().await);

        let ack = h.client.send(RequestPayload::Shutdown).await.unwrap();
        assert_eq!(ack, ResponsePayload::ShutdownAck);

        for _ in 0..100 {
            if !h.client.is_alive().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the server kept accepting connections after a shutdown request");
    }

    #[tokio::test]
    async fn unreachable_endpoint_is_reported_as_such() {
        let dir = tempfile::tempdir().unwrap();
        let client =
            IpcClient::new(dir.path().join("absent.sock")).with_timeout(Duration::from_millis(500));
        let err = client.send(RequestPayload::Ping).await.unwrap_err();
        assert!(matches!(err, IpcError::Connect { .. }));
        assert!(!client.is_alive().await);
    }

    #[tokio::test]
    async fn oversized_frames_are_refused() {
        let mut reader = BufReader::new(&b"aaaaaaaaaaaaaaaa"[..]);
        let err = read_frame(&mut reader, 4).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn frames_split_on_newlines() {
        let mut reader = BufReader::new(&b"one\ntwo\n"[..]);
        assert_eq!(
            read_frame(&mut reader, 1024).await.unwrap().unwrap(),
            b"one".to_vec()
        );
        assert_eq!(
            read_frame(&mut reader, 1024).await.unwrap().unwrap(),
            b"two".to_vec()
        );
        assert!(read_frame(&mut reader, 1024).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_trailing_frame_without_newline_is_still_returned() {
        let mut reader = BufReader::new(&b"tail"[..]);
        assert_eq!(
            read_frame(&mut reader, 1024).await.unwrap().unwrap(),
            b"tail".to_vec()
        );
    }
}
