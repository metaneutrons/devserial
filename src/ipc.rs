// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! IPC server and client for background daemon communication.
//!
//! Uses Unix Domain Sockets on POSIX systems with newline-delimited JSON-RPC framing.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

use crate::engine::CommandEngine;
use crate::protocol::{RequestPayload, ResponsePayload, RpcRequest, RpcResponse};

/// Default location for the devserial daemon IPC socket.
#[must_use]
pub fn default_socket_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map_or_else(
            || PathBuf::from("./devserial.sock"),
            |h| PathBuf::from(h).join("Library/Application Support/devserial/devserial.sock"),
        )
    }
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(|r| PathBuf::from(r).join("devserial.sock"))
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|h| PathBuf::from(h).join(".local/state/devserial/devserial.sock"))
            })
            .unwrap_or_else(|| PathBuf::from("/tmp/devserial.sock"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        std::env::temp_dir().join("devserial.sock")
    }
}

/// Default location for the devserial daemon PID file.
#[must_use]
pub fn default_pid_path() -> PathBuf {
    let mut sock = default_socket_path();
    sock.set_extension("pid");
    sock
}

/// The IPC server running in the daemon process.
pub struct IpcServer {
    engine: CommandEngine,
    socket_path: PathBuf,
    pid_path: PathBuf,
}

impl IpcServer {
    /// Create a new IPC server.
    #[must_use]
    pub const fn new(engine: CommandEngine, socket_path: PathBuf, pid_path: PathBuf) -> Self {
        Self {
            engine,
            socket_path,
            pid_path,
        }
    }

    /// Run the IPC server accept loop until shutdown signal.
    ///
    /// # Errors
    /// Returns error if socket cannot be created or bound.
    pub async fn run(
        &self,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) -> Result<(), std::io::Error> {
        // Ensure parent directory exists
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Clean up stale socket file if daemon is not actually running
        if self.socket_path.exists() {
            if let Ok(mut stream) = UnixStream::connect(&self.socket_path).await {
                // Another daemon is actively listening!
                let _ = stream.shutdown().await;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!(
                        "daemon is already running at {}",
                        self.socket_path.display()
                    ),
                ));
            }
            std::fs::remove_file(&self.socket_path)?;
        }

        // Write PID file
        let pid = std::process::id();
        std::fs::write(&self.pid_path, pid.to_string())?;

        let listener = UnixListener::bind(&self.socket_path)?;
        tracing::info!(socket = %self.socket_path.display(), pid = pid, "IPC server listening");

        let (shutdown_tx, _) = broadcast::channel::<()>(1);

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    tracing::info!("IPC server received shutdown signal");
                    break;
                }
                accept_res = listener.accept() => {
                    match accept_res {
                        Ok((stream, _addr)) => {
                            let engine = self.engine.clone();
                            let shutdown_notifier = shutdown_tx.clone();
                            tokio::spawn(async move {
                                handle_client_connection(stream, engine, shutdown_notifier).await;
                            });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "IPC accept failed");
                        }
                    }
                }
            }
        }

        // Cleanup socket & PID file
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.pid_path);

        Ok(())
    }
}

async fn handle_client_connection(
    stream: UnixStream,
    engine: CommandEngine,
    shutdown_notifier: broadcast::Sender<()>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        match buf_reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let req: RpcRequest = match serde_json::from_str(trimmed) {
                    Ok(r) => r,
                    Err(e) => {
                        let err_resp = RpcResponse {
                            id: 0,
                            result: Err(format!("invalid JSON-RPC request: {e}")),
                        };
                        if let Ok(mut json) = serde_json::to_string(&err_resp) {
                            json.push('\n');
                            let _ = writer.write_all(json.as_bytes()).await;
                        }
                        continue;
                    }
                };

                let req_id = req.id;
                let is_shutdown = matches!(req.payload, RequestPayload::Shutdown);

                let result = engine.execute(req.payload).await;

                let resp = RpcResponse { id: req_id, result };

                if let Ok(mut json) = serde_json::to_string(&resp) {
                    json.push('\n');
                    if writer.write_all(json.as_bytes()).await.is_err() {
                        break;
                    }
                }

                if is_shutdown {
                    let _ = shutdown_notifier.send(());
                    break;
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "IPC connection read error");
                break;
            }
        }
    }
}

/// IPC client to communicate with a running devserial daemon.
#[derive(Debug, Clone)]
pub struct IpcClient {
    socket_path: PathBuf,
    next_id: Arc<AtomicU64>,
}

impl IpcClient {
    /// Create a new IPC client for the given socket path.
    #[must_use]
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Check if the daemon is currently running and responding to pings.
    pub async fn is_alive(&self) -> bool {
        matches!(
            self.send(RequestPayload::Ping).await,
            Ok(ResponsePayload::Pong)
        )
    }

    /// Send a request payload to the daemon and receive the response.
    ///
    /// # Errors
    /// Returns error if communication fails or daemon returns an error.
    pub async fn send(&self, payload: RequestPayload) -> Result<ResponsePayload, String> {
        let stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            format!(
                "failed to connect to daemon socket at {}: {e}",
                self.socket_path.display()
            )
        })?;

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = RpcRequest { id, payload };

        let mut req_json = serde_json::to_string(&req).map_err(|e| e.to_string())?;
        req_json.push('\n');

        let (reader, mut writer) = stream.into_split();
        writer
            .write_all(req_json.as_bytes())
            .await
            .map_err(|e| format!("IPC write error: {e}"))?;
        writer
            .flush()
            .await
            .map_err(|e| format!("IPC flush error: {e}"))?;

        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        buf_reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("IPC read error: {e}"))?;

        if line.is_empty() {
            return Err("daemon closed connection unexpectedly".to_string());
        }

        let resp: RpcResponse = serde_json::from_str(line.trim())
            .map_err(|e| format!("invalid response from daemon: {e} (raw: {line:?})"))?;

        resp.result
    }

    /// Ensure the daemon is running, auto-spawning it in background if necessary.
    ///
    /// # Errors
    /// Returns error if daemon cannot be spawned or fails to become ready within timeout.
    pub async fn ensure_daemon(&self) -> Result<(), String> {
        if self.is_alive().await {
            return Ok(());
        }

        let exe = std::env::current_exe()
            .map_err(|e| format!("failed to get current executable: {e}"))?;

        tracing::info!(socket = %self.socket_path.display(), "auto-spawning devserial daemon...");

        #[cfg(target_os = "windows")]
        {
            let mut cmd = std::process::Command::new(exe);
            cmd.arg("daemon")
                .arg("--socket")
                .arg(&self.socket_path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            cmd.spawn()
                .map_err(|e| format!("failed to spawn daemon: {e}"))?;
        }

        #[cfg(not(target_os = "windows"))]
        {
            let mut cmd = std::process::Command::new(exe);
            cmd.arg("daemon")
                .arg("--socket")
                .arg(&self.socket_path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            cmd.spawn()
                .map_err(|e| format!("failed to spawn daemon: {e}"))?;
        }

        // Wait with backoff up to 2.5s
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if self.is_alive().await {
                tracing::info!("daemon is ready!");
                return Ok(());
            }
        }

        Err("daemon auto-spawn timed out after 2.5s".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::port_manager::PortManagerHandle;
    use crate::state::StateDb;
    use std::sync::Mutex;

    #[tokio::test]
    async fn test_ipc_ping_pong() {
        let _ = std::fs::create_dir_all("./target/tmp");
        let dir = tempfile::Builder::new().tempdir_in("./target/tmp").unwrap();
        let socket_path = dir.path().join("test_daemon.sock");
        let pid_path = dir.path().join("test_daemon.pid");

        let pm = PortManagerHandle::new();
        let state_db = Arc::new(Mutex::new(StateDb::open_memory().unwrap()));
        let config = Arc::new(Config::default());
        let engine = CommandEngine::new(pm, state_db, config, dir.path().to_path_buf());

        let server = IpcServer::new(engine, socket_path.clone(), pid_path.clone());
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

        tokio::spawn(async move {
            let _ = server.run(shutdown_rx).await;
        });

        // Wait for server to bind
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = IpcClient::new(socket_path);
        let resp = client.send(RequestPayload::Ping).await.unwrap();
        assert_eq!(resp, ResponsePayload::Pong);

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn test_ipc_list_ports_empty() {
        let _ = std::fs::create_dir_all("./target/tmp");
        let dir = tempfile::Builder::new().tempdir_in("./target/tmp").unwrap();
        let socket_path = dir.path().join("test_list.sock");
        let pid_path = dir.path().join("test_list.pid");

        let pm = PortManagerHandle::new();
        let state_db = Arc::new(Mutex::new(StateDb::open_memory().unwrap()));
        let config = Arc::new(Config::default());
        let engine = CommandEngine::new(pm, state_db, config, dir.path().to_path_buf());

        let server = IpcServer::new(engine, socket_path.clone(), pid_path.clone());
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

        tokio::spawn(async move {
            let _ = server.run(shutdown_rx).await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = IpcClient::new(socket_path);
        let resp = client.send(RequestPayload::ListPorts).await.unwrap();
        assert_eq!(resp, ResponsePayload::PortList(vec![]));

        let _ = shutdown_tx.send(());
    }
}
