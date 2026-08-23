// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::AsyncRead;
use tokio::sync::{mpsc, oneshot};

use crate::config::PortConfig;
use crate::reader::{
    ConnectionState, PortReaderHandle, ReaderFactory, spawn_reader_with_reconnect,
};
use crate::storage::SqliteStorage;

/// A boxed async reader that can be sent across threads.
type BoxedReader = Box<dyn AsyncRead + Unpin + Send>;

/// Handle to the underlying serial port for write/signal operations.
pub type SerialPortHandle = Arc<tokio::sync::Mutex<serial2_tokio::SerialPort>>;

/// Handle to a managed port's resources.
struct ManagedPort {
    reader_handle: PortReaderHandle,
    storage: Arc<std::sync::Mutex<SqliteStorage>>,
    serial_port: Option<SerialPortHandle>,
    config: PortConfig,
}

/// Info about a managed port returned by `list`.
#[derive(Debug, Clone)]
pub struct PortInfo {
    pub name: String,
    pub state: ConnectionState,
    pub total_lines: u64,
}

/// Commands sent to the `PortManager` actor.
enum PortCommand {
    OpenReal {
        name: String,
        config: PortConfig,
        data_dir: std::path::PathBuf,
        reply: oneshot::Sender<Result<(), PortManagerError>>,
    },
    OpenMock {
        name: String,
        reader: BoxedReader,
        config: PortConfig,
        storage: Arc<std::sync::Mutex<SqliteStorage>>,
        reply: oneshot::Sender<Result<(), PortManagerError>>,
    },
    Close {
        name: String,
        reply: oneshot::Sender<Result<(), PortManagerError>>,
    },
    CloseWithConfig {
        name: String,
        reply: oneshot::Sender<Result<PortConfig, PortManagerError>>,
    },
    List {
        reply: oneshot::Sender<Vec<PortInfo>>,
    },
    GetState {
        name: String,
        reply: oneshot::Sender<Result<ConnectionState, PortManagerError>>,
    },
    GetStorage {
        name: String,
        reply: oneshot::Sender<Result<Arc<std::sync::Mutex<SqliteStorage>>, PortManagerError>>,
    },
    GetSerialPort {
        name: String,
        reply: oneshot::Sender<Result<SerialPortHandle, PortManagerError>>,
    },
    Write {
        name: String,
        data: Vec<u8>,
        reply: oneshot::Sender<Result<(), PortManagerError>>,
    },
}

/// Errors from the port manager.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PortManagerError {
    #[error("port '{0}' not found")]
    NotFound(String),
    #[error("port '{0}' already managed")]
    AlreadyManaged(String),
    #[error("serial port error: {0}")]
    Serial(String),
    #[error("port '{0}' has no hardware handle (mock port)")]
    NoHardware(String),
}

/// Handle to communicate with the `PortManager` actor.
#[derive(Clone)]
pub struct PortManagerHandle {
    tx: mpsc::Sender<PortCommand>,
}

impl PortManagerHandle {
    /// Spawn the port manager actor and return a handle.
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(port_manager_loop(rx));
        Self { tx }
    }

    /// Open a real serial port.
    ///
    /// # Errors
    /// Returns error if the port is already managed or cannot be opened.
    pub async fn open_serial(
        &self,
        name: String,
        config: PortConfig,
        data_dir: std::path::PathBuf,
    ) -> Result<(), PortManagerError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(PortCommand::OpenReal {
                name,
                config,
                data_dir,
                reply: reply_tx,
            })
            .await
            .ok();
        reply_rx
            .await
            .unwrap_or_else(|_| Err(PortManagerError::NotFound("channel closed".into())))
    }

    /// Open a port with any `AsyncRead` source (for testing).
    ///
    /// # Errors
    /// Returns error if the port is already managed.
    pub async fn open(
        &self,
        name: String,
        reader: impl AsyncRead + Unpin + Send + 'static,
        config: PortConfig,
        storage: Arc<std::sync::Mutex<SqliteStorage>>,
    ) -> Result<(), PortManagerError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(PortCommand::OpenMock {
                name,
                reader: Box::new(reader),
                config,
                storage,
                reply: reply_tx,
            })
            .await
            .ok();
        reply_rx
            .await
            .unwrap_or_else(|_| Err(PortManagerError::NotFound("channel closed".into())))
    }

    /// Close a managed port.
    ///
    /// # Errors
    /// Returns error if the port is not found.
    pub async fn close(&self, name: &str) -> Result<(), PortManagerError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(PortCommand::Close {
                name: name.to_string(),
                reply: reply_tx,
            })
            .await
            .ok();
        reply_rx
            .await
            .unwrap_or_else(|_| Err(PortManagerError::NotFound("channel closed".into())))
    }

    /// Close a managed port and return its config for reopening.
    ///
    /// # Errors
    /// Returns error if the port is not found.
    pub async fn close_with_config(&self, name: &str) -> Result<PortConfig, PortManagerError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(PortCommand::CloseWithConfig {
                name: name.to_string(),
                reply: reply_tx,
            })
            .await
            .ok();
        reply_rx
            .await
            .unwrap_or_else(|_| Err(PortManagerError::NotFound("channel closed".into())))
    }

    /// List all managed ports with their state.
    pub async fn list(&self) -> Vec<PortInfo> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(PortCommand::List { reply: reply_tx })
            .await
            .ok();
        reply_rx.await.unwrap_or_default()
    }

    /// Get connection state of a specific port.
    ///
    /// # Errors
    /// Returns error if the port is not found.
    pub async fn get_state(&self, name: &str) -> Result<ConnectionState, PortManagerError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(PortCommand::GetState {
                name: name.to_string(),
                reply: reply_tx,
            })
            .await
            .ok();
        reply_rx
            .await
            .unwrap_or_else(|_| Err(PortManagerError::NotFound("channel closed".into())))
    }

    /// Get storage handle for a specific port.
    ///
    /// # Errors
    /// Returns error if the port is not found.
    pub async fn get_storage(
        &self,
        name: &str,
    ) -> Result<Arc<std::sync::Mutex<SqliteStorage>>, PortManagerError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(PortCommand::GetStorage {
                name: name.to_string(),
                reply: reply_tx,
            })
            .await
            .ok();
        reply_rx
            .await
            .unwrap_or_else(|_| Err(PortManagerError::NotFound("channel closed".into())))
    }

    /// Get the serial port handle for signal control.
    ///
    /// # Errors
    /// Returns error if the port is not found or is a mock port.
    pub async fn get_serial_port(&self, name: &str) -> Result<SerialPortHandle, PortManagerError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(PortCommand::GetSerialPort {
                name: name.to_string(),
                reply: reply_tx,
            })
            .await
            .ok();
        reply_rx
            .await
            .unwrap_or_else(|_| Err(PortManagerError::NotFound("channel closed".into())))
    }

    /// Write data to a serial port.
    ///
    /// # Errors
    /// Returns error if the port is not found or write fails.
    pub async fn write(&self, name: &str, data: Vec<u8>) -> Result<(), PortManagerError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(PortCommand::Write {
                name: name.to_string(),
                data,
                reply: reply_tx,
            })
            .await
            .ok();
        reply_rx
            .await
            .unwrap_or_else(|_| Err(PortManagerError::NotFound("channel closed".into())))
    }
}

impl Default for PortManagerHandle {
    fn default() -> Self {
        Self::new()
    }
}

use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::ReadBuf;

/// Thread-safe duplex wrapper around a single `serial2_tokio::SerialPort`.
///
/// Allows concurrent reading (via `AsyncRead`) and writing/signaling
/// without opening multiple OS file descriptors or holding locks during I/O waits.
#[derive(Clone)]
pub struct SharedSerialPort {
    inner: Arc<tokio::sync::Mutex<serial2_tokio::SerialPort>>,
}

impl SharedSerialPort {
    /// Create a new shared serial port from an open port.
    #[must_use]
    pub fn new(port: serial2_tokio::SerialPort) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(port)),
        }
    }

    /// Get the shared handle for write/signal operations.
    #[must_use]
    pub fn handle(&self) -> SerialPortHandle {
        Arc::clone(&self.inner)
    }
}

impl AsyncRead for SharedSerialPort {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let Ok(mut guard) = self.inner.try_lock() else {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        };
        Pin::new(&mut *guard).poll_read(cx, buf)
    }
}

/// Factory that reconnects to a real serial port.
struct SerialReconnectFactory {
    path: String,
    config: PortConfig,
    port_handle: SerialPortHandle,
}

impl ReaderFactory for SerialReconnectFactory {
    fn try_connect(&self) -> crate::reader::ReconnectFuture<'_> {
        Box::pin(async {
            match open_serial_port_raw(&self.path, &self.config) {
                Ok(new_port) => {
                    let mut guard = self.port_handle.lock().await;
                    *guard = new_port;
                    drop(guard);
                    let shared = SharedSerialPort {
                        inner: Arc::clone(&self.port_handle),
                    };
                    Some(Box::new(shared) as Box<dyn tokio::io::AsyncRead + Unpin + Send>)
                }
                Err(e) => {
                    tracing::debug!(path = %self.path, error = %e, "reconnect failed");
                    None
                }
            }
        })
    }
}

/// Open a serial port with the given configuration (public for standalone mode).
///
/// # Errors
/// Returns error if the port cannot be opened.
pub fn open_serial_port_raw(
    path: &str,
    config: &PortConfig,
) -> Result<serial2_tokio::SerialPort, std::io::Error> {
    let port = serial2_tokio::SerialPort::open(path, |mut settings: serial2::Settings| {
        settings.set_baud_rate(config.baudrate)?;
        settings.set_char_size(match config.data_bits {
            5 => serial2::CharSize::Bits5,
            6 => serial2::CharSize::Bits6,
            7 => serial2::CharSize::Bits7,
            _ => serial2::CharSize::Bits8,
        });
        settings.set_stop_bits(match config.stop_bits {
            2 => serial2::StopBits::Two,
            _ => serial2::StopBits::One,
        });
        settings.set_parity(match config.parity.as_str() {
            "odd" => serial2::Parity::Odd,
            "even" => serial2::Parity::Even,
            _ => serial2::Parity::None,
        });
        settings.set_flow_control(match config.flow_control.as_str() {
            "software" => serial2::FlowControl::XonXoff,
            "hardware" => serial2::FlowControl::RtsCts,
            _ => serial2::FlowControl::None,
        });
        Ok(settings)
    })?;
    Ok(port)
}

#[allow(clippy::too_many_lines)]
async fn port_manager_loop(mut rx: mpsc::Receiver<PortCommand>) {
    let mut ports: HashMap<String, ManagedPort> = HashMap::new();

    while let Some(cmd) = rx.recv().await {
        match cmd {
            PortCommand::OpenReal {
                name,
                config,
                data_dir,
                reply,
            } => {
                if ports.contains_key(&name) {
                    reply.send(Err(PortManagerError::AlreadyManaged(name))).ok();
                    continue;
                }

                let result = open_serial_port_raw(&name, &config);
                match result {
                    Ok(port) => {
                        let sanitized = name.replace(['/', '\\'], "_");
                        let db_path = data_dir.join(format!("{sanitized}.db"));
                        let storage = match SqliteStorage::open(&db_path) {
                            Ok(s) => Arc::new(std::sync::Mutex::new(s)),
                            Err(e) => {
                                reply
                                    .send(Err(PortManagerError::Serial(e.to_string())))
                                    .ok();
                                continue;
                            }
                        };

                        let shared = SharedSerialPort::new(port);
                        let port_handle = shared.handle();

                        let factory = SerialReconnectFactory {
                            path: name.clone(),
                            config: config.clone(),
                            port_handle: Arc::clone(&port_handle),
                        };

                        let reader_handle =
                            spawn_reader_with_reconnect(shared, &storage, &config, factory);

                        ports.insert(
                            name,
                            ManagedPort {
                                reader_handle,
                                storage,
                                serial_port: Some(port_handle),
                                config,
                            },
                        );
                        reply.send(Ok(())).ok();
                    }
                    Err(e) => {
                        reply
                            .send(Err(PortManagerError::Serial(e.to_string())))
                            .ok();
                    }
                }
            }
            PortCommand::OpenMock {
                name,
                reader,
                config,
                storage,
                reply,
            } => {
                if ports.contains_key(&name) {
                    reply.send(Err(PortManagerError::AlreadyManaged(name))).ok();
                    continue;
                }
                let reader_handle = spawn_reader_with_reconnect(
                    reader,
                    &storage,
                    &config,
                    crate::reader::NoReconnect,
                );
                ports.insert(
                    name,
                    ManagedPort {
                        reader_handle,
                        storage,
                        serial_port: None,
                        config,
                    },
                );
                reply.send(Ok(())).ok();
            }
            PortCommand::Close { name, reply } => {
                if let Some(port) = ports.remove(&name) {
                    port.reader_handle.shutdown().await;
                    reply.send(Ok(())).ok();
                } else {
                    reply.send(Err(PortManagerError::NotFound(name))).ok();
                }
            }
            PortCommand::CloseWithConfig { name, reply } => {
                if let Some(port) = ports.remove(&name) {
                    port.reader_handle.shutdown().await;
                    reply.send(Ok(port.config)).ok();
                } else {
                    reply.send(Err(PortManagerError::NotFound(name))).ok();
                }
            }
            PortCommand::List { reply } => {
                let mut infos = Vec::new();
                for (name, port) in &ports {
                    let state = port.reader_handle.state();
                    let total_lines = port
                        .storage
                        .lock()
                        .ok()
                        .and_then(|s| s.get_stats().ok())
                        .map_or(0, |s| s.total_lines);
                    infos.push(PortInfo {
                        name: name.clone(),
                        state,
                        total_lines,
                    });
                }
                reply.send(infos).ok();
            }
            PortCommand::GetState { name, reply } => {
                if let Some(port) = ports.get(&name) {
                    reply.send(Ok(port.reader_handle.state())).ok();
                } else {
                    reply.send(Err(PortManagerError::NotFound(name))).ok();
                }
            }
            PortCommand::GetStorage { name, reply } => {
                if let Some(port) = ports.get(&name) {
                    reply.send(Ok(Arc::clone(&port.storage))).ok();
                } else {
                    reply.send(Err(PortManagerError::NotFound(name))).ok();
                }
            }
            PortCommand::GetSerialPort { name, reply } => {
                if let Some(port) = ports.get(&name) {
                    if let Some(ref sp) = port.serial_port {
                        reply.send(Ok(Arc::clone(sp))).ok();
                    } else {
                        reply.send(Err(PortManagerError::NoHardware(name))).ok();
                    }
                } else {
                    reply.send(Err(PortManagerError::NotFound(name))).ok();
                }
            }
            PortCommand::Write { name, data, reply } => {
                if let Some(port) = ports.get(&name) {
                    if let Some(ref sp) = port.serial_port {
                        let sp = Arc::clone(sp);
                        let result = async {
                            let guard = sp.lock().await;
                            guard
                                .write_all(&data)
                                .await
                                .map_err(|e| PortManagerError::Serial(e.to_string()))
                        }
                        .await;
                        reply.send(result).ok();
                    } else {
                        reply.send(Err(PortManagerError::NoHardware(name))).ok();
                    }
                } else {
                    reply.send(Err(PortManagerError::NotFound(name))).ok();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::ConnectionState;
    use crate::testutil::mock_serial::mock_serial;
    use std::time::Duration;

    fn test_storage() -> Arc<std::sync::Mutex<SqliteStorage>> {
        Arc::new(std::sync::Mutex::new(SqliteStorage::open_memory().unwrap()))
    }

    #[tokio::test]
    async fn test_open_and_list() {
        let mgr = PortManagerHandle::new();
        let (mock, _ctrl) = mock_serial(100);

        mgr.open(
            "/dev/ttyUSB0".into(),
            mock,
            PortConfig::default(),
            test_storage(),
        )
        .await
        .unwrap();

        let ports = mgr.list().await;
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].name, "/dev/ttyUSB0");
        assert_eq!(ports[0].state, ConnectionState::Connected);
    }

    #[tokio::test]
    async fn test_open_two_ports() {
        let mgr = PortManagerHandle::new();
        let (m1, _c1) = mock_serial(100);
        let (m2, _c2) = mock_serial(100);

        mgr.open("port1".into(), m1, PortConfig::default(), test_storage())
            .await
            .unwrap();
        mgr.open("port2".into(), m2, PortConfig::default(), test_storage())
            .await
            .unwrap();

        let ports = mgr.list().await;
        assert_eq!(ports.len(), 2);
    }

    #[tokio::test]
    async fn test_open_duplicate_error() {
        let mgr = PortManagerHandle::new();
        let (m1, _c1) = mock_serial(100);
        let (m2, _c2) = mock_serial(100);
        let storage = test_storage();

        mgr.open(
            "port1".into(),
            m1,
            PortConfig::default(),
            Arc::clone(&storage),
        )
        .await
        .unwrap();
        let err = mgr
            .open("port1".into(), m2, PortConfig::default(), storage)
            .await
            .unwrap_err();
        assert!(matches!(err, PortManagerError::AlreadyManaged(_)));
    }

    #[tokio::test]
    async fn test_close_port() {
        let mgr = PortManagerHandle::new();
        let (mock, _ctrl) = mock_serial(100);

        mgr.open("port1".into(), mock, PortConfig::default(), test_storage())
            .await
            .unwrap();
        mgr.close("port1").await.unwrap();

        let ports = mgr.list().await;
        assert!(ports.is_empty());
    }

    #[tokio::test]
    async fn test_close_nonexistent() {
        let mgr = PortManagerHandle::new();
        let err = mgr.close("nope").await.unwrap_err();
        assert!(matches!(err, PortManagerError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_get_state_nonexistent() {
        let mgr = PortManagerHandle::new();
        let err = mgr.get_state("nope").await.unwrap_err();
        assert!(matches!(err, PortManagerError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_disconnect_reflected_in_state() {
        let mgr = PortManagerHandle::new();
        let (mock, ctrl) = mock_serial(100);

        mgr.open("port1".into(), mock, PortConfig::default(), test_storage())
            .await
            .unwrap();

        ctrl.simulate_disconnect();
        ctrl.feed_data(b"trigger").await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        let state = mgr.get_state("port1").await.unwrap();
        assert!(matches!(state, ConnectionState::Disconnected { .. }));
    }

    #[tokio::test]
    async fn test_get_serial_port_mock_returns_no_hardware() {
        let mgr = PortManagerHandle::new();
        let (mock, _ctrl) = mock_serial(100);

        mgr.open("port1".into(), mock, PortConfig::default(), test_storage())
            .await
            .unwrap();

        let err = mgr.get_serial_port("port1").await.unwrap_err();
        assert!(matches!(err, PortManagerError::NoHardware(_)));
    }
}
