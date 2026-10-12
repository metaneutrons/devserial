// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! MCP server execution over standard I/O.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::cli::{CliError, bootstrap};
use crate::engine::CommandEngine;
use crate::ipc::IpcClient;
use crate::paths;
use crate::server::DevSerialServer;
use crate::state::StateDb;

/// Run the MCP server until the client disconnects.
///
/// # Errors
/// Returns an error on initialization or serving failure.
pub fn run_mcp(socket: Option<PathBuf>, config_path: Option<&Path>) -> Result<(), CliError> {
    let startup = bootstrap::start(config_path, "mcp")?;
    let config = Arc::clone(&startup.config);

    startup.runtime.block_on(async move {
        use rmcp::{ServiceExt, transport::stdio};

        // The daemon is the sole serial-port owner. MCP is another client of
        // that daemon, not a second reader competing for the same hardware.
        let client = IpcClient::new(socket.unwrap_or_else(paths::default_socket_path))
            .with_config_path(config_path.map(Path::to_path_buf));
        client.ensure_daemon().await?;
        // Held for the whole MCP session (#114): an AI tool keeps its MCP
        // server running for as long as its own session lasts, and the daemon
        // stays with it. Released when the client disconnects.
        let _lease =
            crate::ipc::LeaseKeeper::start(client.clone(), crate::protocol::LeaseKind::Mcp);

        let port_manager = bootstrap::port_manager(&config);
        let state_db = Arc::new(Mutex::new(
            StateDb::open_memory().map_err(|e| CliError::msg(e.to_string()))?,
        ));
        let notice = client.build_warning().await;
        let engine = CommandEngine::new(port_manager, state_db, config).with_remote(client);
        let service = DevSerialServer::new(engine)
            .with_notice(notice)
            .serve(stdio())
            .await
            .map_err(|e| CliError::msg(format!("MCP transport error: {e}")))?;

        service
            .waiting()
            .await
            .map_err(|e| CliError::msg(format!("MCP session error: {e}")))?;
        Ok(())
    })
}
