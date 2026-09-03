// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Shared startup for the long-running roles.
//!
//! The daemon and the MCP server need the same four steps: build a runtime,
//! install logging, load the configuration and restore the ports that were open
//! before the last shutdown. That sequence exists once, here.

use std::path::Path;
use std::sync::Arc;

use crate::cli::CliError;
use crate::config::{Config, load_config};
use crate::port_manager::PortManagerHandle;
use crate::reader::FlushSettings;
use crate::state::StateDb;
use tracing_subscriber::EnvFilter;

/// Everything a long-running role needs to start.
pub struct Startup {
    /// Multi-threaded runtime owned by the role.
    pub runtime: tokio::runtime::Runtime,
    /// Effective configuration.
    pub config: Arc<Config>,
}

/// Load the configuration, install logging and build a runtime.
///
/// Logging is configured from `global.log_level`, which used to be ignored.
///
/// # Errors
/// Returns an error if the configuration is invalid or the runtime cannot be
/// created.
pub fn start(config_path: Option<&Path>, role: &str) -> Result<Startup, CliError> {
    let config = load_config(config_path)?;

    // An explicit RUST_LOG always wins; otherwise the configured level applies.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(config.global.log_directive()));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    tracing::info!(
        role,
        version = env!("CARGO_PKG_VERSION"),
        "starting devserial"
    );
    tracing::info!(
        data_dir = %config.global.data_dir.display(),
        archive_dir = %config.global.archive_dir.display(),
        "configuration loaded"
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    Ok(Startup {
        runtime,
        config: Arc::new(config),
    })
}

/// Create a port manager that honours the configured write batching.
#[must_use]
pub fn port_manager(config: &Config) -> PortManagerHandle {
    PortManagerHandle::with_flush(FlushSettings::from(&config.global))
}

/// Reopen the ports that were open when the process last stopped.
///
/// Returns the number of ports restored.
pub async fn restore_ports(
    port_manager: &PortManagerHandle,
    state_db: &Arc<std::sync::Mutex<StateDb>>,
    config: &Config,
) -> usize {
    let entries = state_db
        .lock()
        .map_or_else(|_| Vec::new(), |db| db.active_ports().unwrap_or_default());

    let mut restored = 0;
    for entry in entries {
        match port_manager
            .open_serial(
                entry.name.clone(),
                entry.config,
                config.global.data_dir.clone(),
            )
            .await
        {
            Ok(()) => {
                restored += 1;
                tracing::info!(port = %entry.name, "restored port");
            }
            Err(e) => {
                tracing::warn!(port = %entry.name, error = %e, "could not restore port");
            }
        }
    }
    restored
}
