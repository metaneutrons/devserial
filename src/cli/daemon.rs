// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Foreground execution of the background daemon.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::cli::{CliError, bootstrap};
use crate::engine::CommandEngine;
use crate::ipc::IpcServer;
use crate::paths;
use crate::state::StateDb;

/// Run the daemon until it is asked to shut down.
///
/// # Errors
/// Returns an error on initialization or endpoint failure.
pub fn run_daemon(socket: Option<PathBuf>, config_path: Option<&Path>) -> Result<(), CliError> {
    let startup = bootstrap::start(config_path, "daemon")?;
    let config = Arc::clone(&startup.config);
    let endpoint = socket.unwrap_or_else(paths::default_socket_path);
    let pid_path = paths::pid_path(&endpoint);

    startup.runtime.block_on(async move {
        let port_manager = bootstrap::port_manager(&config);
        let state_db = Arc::new(Mutex::new(
            StateDb::open(&config.global.data_dir).map_err(|e| CliError::msg(e.to_string()))?,
        ));

        let restored = bootstrap::restore_ports(&port_manager, &state_db, &config).await;
        if restored > 0 {
            tracing::info!(restored, "restored previously open ports");
        }

        let engine = CommandEngine::new(port_manager, state_db, Arc::clone(&config));
        let server = IpcServer::new(engine, endpoint, pid_path);
        let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);

        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = shutdown_tx.send(());
            }
        });

        server.run(shutdown_rx).await?;
        Ok(())
    })
}
