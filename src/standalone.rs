// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Standalone mode entry points (no MCP server needed).

#[cfg(any(feature = "monitor", feature = "tui"))]
use crate::config::PortConfig;

#[cfg(feature = "monitor")]
struct TokioSerialWriter {
    handle: crate::port_manager::SerialPortHandle,
    rt: tokio::runtime::Handle,
}

#[cfg(feature = "monitor")]
impl std::io::Write for TokioSerialWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let handle = std::sync::Arc::clone(&self.handle);
        let data = buf.to_vec();
        self.rt.block_on(async move {
            let guard = handle.lock().await;
            guard.write_all(&data).await?;
            drop(guard);
            Ok(data.len())
        })
    }

    fn flush(&mut self) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        let handle = std::sync::Arc::clone(&self.handle);
        self.rt.block_on(async move {
            let mut guard = handle.lock().await;
            guard.flush().await
        })
    }
}

/// Open a serial port directly and launch the GUI monitor.
///
/// # Errors
/// Returns error if the port cannot be opened or the window fails.
#[cfg(feature = "monitor")]
pub fn run_monitor_standalone(port: &str, baud: u32) -> Result<(), Box<dyn std::error::Error>> {
    let config = PortConfig {
        baudrate: baud,
        ..PortConfig::default()
    };

    // Create a temp DB for the standalone session
    let data_dir = state_dir();
    std::fs::create_dir_all(&data_dir)?;
    let sanitized = port.replace(['/', '\\'], "_");
    let db_path = data_dir.join(format!("{sanitized}.db"));

    let storage = crate::storage::SqliteStorage::open(&db_path)?;

    // Spawn a reader in a background thread to feed the DB
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _guard = rt.enter();

    let storage_arc = std::sync::Arc::new(std::sync::Mutex::new(storage));
    let storage_clone = std::sync::Arc::clone(&storage_arc);

    // Open port exactly once
    let serial_port = crate::port_manager::open_serial_port_raw(port, &config)?;
    let shared = crate::port_manager::SharedSerialPort::new(serial_port);
    let write_handle = shared.handle();

    let shared_reader = shared;
    let config_clone = config;

    rt.spawn(async move {
        crate::reader::spawn_reader(shared_reader, &storage_clone, &config_clone);
        // Keep runtime alive
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });

    let info = format!("{baud} 8N1");

    let writer = TokioSerialWriter {
        handle: std::sync::Arc::clone(&write_handle),
        rt: rt.handle().clone(),
    };

    // Try GUI first, fall back to TUI if no display available
    match crate::monitor::run_monitor_with_port(port, &db_path, &info, Box::new(writer)) {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("GUI failed ({e}), falling back to TUI...");
            #[cfg(feature = "tui")]
            {
                crate::tui::run_tui(port, baud, &storage_arc, &write_handle, &rt)
            }
            #[cfg(not(feature = "tui"))]
            Err(e.into())
        }
    }
}

/// Open a serial port directly and launch the TUI monitor.
///
/// # Errors
/// Returns error if the port cannot be opened or the TUI fails.
#[cfg(feature = "tui")]
pub fn run_tui_standalone(port: &str, baud: u32) -> Result<(), Box<dyn std::error::Error>> {
    let config = PortConfig {
        baudrate: baud,
        ..PortConfig::default()
    };

    let data_dir = state_dir();
    std::fs::create_dir_all(&data_dir)?;
    let sanitized = port.replace(['/', '\\'], "_");
    let db_path = data_dir.join(format!("{sanitized}.db"));

    let storage = crate::storage::SqliteStorage::open(&db_path)?;
    let storage_arc = std::sync::Arc::new(std::sync::Mutex::new(storage));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _guard = rt.enter();

    // Open port exactly once and share between reader and writer
    let serial_port = crate::port_manager::open_serial_port_raw(port, &config)?;
    let shared = crate::port_manager::SharedSerialPort::new(serial_port);
    let write_handle = shared.handle();

    rt.spawn({
        let storage_clone = std::sync::Arc::clone(&storage_arc);
        async move {
            crate::reader::spawn_reader(shared, &storage_clone, &config);
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        }
    });

    crate::tui::run_tui(port, baud, &storage_arc, &write_handle, &rt)
}

/// Platform-specific state directory.
#[cfg(any(feature = "monitor", feature = "tui"))]
fn state_dir() -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map_or_else(
            || std::path::PathBuf::from("./data"),
            |h| std::path::PathBuf::from(h).join("Library/Application Support/devserial"),
        )
    }
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("XDG_STATE_HOME")
            .map(|h| std::path::PathBuf::from(h).join("devserial"))
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|h| std::path::PathBuf::from(h).join(".local/state/devserial"))
            })
            .unwrap_or_else(|| std::path::PathBuf::from("./data"))
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|h| std::path::PathBuf::from(h).join("devserial"))
            .unwrap_or_else(|| std::path::PathBuf::from("./data"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        std::path::PathBuf::from("./data")
    }
}
