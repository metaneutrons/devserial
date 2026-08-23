// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Daemon foreground execution and port restoration service.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::config::load_config;
use crate::engine::CommandEngine;
use crate::ipc::{IpcServer, default_pid_path};
use crate::port_manager::PortManagerHandle;
use crate::state::StateDb;

/// Run the background daemon process until shutdown signal.
///
/// # Errors
/// Returns error on initialization or socket failure.
pub fn run_daemon(socket_path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            use tracing_subscriber::EnvFilter;

            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()),
                )
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .init();

            tracing::info!("Starting DevSerial Background Daemon");

            let config = load_config(None).unwrap_or_default();
            tracing::info!(data_dir = %config.global.data_dir.display(), "config loaded");

            let port_manager = PortManagerHandle::new();
            let state_db = Arc::new(Mutex::new(StateDb::open(&config.global.data_dir)?));

            // Restore previously open ports
            let restored_ports = state_db
                .lock()
                .map_or_else(|_| Vec::new(), |db| db.active_ports().unwrap_or_default());

            for entry in restored_ports {
                let data_dir = config.global.data_dir.clone();
                match port_manager
                    .open_serial(entry.name.clone(), entry.config.clone(), data_dir)
                    .await
                {
                    Ok(()) => tracing::info!(port = %entry.name, "restored port"),
                    Err(e) => {
                        tracing::warn!(port = %entry.name, error = %e, "failed to restore port");
                    }
                }
            }

            let pid_path = default_pid_path();
            let engine = CommandEngine::new(
                port_manager,
                state_db,
                Arc::new(config.clone()),
                config.global.data_dir.clone(),
            );

            let server = IpcServer::new(engine, socket_path, pid_path);
            let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);

            // Handle Ctrl+C
            tokio::spawn(async move {
                tokio::signal::ctrl_c().await.ok();
                let _ = shutdown_tx.send(());
            });

            server.run(shutdown_rx).await?;
            Ok(())
        })
}
