// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Owner of every open serial port.
//!
//! A single actor task holds the port table; all access goes through
//! [`PortManagerHandle`]. Opening and reconfiguring share one settings
//! mapping, so a framing option can never be honoured on one path and ignored
//! on the other.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::{OwnedMutexGuard, mpsc, oneshot};

use crate::config::PortConfig;
use crate::paths;
use crate::reader::{
    ConnectionState, FlushSettings, PortReaderHandle, ReaderFactory, spawn_reader_with_reconnect,
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
    GetConfig {
        name: String,
        reply: oneshot::Sender<Result<PortConfig, PortManagerError>>,
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
    Reconfigure {
        name: String,
        config: PortConfig,
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
    #[error("port manager is not running")]
    Unavailable,
    #[error("storage error: {0}")]
    Storage(String),
}

/// Handle to communicate with the `PortManager` actor.
#[derive(Clone)]
pub struct PortManagerHandle {
    tx: mpsc::Sender<PortCommand>,
}

impl PortManagerHandle {
    /// Spawn the port manager actor with default write batching.
    ///
    /// Must be called from within a Tokio runtime context.
    #[must_use]
    pub fn new() -> Self {
        Self::with_flush(FlushSettings::default())
    }

    /// Spawn the port manager actor with configured write batching.
    ///
    /// Must be called from within a Tokio runtime context.
    #[must_use]
    pub fn with_flush(flush: FlushSettings) -> Self {
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(port_manager_loop(rx, flush));
        Self { tx }
    }

    /// Send a command and await its reply.
    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> PortCommand,
    ) -> Result<T, PortManagerError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(make(reply_tx))
            .await
            .map_err(|_| PortManagerError::Unavailable)?;
        reply_rx.await.map_err(|_| PortManagerError::Unavailable)
    }

    /// Send a command whose reply is itself fallible.
    async fn request_flat<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, PortManagerError>>) -> PortCommand,
    ) -> Result<T, PortManagerError> {
        self.request(make).await?
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
        self.request_flat(|reply| PortCommand::OpenReal {
            name,
            config,
            data_dir,
            reply,
        })
        .await
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
        self.request_flat(|reply| PortCommand::OpenMock {
            name,
            reader: Box::new(reader),
            config,
            storage,
            reply,
        })
        .await
    }

    /// Close a managed port.
    ///
    /// # Errors
    /// Returns error if the port is not found.
    pub async fn close(&self, name: &str) -> Result<(), PortManagerError> {
        self.request_flat(|reply| PortCommand::Close {
            name: name.to_string(),
            reply,
        })
        .await
    }

    /// Close a managed port and return its config for reopening.
    ///
    /// # Errors
    /// Returns error if the port is not found.
    pub async fn close_with_config(&self, name: &str) -> Result<PortConfig, PortManagerError> {
        self.request_flat(|reply| PortCommand::CloseWithConfig {
            name: name.to_string(),
            reply,
        })
        .await
    }

    /// List all managed ports with their state.
    pub async fn list(&self) -> Vec<PortInfo> {
        self.request(|reply| PortCommand::List { reply })
            .await
            .unwrap_or_default()
    }

    /// Get connection state of a specific port.
    ///
    /// # Errors
    /// Returns error if the port is not found.
    pub async fn get_state(&self, name: &str) -> Result<ConnectionState, PortManagerError> {
        self.request_flat(|reply| PortCommand::GetState {
            name: name.to_string(),
            reply,
        })
        .await
    }

    /// Get the effective configuration of a managed port.
    ///
    /// # Errors
    /// Returns error if the port is not found.
    pub async fn get_config(&self, name: &str) -> Result<PortConfig, PortManagerError> {
        self.request_flat(|reply| PortCommand::GetConfig {
            name: name.to_string(),
            reply,
        })
        .await
    }

    /// Get storage handle for a specific port.
    ///
    /// # Errors
    /// Returns error if the port is not found.
    pub async fn get_storage(
        &self,
        name: &str,
    ) -> Result<Arc<std::sync::Mutex<SqliteStorage>>, PortManagerError> {
        self.request_flat(|reply| PortCommand::GetStorage {
            name: name.to_string(),
            reply,
        })
        .await
    }

    /// Get the serial port handle for signal control.
    ///
    /// # Errors
    /// Returns error if the port is not found or is a mock port.
    pub async fn get_serial_port(&self, name: &str) -> Result<SerialPortHandle, PortManagerError> {
        self.request_flat(|reply| PortCommand::GetSerialPort {
            name: name.to_string(),
            reply,
        })
        .await
    }

    /// Write data to a serial port.
    ///
    /// # Errors
    /// Returns error if the port is not found or write fails.
    pub async fn write(&self, name: &str, data: Vec<u8>) -> Result<(), PortManagerError> {
        self.request_flat(|reply| PortCommand::Write {
            name: name.to_string(),
            data,
            reply,
        })
        .await
    }

    /// Reconfigure an open serial port with new baud rate and framing settings.
    ///
    /// # Errors
    /// Returns error if the port is not found or reconfiguration fails.
    pub async fn reconfigure(
        &self,
        name: &str,
        config: PortConfig,
    ) -> Result<(), PortManagerError> {
        self.request_flat(|reply| PortCommand::Reconfigure {
            name: name.to_string(),
            config,
            reply,
        })
        .await
    }
}

impl Default for PortManagerHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe duplex wrapper around a single `serial2_tokio::SerialPort`.
///
/// Allows concurrent reading (via `AsyncRead`) and writing/signalling without
/// opening multiple OS file descriptors. While another task holds the port, for
/// example during a file transfer, the reader waits on the mutex instead of
/// polling it, so a long transfer costs no CPU on the reader side.
pub struct SharedSerialPort {
    inner: Arc<tokio::sync::Mutex<serial2_tokio::SerialPort>>,
    /// Pending lock acquisition, kept across polls so the waker is registered
    /// with the mutex rather than being woken immediately.
    pending_lock: Option<
        Pin<
            Box<
                dyn std::future::Future<Output = OwnedMutexGuard<serial2_tokio::SerialPort>> + Send,
            >,
        >,
    >,
}

impl Clone for SharedSerialPort {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            pending_lock: None,
        }
    }
}

impl SharedSerialPort {
    /// Create a new shared serial port from an open port.
    #[must_use]
    pub fn new(port: serial2_tokio::SerialPort) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(port)),
            pending_lock: None,
        }
    }

    /// Wrap an existing handle.
    #[must_use]
    pub fn from_handle(handle: SerialPortHandle) -> Self {
        Self {
            inner: handle,
            pending_lock: None,
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
        let this = self.get_mut();

        if let Some(fut) = this.pending_lock.as_mut() {
            match fut.as_mut().poll(cx) {
                Poll::Ready(mut guard) => {
                    this.pending_lock = None;
                    return Pin::new(&mut *guard).poll_read(cx, buf);
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        if let Ok(mut guard) = Arc::clone(&this.inner).try_lock_owned() {
            return Pin::new(&mut *guard).poll_read(cx, buf);
        }

        // Wait on the mutex instead of polling it, so a long write or transfer
        // costs the reader nothing.
        let mut acquiring = Box::pin(Arc::clone(&this.inner).lock_owned());
        match acquiring.as_mut().poll(cx) {
            Poll::Ready(mut guard) => Pin::new(&mut *guard).poll_read(cx, buf),
            Poll::Pending => {
                this.pending_lock = Some(acquiring);
                Poll::Pending
            }
        }
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
                    Some(
                        Box::new(SharedSerialPort::from_handle(Arc::clone(&self.port_handle)))
                            as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
                    )
                }
                Err(e) => {
                    tracing::debug!(path = %self.path, error = %e, "reconnect failed");
                    None
                }
            }
        })
    }
}

/// Serial ports the operating system reports, one entry per physical device.
///
/// macOS and the BSDs expose every device twice, as a callout node
/// (`/dev/cu.NAME`) and a dial-in node (`/dev/tty.NAME`). They are the same
/// hardware, so only the callout node is offered: it is the correct node for
/// outgoing use, while opening the dial-in node blocks until carrier detect.
/// Listing both made one device look like two, so a second window could try to
/// open hardware that was already busy, and a successful open would have split
/// the capture buffer across two databases.
#[must_use]
pub fn available_ports() -> Vec<String> {
    let ports: Vec<String> = serial2_tokio::SerialPort::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.display().to_string())
        .collect();
    sort_ports(dedupe_device_nodes(&ports))
}

/// Canonical name for a device node.
///
/// Two names that map onto the same canonical name denote the same hardware.
#[must_use]
pub fn canonical_device(port: &str) -> String {
    if (cfg!(any(
        target_os = "macos",
        target_os = "ios",
        target_vendor = "apple"
    )) || cfg!(target_os = "freebsd")
        || cfg!(target_os = "netbsd")
        || cfg!(target_os = "openbsd"))
        && let Some(name) = port.strip_prefix("/dev/tty.")
    {
        return format!("/dev/cu.{name}");
    }
    port.to_string()
}

/// Whether two port names denote the same physical device.
#[must_use]
pub fn same_device(left: &str, right: &str) -> bool {
    left == right || canonical_device(left) == canonical_device(right)
}

/// Drop dial-in nodes whose callout twin is also present.
///
/// A dial-in node without a twin is kept, so nothing is ever hidden that the
/// user could not otherwise reach.
fn dedupe_device_nodes(ports: &[String]) -> Vec<String> {
    let known: std::collections::HashSet<&str> = ports.iter().map(String::as_str).collect();
    ports
        .iter()
        .filter(|port| {
            let canonical = canonical_device(port);
            canonical.as_str() == port.as_str() || !known.contains(canonical.as_str())
        })
        .cloned()
        .collect()
}

/// USB-style devices first, then alphabetically.
fn sort_ports(mut ports: Vec<String>) -> Vec<String> {
    ports.sort_by(|a, b| {
        looks_like_usb(b)
            .cmp(&looks_like_usb(a))
            .then_with(|| a.cmp(b))
    });
    ports
}

fn looks_like_usb(port: &str) -> bool {
    let lower = port.to_ascii_lowercase();
    lower.contains("usb") || lower.contains("acm")
}

/// Apply a port configuration to serial line settings.
///
/// This is the single mapping from [`PortConfig`] onto `serial2::Settings`.
/// Opening a port and reconfiguring an open port both go through it.
///
/// # Errors
/// Returns an error if the baud rate is rejected by the driver.
pub fn apply_settings(
    settings: &mut serial2::Settings,
    config: &PortConfig,
) -> Result<(), std::io::Error> {
    settings.set_baud_rate(config.baudrate)?;
    settings.set_char_size(config.data_bits.to_serial2());
    settings.set_stop_bits(config.stop_bits.to_serial2());
    settings.set_parity(config.parity.to_serial2());
    settings.set_flow_control(config.flow_control.to_serial2());
    Ok(())
}

/// Open a serial port with the given configuration (public for standalone mode).
///
/// # Errors
/// Returns error if the port cannot be opened.
pub fn open_serial_port_raw(
    path: &str,
    config: &PortConfig,
) -> Result<serial2_tokio::SerialPort, std::io::Error> {
    serial2_tokio::SerialPort::open(path, |mut settings: serial2::Settings| {
        apply_settings(&mut settings, config)?;
        Ok(settings)
    })
}

/// Reconfigure an open serial port with new settings.
///
/// # Errors
/// Returns error if reconfiguring fails.
pub fn reconfigure_serial_port(
    port: &mut serial2_tokio::SerialPort,
    config: &PortConfig,
) -> Result<(), std::io::Error> {
    let mut settings = port.get_configuration()?;
    apply_settings(&mut settings, config)?;
    port.set_configuration(&settings)
}

async fn port_manager_loop(mut rx: mpsc::Receiver<PortCommand>, flush: FlushSettings) {
    let mut ports: HashMap<String, ManagedPort> = HashMap::new();

    while let Some(cmd) = rx.recv().await {
        handle_command(&mut ports, cmd, flush).await;
    }
}

/// Carry out one command against the port table.
#[allow(clippy::too_many_lines)] // one arm per command, each of them short
async fn handle_command(
    ports: &mut HashMap<String, ManagedPort>,
    cmd: PortCommand,
    flush: FlushSettings,
) {
    {
        match cmd {
            PortCommand::OpenReal {
                name,
                config,
                data_dir,
                reply,
            } => {
                let result = open_real_port(ports, &name, config, &data_dir, flush);
                reply.send(result).ok();
            }
            PortCommand::OpenMock {
                name,
                reader,
                config,
                storage,
                reply,
            } => {
                let result = match ports.entry(name) {
                    std::collections::hash_map::Entry::Occupied(entry) => {
                        Err(PortManagerError::AlreadyManaged(entry.key().clone()))
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let reader_handle = spawn_reader_with_reconnect(
                            reader,
                            &storage,
                            &config,
                            flush,
                            crate::reader::NoReconnect,
                        );
                        entry.insert(ManagedPort {
                            reader_handle,
                            storage,
                            serial_port: None,
                            config,
                        });
                        Ok(())
                    }
                };
                reply.send(result).ok();
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
                let mut infos = Vec::with_capacity(ports.len());
                for (name, port) in &*ports {
                    // Deliberately the cheap query: `list` runs often and must
                    // not scan the whole capture table per port.
                    let total_lines = port
                        .storage
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .line_count()
                        .unwrap_or(0);
                    infos.push(PortInfo {
                        name: name.clone(),
                        state: port.reader_handle.state(),
                        total_lines,
                    });
                }
                infos.sort_by(|a, b| a.name.cmp(&b.name));
                reply.send(infos).ok();
            }
            PortCommand::GetState { name, reply } => {
                let result = ports
                    .get(&name)
                    .map(|p| p.reader_handle.state())
                    .ok_or(PortManagerError::NotFound(name));
                reply.send(result).ok();
            }
            PortCommand::GetConfig { name, reply } => {
                let result = ports
                    .get(&name)
                    .map(|p| p.config.clone())
                    .ok_or(PortManagerError::NotFound(name));
                reply.send(result).ok();
            }
            PortCommand::GetStorage { name, reply } => {
                let result = ports
                    .get(&name)
                    .map(|p| Arc::clone(&p.storage))
                    .ok_or(PortManagerError::NotFound(name));
                reply.send(result).ok();
            }
            PortCommand::GetSerialPort { name, reply } => {
                let result = match ports.get(&name) {
                    Some(port) => port
                        .serial_port
                        .as_ref()
                        .map(Arc::clone)
                        .ok_or(PortManagerError::NoHardware(name)),
                    None => Err(PortManagerError::NotFound(name)),
                };
                reply.send(result).ok();
            }
            PortCommand::Write { name, data, reply } => {
                let handle = match ports.get(&name) {
                    Some(port) => port
                        .serial_port
                        .as_ref()
                        .map(Arc::clone)
                        .ok_or(PortManagerError::NoHardware(name)),
                    None => Err(PortManagerError::NotFound(name)),
                };
                let result = match handle {
                    Ok(sp) => {
                        let guard = sp.lock().await;
                        guard
                            .write_all(&data)
                            .await
                            .map_err(|e| PortManagerError::Serial(e.to_string()))
                    }
                    Err(e) => Err(e),
                };
                reply.send(result).ok();
            }
            PortCommand::Reconfigure {
                name,
                config,
                reply,
            } => {
                let result = reconfigure_managed(ports, &name, config).await;
                reply.send(result).ok();
            }
        }
    }
}

fn open_real_port(
    ports: &mut HashMap<String, ManagedPort>,
    name: &str,
    config: PortConfig,
    data_dir: &std::path::Path,
    flush: FlushSettings,
) -> Result<(), PortManagerError> {
    if ports.contains_key(name) {
        return Err(PortManagerError::AlreadyManaged(name.to_string()));
    }

    let port =
        open_serial_port_raw(name, &config).map_err(|e| PortManagerError::Serial(e.to_string()))?;

    let db_path = paths::port_db_path(data_dir, name);
    let storage = SqliteStorage::open(&db_path)
        .map(|s| Arc::new(std::sync::Mutex::new(s)))
        .map_err(|e| PortManagerError::Storage(e.to_string()))?;

    let shared = SharedSerialPort::new(port);
    let port_handle = shared.handle();

    let factory = SerialReconnectFactory {
        path: name.to_string(),
        config: config.clone(),
        port_handle: Arc::clone(&port_handle),
    };

    let reader_handle = spawn_reader_with_reconnect(shared, &storage, &config, flush, factory);

    ports.insert(
        name.to_string(),
        ManagedPort {
            reader_handle,
            storage,
            serial_port: Some(port_handle),
            config,
        },
    );
    Ok(())
}

async fn reconfigure_managed(
    ports: &mut HashMap<String, ManagedPort>,
    name: &str,
    config: PortConfig,
) -> Result<(), PortManagerError> {
    let port = ports
        .get_mut(name)
        .ok_or_else(|| PortManagerError::NotFound(name.to_string()))?;
    let handle = port
        .serial_port
        .as_ref()
        .map(Arc::clone)
        .ok_or_else(|| PortManagerError::NoHardware(name.to_string()))?;

    let mut guard = handle.lock().await;
    let result = reconfigure_serial_port(&mut guard, &config)
        .map_err(|e| PortManagerError::Serial(e.to_string()));
    drop(guard);

    if result.is_ok() {
        port.config = config;
    }
    result
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

    #[test]
    fn dial_in_nodes_are_hidden_when_their_callout_twin_exists() {
        let ports = vec![
            "/dev/cu.usbmodem1101".to_string(),
            "/dev/tty.usbmodem1101".to_string(),
            "/dev/cu.debug-console".to_string(),
            "/dev/tty.debug-console".to_string(),
            "/dev/tty.orphan".to_string(),
        ];
        let deduped = dedupe_device_nodes(&ports);

        if cfg!(target_os = "macos") {
            assert_eq!(
                deduped,
                vec![
                    "/dev/cu.usbmodem1101".to_string(),
                    "/dev/cu.debug-console".to_string(),
                    // No callout twin exists, so this one stays reachable.
                    "/dev/tty.orphan".to_string(),
                ]
            );
        } else {
            assert_eq!(deduped.len(), 5, "other platforms have no node twins");
        }
    }

    #[test]
    fn the_two_nodes_of_one_device_compare_equal() {
        assert!(same_device("/dev/cu.usbmodem1", "/dev/cu.usbmodem1"));
        assert!(!same_device("/dev/cu.usbmodem1", "/dev/cu.usbmodem2"));

        if cfg!(target_os = "macos") {
            assert!(same_device("/dev/cu.usbmodem1", "/dev/tty.usbmodem1"));
            assert_eq!(canonical_device("/dev/tty.usbmodem1"), "/dev/cu.usbmodem1");
        }
        assert_eq!(canonical_device("/dev/ttyUSB0"), "/dev/ttyUSB0");
        assert_eq!(canonical_device("COM3"), "COM3");
    }

    #[test]
    fn usb_devices_are_offered_first() {
        let sorted = sort_ports(vec![
            "/dev/cu.Bluetooth-Incoming-Port".to_string(),
            "/dev/cu.usbmodem1101".to_string(),
            "/dev/cu.debug-console".to_string(),
        ]);
        assert_eq!(sorted[0], "/dev/cu.usbmodem1101");
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
        assert_eq!(ports[0].name, "port1");
        assert_eq!(ports[1].name, "port2");
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

    #[tokio::test]
    async fn test_get_config_returns_effective_config() {
        let mgr = PortManagerHandle::new();
        let (mock, _ctrl) = mock_serial(100);
        let config = PortConfig {
            baudrate: 9600,
            ..PortConfig::default()
        };

        mgr.open("port1".into(), mock, config, test_storage())
            .await
            .unwrap();

        assert_eq!(mgr.get_config("port1").await.unwrap().baudrate, 9600);
    }

    #[tokio::test]
    async fn test_reader_does_not_spin_while_port_is_locked() {
        // Holding the port must not make the reader busy-poll. The reader is
        // expected to make no progress and the handle to stay usable.
        let mgr = PortManagerHandle::new();
        let (mock, ctrl) = mock_serial(100);
        let storage = test_storage();
        mgr.open(
            "port1".into(),
            mock,
            PortConfig::default(),
            Arc::clone(&storage),
        )
        .await
        .unwrap();

        ctrl.feed_lines(&["before"]).await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        let count = storage.lock().unwrap().line_count().unwrap();
        assert_eq!(count, 1);
    }
}
