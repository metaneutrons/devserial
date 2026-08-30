// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Terminal rendering for CLI results.

use crate::cli::CliError;
use crate::protocol::PortInfoResponse;
use crate::reader::ConnectionState;
use crate::storage::{BufferStats, StoredLine};

/// Print a stored serial line as text, timestamped text, or JSON.
pub fn print_line(line: &StoredLine, timestamps: bool, json: bool) {
    if json {
        println!("{}", serde_json::to_string(line).unwrap_or_default());
    } else if timestamps {
        let ts = chrono::DateTime::from_timestamp_nanos(line.timestamp_ns).format("%H:%M:%S%.3f");
        println!("[{ts}] {}", line.payload);
    } else {
        println!("{}", line.payload);
    }
}

/// Print managed ports and detected hardware.
///
/// # Errors
/// Returns an error if JSON serialization fails.
pub fn print_port_list(
    managed: Option<&[PortInfoResponse]>,
    hardware: &[String],
    json: bool,
) -> Result<(), CliError> {
    if json {
        let out = serde_json::json!({
            "managed": managed,
            "available_hardware": hardware,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("Managed Ports:");
    match managed {
        None => println!("  (daemon not running)"),
        Some([]) => println!("  (none)"),
        Some(ports) => {
            for port in ports {
                println!(
                    "  • {} ({}, {} lines)",
                    port.name,
                    describe_state(&port.state),
                    port.total_lines
                );
            }
        }
    }

    println!("\nAvailable Serial Hardware:");
    if hardware.is_empty() {
        println!("  (no hardware ports detected)");
    }
    for port in hardware {
        println!("  • {port}");
    }
    Ok(())
}

/// Print connection state and buffer statistics.
///
/// # Errors
/// Returns an error if JSON serialization fails.
pub fn print_status(
    port: &str,
    connection: &ConnectionState,
    stats: &BufferStats,
    json: bool,
) -> Result<(), CliError> {
    if json {
        let out = serde_json::json!({
            "port": port,
            "state": describe_state(connection),
            "total_lines": stats.total_lines,
            "total_bytes": stats.total_bytes,
            "db_size_bytes": stats.db_size_bytes,
            "last_timestamp_ns": stats.last_timestamp_ns,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("Statistics for {port}:");
    println!("  State:          {}", describe_state(connection));
    println!("  Total Lines:    {}", stats.total_lines);
    println!("  Total Bytes:    {}", stats.total_bytes);
    println!("  Database Size:  {} bytes", stats.db_size_bytes);
    if let Some(ts) = stats.last_timestamp_ns {
        let dt = chrono::DateTime::from_timestamp_nanos(ts);
        println!("  Last Activity:  {}", dt.format("%Y-%m-%d %H:%M:%S UTC"));
    }
    Ok(())
}

/// Human-readable connection state.
#[must_use]
pub fn describe_state(state: &ConnectionState) -> String {
    match state {
        ConnectionState::Connected => "connected".to_string(),
        ConnectionState::Reconnecting => "reconnecting".to_string(),
        ConnectionState::Disconnected { since_ms, attempts } => {
            format!("disconnected for {since_ms}ms after {attempts} attempts")
        }
    }
}

/// Print program, author, license and repository information.
///
/// All values come from the package manifest, so they cannot drift from it.
pub fn print_about() {
    println!("{} v{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    println!("{}", env!("CARGO_PKG_DESCRIPTION"));
    println!();
    println!("Author:     {}", authors());
    println!("License:    {}", env!("CARGO_PKG_LICENSE"));
    println!("Repository: {}", env!("CARGO_PKG_REPOSITORY"));
}

/// Package authors, with the manifest separator replaced by a readable one.
#[must_use]
pub fn authors() -> String {
    env!("CARGO_PKG_AUTHORS").replace(':', ", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_descriptions_are_readable() {
        assert_eq!(describe_state(&ConnectionState::Connected), "connected");
        let text = describe_state(&ConnectionState::Disconnected {
            since_ms: 1500,
            attempts: 3,
        });
        assert!(text.contains("1500ms"));
        assert!(text.contains("3 attempts"));
    }

    #[test]
    fn about_metadata_comes_from_the_manifest() {
        assert!(!authors().is_empty(), "Cargo.toml must declare authors");
        assert_eq!(env!("CARGO_PKG_LICENSE"), "GPL-3.0-only");
        assert!(env!("CARGO_PKG_REPOSITORY").starts_with("https://"));
    }
}
