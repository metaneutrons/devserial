// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! MCP server execution over standard I/O.

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::cli::{CliError, bootstrap};
use crate::engine::CommandEngine;
use crate::server::DevSerialServer;
use crate::state::StateDb;

/// Run the MCP server until the client disconnects.
///
/// # Errors
/// Returns an error on initialization or serving failure.
pub fn run_mcp(config_path: Option<&Path>) -> Result<(), CliError> {
    let startup = bootstrap::start(config_path, "mcp")?;
    let config = Arc::clone(&startup.config);

    startup.runtime.block_on(async move {
        use rmcp::{ServiceExt, transport::stdio};

        let port_manager = bootstrap::port_manager(&config);
        let state_db = Arc::new(Mutex::new(
            StateDb::open(&config.global.data_dir).map_err(|e| CliError::msg(e.to_string()))?,
        ));

        let restored = bootstrap::restore_ports(&port_manager, &state_db, &config).await;
        if restored > 0 {
            tracing::info!(restored, "restored previously open ports");
        }

        let engine = CommandEngine::new(port_manager, state_db, config);
        let service = DevSerialServer::new(engine)
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
