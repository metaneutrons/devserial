// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! MCP server execution service (stdio transport).

use crate::config::load_config;
use crate::port_manager::PortManagerHandle;
use crate::state::StateDb;

/// Run the MCP server over standard I/O until connection closes.
///
/// # Errors
/// Returns error on initialization or serving failure.
pub fn run_mcp() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            use rmcp::{ServiceExt, transport::stdio};
            use tracing_subscriber::EnvFilter;

            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()),
                )
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .init();

            tracing::info!("Starting DevSerial MCP Server");

            let config = load_config(None).unwrap_or_default();
            tracing::info!(data_dir = %config.global.data_dir.display(), "config loaded");

            let port_manager = PortManagerHandle::new();

            // Restore previously open ports
            let restored_ports = StateDb::open(&config.global.data_dir).map_or_else(
                |_| Vec::new(),
                |state_db| state_db.active_ports().unwrap_or_default(),
            );

            for entry in restored_ports {
                let data_dir = config.global.data_dir.clone();
                match port_manager
                    .open_serial(entry.name.clone(), entry.config.clone(), data_dir)
                    .await
                {
                    Ok(()) => tracing::info!(port = %entry.name, "restored port"),
                    Err(e) => {
                        tracing::warn!(port = %entry.name, error = %e, "failed to restore port (will retry on next start)");
                    }
                }
            }

            let server = crate::server::DevSerialServer::new(port_manager, config);
            let service = server.serve(stdio()).await.inspect_err(|e| {
                tracing::error!("serving error: {:?}", e);
            })?;

            service.waiting().await?;
            Ok(())
        })
}
