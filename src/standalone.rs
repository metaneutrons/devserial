// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Standalone mode: GUI or TUI with a directly owned serial port.
//!
//! No daemon and no MCP server are involved. The capture database still lives
//! in the configured data directory, so a port opened here and the same port
//! opened through the daemon share one buffer.

#[cfg(any(feature = "monitor", feature = "tui"))]
use std::path::Path;
#[cfg(any(feature = "monitor", feature = "tui"))]
use std::sync::{Arc, Mutex};

#[cfg(any(feature = "monitor", feature = "tui"))]
use crate::cli::CliError;
#[cfg(any(feature = "monitor", feature = "tui"))]
use crate::config::PortConfig;
#[cfg(any(feature = "monitor", feature = "tui"))]
use crate::storage::SqliteStorage;

/// Resolve the data directory for standalone mode.
///
/// Reads the same configuration the daemon reads, so both write their capture
/// databases to the same place.
#[cfg(any(feature = "monitor", feature = "tui"))]
fn resolve_data_dir(config_path: Option<&Path>) -> std::path::PathBuf {
    match crate::config::load_config(config_path) {
        Ok(config) => config.global.data_dir,
        Err(e) => {
            tracing::warn!(error = %e, "using the default data directory");
            crate::paths::default_data_dir()
        }
    }
}

/// Data directory for sessions opened from inside the GUI.
#[cfg(feature = "monitor")]
fn gui_data_dir() -> std::path::PathBuf {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| resolve_data_dir(None)).clone()
}

/// Keeps the runtime and the reader task of a standalone session alive.
///
/// Previously the reader was kept running by an endless `sleep` loop and the
/// runtime was leaked with `std::mem::forget`, which meant no shutdown ever
/// flushed pending writes.
#[cfg(feature = "monitor")]
pub struct SessionKeepalive {
    runtime: Mutex<Option<tokio::runtime::Runtime>>,
    reader: Mutex<Option<crate::reader::PortReaderHandle>>,
}

#[cfg(feature = "monitor")]
impl SessionKeepalive {
    fn new(runtime: tokio::runtime::Runtime, reader: crate::reader::PortReaderHandle) -> Arc<Self> {
        Arc::new(Self {
            runtime: Mutex::new(Some(runtime)),
            reader: Mutex::new(Some(reader)),
        })
    }

    /// Replace the reader after a reconnect, shutting the previous one down.
    fn replace_reader(&self, reader: Option<crate::reader::PortReaderHandle>) {
        let mut guard = self
            .reader
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = reader;
    }

    /// Runtime handle for blocking calls from the GUI thread.
    fn handle(&self) -> Option<tokio::runtime::Handle> {
        let guard = self
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = guard.as_ref().map(|rt| rt.handle().clone());
        drop(guard);
        handle
    }
}

#[cfg(feature = "monitor")]
impl Drop for SessionKeepalive {
    fn drop(&mut self) {
        // Dropping the reader closes its shutdown channel, which ends the task.
        self.replace_reader(None);
        let taken = {
            let mut guard = self
                .runtime
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.take()
        };
        if let Some(runtime) = taken {
            // Shutting down in the background keeps the GUI thread responsive.
            runtime.shutdown_background();
        }
    }
}

/// Blocking writer that forwards GUI input to the serial port.
#[cfg(feature = "monitor")]
#[derive(Clone)]
pub struct TokioSerialWriter {
    handle: Arc<tokio::sync::Mutex<Option<crate::port_manager::SerialPortHandle>>>,
    runtime: tokio::runtime::Handle,
}

#[cfg(feature = "monitor")]
impl std::io::Write for TokioSerialWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let handle = Arc::clone(&self.handle);
        let data = buf.to_vec();
        self.runtime.block_on(async move {
            let guard = handle.lock().await;
            let port = match guard.as_ref() {
                Some(port) => Arc::clone(port),
                None => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotConnected,
                        "serial port is disconnected",
                    ));
                }
            };
            drop(guard);
            port.lock().await.write_all(&data).await?;
            Ok(data.len())
        })
    }

    fn flush(&mut self) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        let handle = Arc::clone(&self.handle);
        self.runtime.block_on(async move {
            let guard = handle.lock().await;
            match guard.as_ref() {
                Some(port) => port.lock().await.flush().await,
                None => Ok(()),
            }
        })
    }
}

#[cfg(feature = "monitor")]
impl TokioSerialWriter {
    /// Reconfigure the serial port in place.
    ///
    /// # Errors
    /// Returns an error if reconfiguring fails.
    pub fn reconfigure(&self, config: &PortConfig) -> Result<(), String> {
        let handle = Arc::clone(&self.handle);
        let config = config.clone();
        self.runtime.block_on(async move {
            let guard = handle.lock().await;
            match guard.as_ref() {
                Some(port) => {
                    let mut port = port.lock().await;
                    crate::port_manager::reconfigure_serial_port(&mut port, &config)
                        .map_err(|e| format!("failed to reconfigure serial port: {e}"))
                }
                None => Err("serial port is disconnected".to_string()),
            }
        })
    }
}

/// Everything one standalone session owns.
#[cfg(feature = "monitor")]
struct SessionParts {
    storage: SqliteStorage,
    shared_storage: Arc<Mutex<SqliteStorage>>,
    writer_slot: Arc<tokio::sync::Mutex<Option<crate::port_manager::SerialPortHandle>>>,
    keepalive: Arc<SessionKeepalive>,
    history: Vec<String>,
}

/// Open a port, start capturing, and return the pieces a GUI window needs.
#[cfg(feature = "monitor")]
fn start_session(port: &str, config: &PortConfig, data_dir: &Path) -> Result<SessionParts, String> {
    crate::paths::create_private_dir(data_dir)
        .map_err(|e| format!("failed to create data directory: {e}"))?;
    let db_path = crate::paths::port_db_path(data_dir, port);

    let storage = SqliteStorage::open(&db_path)
        .map_err(|e| format!("failed to open capture database: {e}"))?;
    let history = storage.load_send_history(500).unwrap_or_default();
    let shared_storage = Arc::new(Mutex::new(
        SqliteStorage::open(&db_path)
            .map_err(|e| format!("failed to open capture database: {e}"))?,
    ));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to build the async runtime: {e}"))?;

    let serial = crate::port_manager::open_serial_port_raw(port, config)
        .map_err(|e| format!("failed to open port '{port}': {e}"))?;
    let shared = crate::port_manager::SharedSerialPort::new(serial);
    let writer_slot = Arc::new(tokio::sync::Mutex::new(Some(shared.handle())));

    let reader = {
        let _guard = runtime.enter();
        crate::reader::spawn_reader(shared, &shared_storage, config)
    };

    Ok(SessionParts {
        storage,
        shared_storage,
        writer_slot,
        keepalive: SessionKeepalive::new(runtime, reader),
        history,
    })
}

/// Open a standalone monitoring session for use inside a running GUI.
///
/// # Errors
/// Returns an error if the port cannot be opened or the database fails.
#[cfg(feature = "monitor")]
pub fn open_standalone_session(
    port: &str,
    config: &PortConfig,
) -> Result<crate::monitor::PortMonitorState, String> {
    let parts = start_session(port, config, &gui_data_dir())?;
    let runtime = parts
        .keepalive
        .handle()
        .ok_or_else(|| "session runtime is gone".to_string())?;

    let writer = TokioSerialWriter {
        handle: Arc::clone(&parts.writer_slot),
        runtime,
    };

    let reconfigure: crate::monitor::ReconfigureFn = {
        let writer = writer.clone();
        Arc::new(move |config: &PortConfig| writer.reconfigure(config))
    };

    let toggle: crate::monitor::ToggleConnectFn = {
        let port = port.to_string();
        let slot = Arc::clone(&parts.writer_slot);
        let storage = Arc::clone(&parts.shared_storage);
        let keepalive = Arc::clone(&parts.keepalive);
        Arc::new(move |connect: bool, config: &PortConfig| {
            let runtime = keepalive
                .handle()
                .ok_or_else(|| "session runtime is gone".to_string())?;
            if !connect {
                keepalive.replace_reader(None);
                runtime.block_on(async {
                    *slot.lock().await = None;
                });
                return Ok(());
            }

            let serial = crate::port_manager::open_serial_port_raw(&port, config)
                .map_err(|e| format!("failed to open port '{port}': {e}"))?;
            let shared = crate::port_manager::SharedSerialPort::new(serial);
            let handle = shared.handle();
            runtime.block_on(async {
                *slot.lock().await = Some(handle);
            });
            let reader = {
                let _guard = runtime.enter();
                crate::reader::spawn_reader(shared, &storage, config)
            };
            keepalive.replace_reader(Some(reader));
            Ok(())
        })
    };

    Ok(crate::monitor::PortMonitorState::new_with_reconfigure(
        port.to_string(),
        config.framing_summary(),
        parts.storage,
        parts.history,
        Some(Arc::new(Mutex::new(
            Box::new(writer) as Box<dyn std::io::Write + Send>
        ))),
        Some(reconfigure),
        Some(config.clone()),
        Some(toggle),
        Some(parts.keepalive),
    ))
}

/// Open a serial port and launch the GUI monitor for it.
///
/// # Errors
/// Returns an error if the port cannot be opened or the window fails.
#[cfg(feature = "monitor")]
pub fn run_monitor_standalone(
    port: &str,
    config: &PortConfig,
    config_path: Option<&Path>,
) -> Result<(), CliError> {
    let data_dir = resolve_data_dir(config_path);

    // Hand over to an already running GUI before touching the hardware. The
    // receiving instance opens the port itself, so this process does not have
    // to stay alive holding a port it cannot show.
    let request = crate::gui_ipc::OpenPortRequest {
        port_name: port.to_string(),
        db_path: crate::paths::port_db_path(&data_dir, port),
        port_info: config.framing_summary(),
        settings: Some(config_settings(config)),
    };
    match crate::gui_ipc::try_open_in_existing_gui(&request) {
        Ok(true) => {
            println!("Opened {port} in the running devserial window.");
            return Ok(());
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %e, "could not reach the running GUI instance"),
    }

    let parts = start_session(port, config, &data_dir).map_err(CliError::msg)?;
    let runtime = parts
        .keepalive
        .handle()
        .ok_or_else(|| CliError::msg("session runtime is gone"))?;
    let writer = TokioSerialWriter {
        handle: Arc::clone(&parts.writer_slot),
        runtime,
    };

    let db_path = crate::paths::port_db_path(&data_dir, port);
    let info = config.framing_summary();
    let result = crate::monitor::run_monitor_with_port(
        port,
        &db_path,
        &info,
        Box::new(writer),
        Arc::clone(&parts.keepalive),
    );

    match result {
        Ok(()) => Ok(()),
        Err(gui_error) => {
            #[cfg(feature = "tui")]
            {
                eprintln!("GUI unavailable ({gui_error}), falling back to the TUI");
                let handle = parts
                    .keepalive
                    .handle()
                    .ok_or_else(|| CliError::msg("session runtime is gone"))?;
                let port_handle = {
                    let slot = Arc::clone(&parts.writer_slot);
                    handle.block_on(async move { slot.lock().await.clone() })
                };
                match port_handle {
                    Some(port_handle) => crate::tui::run_tui(
                        port,
                        config,
                        &parts.shared_storage,
                        &port_handle,
                        &handle,
                    ),
                    None => Err(CliError::msg("serial port is disconnected")),
                }
            }
            #[cfg(not(feature = "tui"))]
            Err(CliError::msg(gui_error))
        }
    }
}

/// Launch the GUI without opening a port up front.
///
/// # Errors
/// Returns an error if the GUI cannot start.
#[cfg(feature = "monitor")]
pub fn run_monitor_gui_app() -> Result<(), CliError> {
    crate::monitor::run_monitor_gui().map_err(CliError::msg)
}

/// Open a serial port and launch the TUI monitor.
///
/// # Errors
/// Returns an error if the port cannot be opened or the TUI fails.
#[cfg(feature = "tui")]
pub fn run_tui_standalone(
    port: &str,
    config: &PortConfig,
    config_path: Option<&Path>,
) -> Result<(), CliError> {
    let data_dir = resolve_data_dir(config_path);
    crate::paths::create_private_dir(&data_dir)?;
    let db_path = crate::paths::port_db_path(&data_dir, port);

    let storage = Arc::new(Mutex::new(
        SqliteStorage::open(&db_path).map_err(|e| CliError::msg(e.to_string()))?,
    ));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let serial = crate::port_manager::open_serial_port_raw(port, config)?;
    let shared = crate::port_manager::SharedSerialPort::new(serial);
    let port_handle = shared.handle();

    // The handle is kept in scope for the whole session, which is what keeps
    // the reader task running.
    let _reader = {
        let _guard = runtime.enter();
        crate::reader::spawn_reader(shared, &storage, config)
    };

    crate::tui::run_tui(port, config, &storage, &port_handle, runtime.handle())
}

/// Protocol settings for a port configuration.
#[cfg(feature = "monitor")]
const fn config_settings(config: &PortConfig) -> crate::protocol::PortSettings {
    crate::protocol::PortSettings {
        baudrate: Some(config.baudrate),
        data_bits: Some(config.data_bits),
        parity: Some(config.parity),
        stop_bits: Some(config.stop_bits),
        flow_control: Some(config.flow_control),
    }
}

#[cfg(all(test, feature = "monitor"))]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_through_the_protocol() {
        let config = PortConfig {
            baudrate: 9600,
            parity: crate::serial_params::Parity::Even,
            ..PortConfig::default()
        };
        let settings = config_settings(&config);
        assert_eq!(settings.baudrate, Some(9600));
        assert_eq!(settings.parity, Some(crate::serial_params::Parity::Even));
    }

    #[test]
    fn data_dir_follows_the_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let config_file = dir.path().join("devserial.toml");
        let data_dir = dir.path().join("captures");
        std::fs::write(
            &config_file,
            format!("[global]\ndata_dir = \"{}\"\n", data_dir.display()),
        )
        .unwrap();

        assert_eq!(resolve_data_dir(Some(&config_file)), data_dir);
    }
}
