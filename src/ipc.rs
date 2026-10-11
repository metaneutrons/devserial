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

/// How long a stopping daemon goes on answering the requests it already has.
const DRAIN: Duration = Duration::from_secs(5);

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
        let mut connections = tokio::task::JoinSet::new();

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
                            connections.spawn(async move {
                                serve_connection(stream, engine, notifier).await;
                            });
                        }
                        Err(e) => tracing::warn!(error = %e, "IPC accept failed"),
                    }
                }
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }

        // Stopping (#114): nobody new gets in, and everyone already in gets
        // an answer. Removing the endpoint first makes a late client's connect
        // fail, which tells it the request never arrived; it starts a new
        // daemon and sends again. Connections already queued are taken and
        // served, and the daemon ends once they are done.
        transport::cleanup(&self.endpoint);
        // A daemon started meanwhile writes its own PID file; leave that one.
        if std::fs::read_to_string(&self.pid_path).is_ok_and(|held| held.trim() == pid.to_string())
        {
            let _ = std::fs::remove_file(&self.pid_path);
        }
        let deadline = tokio::time::Instant::now() + DRAIN;
        while let Ok(Ok(stream)) = tokio::time::timeout_at(
            deadline.min(tokio::time::Instant::now() + transport::QUEUE_QUIET),
            listener.accept(),
        )
        .await
        {
            let engine = self.engine.clone();
            let notifier = shutdown_tx.clone();
            connections.spawn(async move {
                serve_connection(stream, engine, notifier).await;
            });
        }
        drop(listener);
        let drained = tokio::time::timeout_at(deadline, async {
            while connections.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            tracing::warn!(
                left = connections.len(),
                "requests still running at shutdown were cut off"
            );
            connections.abort_all();
        }
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

/// Holds a lease on the daemon for a long-lived client (#114).
///
/// A window, the terminal monitor, an MCP session or `read --follow` keeps the
/// daemon from stopping through idleness for as long as one of these lives.
/// Every client opens a fresh connection per request, so there is no standing
/// connection the daemon could watch; the lease is renewed every
/// [`LEASE_RENEW_SECS`](crate::protocol::LEASE_RENEW_SECS) instead, from a
/// thread of its own, and released when this value is dropped. A client that
/// dies without dropping it loses the lease after three minutes.
///
/// A renewal never starts a daemon. It goes to whatever daemon is running, and
/// to none if none is: otherwise an open window would bring back a daemon that
/// was stopped on purpose. The next real operation starts one, and the next
/// renewal registers the lease with it.
pub struct LeaseKeeper {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LeaseKeeper {
    /// Hold a lease of this kind on the daemon `client` talks to.
    #[must_use]
    pub fn start(client: IpcClient, kind: crate::protocol::LeaseKind) -> Self {
        Self::start_every(
            client,
            kind,
            Duration::from_secs(crate::protocol::LEASE_RENEW_SECS),
        )
    }

    /// The same, renewing at another interval, for tests.
    #[must_use]
    pub fn start_every(
        client: IpcClient,
        kind: crate::protocol::LeaseKind,
        every: Duration,
    ) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let id = format!("{kind:?}-{}-{stamp}", std::process::id()).to_lowercase();
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        // A renewal that waits on a daemon in trouble must not hold up a quit.
        let client = client.with_timeout(Duration::from_secs(5));
        let thread = std::thread::Builder::new()
            .name("devserial-lease".to_string())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                loop {
                    let renewal = runtime.block_on(client.send(RequestPayload::RenewLease {
                        client: id.clone(),
                        kind,
                    }));
                    if let Err(e) = renewal {
                        tracing::debug!(error = %e, "lease renewal did not reach a daemon");
                    }
                    match stopped.recv_timeout(every) {
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                let _ = runtime.block_on(client.send(RequestPayload::ReleaseLease { client: id }));
            })
            .ok();
        Self {
            stop: Some(stop),
            thread,
        }
    }
}

impl Drop for LeaseKeeper {
    fn drop(&mut self) {
        // Closing the channel ends the renewal loop, which then releases.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
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

    /// Send a request, starting the daemon first if none answers.
    ///
    /// A daemon can stop between that check and the request, through its
    /// idle stop (#114). It then refuses the connection, so the request never
    /// left this process, and starting a new daemon and sending once more is
    /// safe whatever the request does. A failure after the request went out
    /// is returned as it is: the daemon may have acted on it.
    ///
    /// # Errors
    /// Returns an error if no daemon can be started or reached, or the
    /// daemon reports a failure.
    pub async fn send_starting(
        &self,
        payload: RequestPayload,
    ) -> Result<ResponsePayload, IpcError> {
        self.send_after(payload, || self.ensure_daemon()).await
    }

    /// [`send_starting`](Self::send_starting) with the start step given, so
    /// a test can stand in for spawning the binary.
    async fn send_after<F, Fut>(
        &self,
        payload: RequestPayload,
        start: F,
    ) -> Result<ResponsePayload, IpcError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<(), IpcError>>,
    {
        start().await?;
        match self.send(payload.clone()).await {
            Err(IpcError::Connect { .. }) => {
                tracing::debug!("the daemon went away before the request; starting a new one");
                start().await?;
                self.send(payload).await
            }
            other => other,
        }
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

        // Windows needs two things Stdio::null() does not provide.
        //
        // Stdio::null() redirects the daemon's own three handles. It does not
        // stop the daemon from inheriting the handles this process holds, and
        // CreateProcess passes on every handle marked inheritable. When
        // devserial's own output is a pipe, the daemon inherits the write end
        // and keeps it open for its whole life. Whoever reads that pipe then
        // waits for an end-of-file that never comes: `devserial stats COM3 |
        // more` did not return, and the CLI test that auto-starts the daemon
        // hung for as long as it was allowed to. Clearing the inheritance flag
        // before the spawn is the fix; there is no safe std API for it.
        //
        // DETACHED_PROCESS and CREATE_NEW_PROCESS_GROUP are separate and
        // independent: a child otherwise shares the console and the process
        // group, so Ctrl+C in the terminal would also reach a daemon that is
        // meant to outlive the command.
        #[cfg(windows)]
        {
            use std::os::windows::io::{AsRawHandle, RawHandle};
            use std::os::windows::process::CommandExt;

            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;

            // The only unsafe in this module, and the second in the crate
            // after the macOS integration in platform.rs. Declared locally
            // rather than through a dependency, for the same reason as there.
            #[allow(unsafe_code)]
            unsafe extern "system" {
                fn SetHandleInformation(handle: RawHandle, mask: u32, flags: u32) -> i32;
            }

            #[allow(unsafe_code)]
            fn stop_inheriting(handle: RawHandle) {
                if handle.is_null() {
                    return;
                }
                // SAFETY: the handle comes from the standard library's own std
                // stream and is valid for the life of the process. The call
                // only clears an inheritance flag on it and cannot invalidate
                // it. A failure is not actionable here: the spawn still works,
                // the daemon merely keeps inheriting, so the return value is
                // deliberately dropped.
                let _ = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
            }

            stop_inheriting(std::io::stdin().as_raw_handle());
            stop_inheriting(std::io::stdout().as_raw_handle());
            stop_inheriting(std::io::stderr().as_raw_handle());

            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
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

    async fn leases(client: &IpcClient) -> Vec<crate::protocol::LeaseKind> {
        match client.send(RequestPayload::GetLifecycle).await {
            Ok(ResponsePayload::Lifecycle(state)) => state.leases,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// #114: a keeper registers its lease, keeps it while alive, and
    /// releases it when dropped.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_lease_is_held_while_the_keeper_lives() {
        use crate::protocol::LeaseKind;
        let h = harness("lease").await;
        let before = leases(&h.client).await;
        assert!(before.is_empty(), "{before:?}");

        let keeper =
            LeaseKeeper::start_every(h.client.clone(), LeaseKind::Mcp, Duration::from_millis(50));
        let mut held = Vec::new();
        for _ in 0..50 {
            held = leases(&h.client).await;
            if !held.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(held, vec![LeaseKind::Mcp]);

        // Dropping joins the renewal thread, which releases on its way out.
        tokio::task::spawn_blocking(move || drop(keeper))
            .await
            .unwrap();
        assert!(
            leases(&h.client).await.is_empty(),
            "the lease outlived its keeper"
        );
        let _ = h.shutdown.send(());
    }

    /// Serve a fresh engine on `endpoint` until the returned sender fires.
    fn serve(endpoint: &std::path::Path, dir: &std::path::Path) -> broadcast::Sender<()> {
        serve_watched(endpoint, dir).0
    }

    /// The same, with the server task to watch.
    fn serve_watched(
        endpoint: &std::path::Path,
        dir: &std::path::Path,
    ) -> (broadcast::Sender<()>, tokio::task::JoinHandle<()>) {
        let mut config = Config::default();
        config.global.data_dir = dir.to_path_buf();
        config.global.archive_dir = dir.join("archive");
        let engine = CommandEngine::new(
            PortManagerHandle::new(),
            Arc::new(Mutex::new(StateDb::open_memory().unwrap())),
            Arc::new(config),
        );
        let server = IpcServer::new(engine, endpoint.to_path_buf(), dir.join("serve.pid"));
        let (shutdown, shutdown_rx) = broadcast::channel::<()>(1);
        let task = tokio::spawn(async move {
            let _ = server.run(shutdown_rx).await;
        });
        (shutdown, task)
    }

    /// #114: a daemon that stops answers the requests on the connections it
    /// already has before it ends, and a late client is refused at once.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stopping_daemon_answers_the_requests_it_already_has() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("drain.sock");
        let client = IpcClient::new(endpoint.clone()).with_timeout(Duration::from_secs(5));
        let (shutdown, server) = serve_watched(&endpoint, dir.path());
        for _ in 0..100 {
            if client.is_alive().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let stream = transport::connect(&endpoint).await.expect("connect");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = shutdown.send(());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !server.is_finished(),
            "the daemon ended with a connection still to answer"
        );
        assert!(
            matches!(
                client.send(RequestPayload::Ping).await,
                Err(IpcError::Connect { .. })
            ),
            "a late client was not refused"
        );

        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);
        write_frame(
            &mut writer,
            &RpcRequest {
                id: 7,
                payload: RequestPayload::Ping,
            },
        )
        .await
        .expect("write");
        let frame = read_frame(&mut reader, MAX_FRAME_BYTES)
            .await
            .expect("read")
            .expect("an answer");
        let answer: RpcResponse = serde_json::from_slice(&frame).expect("json");
        assert_eq!(answer.result, Ok(ResponsePayload::Pong));

        drop((reader, writer));
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("the daemon did not end once it had answered")
            .expect("server task");
    }

    /// #114: a request whose connection was refused never arrived, so it goes
    /// to a new daemon.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_that_never_arrived_goes_to_a_new_daemon() {
        use std::sync::atomic::AtomicUsize;
        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("late.sock");
        let client = IpcClient::new(endpoint.clone()).with_timeout(Duration::from_secs(5));
        let starts = AtomicUsize::new(0);
        let started = Mutex::new(None);

        // The first start finds the old daemon, which is gone by the time the
        // request goes out; the second start brings a new one.
        let answer = client
            .send_after(RequestPayload::ListPorts, || {
                let round = starts.fetch_add(1, Ordering::SeqCst) + 1;
                let (endpoint, dir, started, client) = (&endpoint, dir.path(), &started, &client);
                async move {
                    if round == 2 {
                        *started.lock().unwrap() = Some(serve(endpoint, dir));
                        for _ in 0..100 {
                            if client.is_alive().await {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    }
                    Ok(())
                }
            })
            .await;
        assert_eq!(
            answer.expect("an answer"),
            ResponsePayload::PortList(vec![])
        );
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        let second = started.lock().unwrap().take();
        if let Some(shutdown) = second {
            let _ = shutdown.send(());
        }
    }

    /// #114: a request that went out is never sent again, because the daemon
    /// may have acted on it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_that_went_out_is_not_sent_again() {
        use std::sync::atomic::AtomicUsize;
        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("mute.sock");
        let mut listener = transport::Listener::bind(&endpoint).await.expect("bind");
        let received = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&received);
        tokio::spawn(async move {
            while let Ok(stream) = listener.accept().await {
                let (reader, _writer) = tokio::io::split(stream);
                let mut reader = BufReader::new(reader);
                if let Ok(Some(_)) = read_frame(&mut reader, MAX_FRAME_BYTES).await {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
                // Hangs up without an answer.
            }
        });

        let client = IpcClient::new(endpoint).with_timeout(Duration::from_secs(5));
        let starts = AtomicUsize::new(0);
        let answer = client
            .send_after(RequestPayload::ListPorts, || {
                starts.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            })
            .await;
        assert!(matches!(answer, Err(IpcError::Protocol(_))), "{answer:?}");
        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "started again after sending"
        );
        assert_eq!(
            received.load(Ordering::SeqCst),
            1,
            "the request went out twice"
        );
    }

    /// Poll until the daemon at `client` reports `want` as its leases.
    async fn wait_for_leases(client: &IpcClient, want: &[crate::protocol::LeaseKind]) -> bool {
        for _ in 0..100 {
            if let Ok(ResponsePayload::Lifecycle(state)) =
                client.send(RequestPayload::GetLifecycle).await
                && state.leases == want
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    /// #114: when a daemon stops and a real operation starts a new one, the
    /// keeper's next renewal holds the new daemon. Between the two, with no
    /// daemon, the renewals leave nothing behind.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_next_renewal_holds_a_restarted_daemon() {
        use crate::protocol::LeaseKind;
        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("restart.sock");
        let client = IpcClient::new(endpoint.clone()).with_timeout(Duration::from_secs(5));

        let first = serve(&endpoint, dir.path());
        assert!(wait_for_leases(&client, &[]).await, "no first daemon");
        let keeper =
            LeaseKeeper::start_every(client.clone(), LeaseKind::Tui, Duration::from_millis(30));
        assert!(
            wait_for_leases(&client, &[LeaseKind::Tui]).await,
            "the first daemon was not held"
        );

        let _ = first.send(());
        for _ in 0..100 {
            if !client.is_alive().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!client.is_alive().await, "the first daemon did not stop");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!endpoint.exists(), "a renewal started a daemon");

        // What a real operation does through `ensure_daemon`.
        let second = serve(&endpoint, dir.path());
        assert!(
            wait_for_leases(&client, &[LeaseKind::Tui]).await,
            "the next renewal did not hold the new daemon"
        );

        tokio::task::spawn_blocking(move || drop(keeper))
            .await
            .unwrap();
        assert!(
            wait_for_leases(&client, &[]).await,
            "the lease outlived its keeper"
        );
        let _ = second.send(());
    }

    /// #114: a renewal never starts a daemon. With nothing listening, a keeper
    /// renewing many times leaves no endpoint behind.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_renewal_never_starts_a_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("absent.sock");
        let keeper = LeaseKeeper::start_every(
            IpcClient::new(endpoint.clone()),
            crate::protocol::LeaseKind::Gui,
            Duration::from_millis(20),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        tokio::task::spawn_blocking(move || drop(keeper))
            .await
            .unwrap();
        assert!(!endpoint.exists(), "a renewal started a daemon");
    }
}
