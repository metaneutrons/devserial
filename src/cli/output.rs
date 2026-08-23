// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! CLI output formatting utilities for text, JSON, and timestamps.

use crate::protocol::{PortInfoResponse, ResponsePayload};
use crate::storage::{BufferStats, StoredLine};
use std::path::PathBuf;

/// Print a stored serial line formatted as text, timestamped, or JSON.
pub fn print_line(l: &StoredLine, timestamps: bool, json: bool) {
    if json {
        println!("{}", serde_json::to_string(l).unwrap_or_default());
    } else if timestamps {
        let dt = chrono::DateTime::from_timestamp_nanos(l.timestamp_ns);
        println!("[{}] {}", dt.format("%H:%M:%S%.3f"), l.payload);
    } else {
        println!("{}", l.payload);
    }
}

/// Print port list (managed and detected hardware).
///
/// # Errors
/// Returns error if JSON serialization fails.
pub fn print_port_list(
    managed: Option<ResponsePayload>,
    hw_ports: &[PathBuf],
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        let out = serde_json::json!({
            "managed": managed,
            "available_hardware": hw_ports.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("Managed Ports:");
        if let Some(ResponsePayload::PortList(ports)) = managed {
            if ports.is_empty() {
                println!("  (none)");
            } else {
                for p in ports {
                    print_port_item(&p);
                }
            }
        } else {
            println!("  (daemon unreachable)");
        }

        println!("\nAvailable Serial Hardware:");
        if hw_ports.is_empty() {
            println!("  (no hardware ports detected)");
        } else {
            for p in hw_ports {
                println!("  • {}", p.display());
            }
        }
    }
    Ok(())
}

fn print_port_item(p: &PortInfoResponse) {
    println!("  • {} ({:?}, {} lines)", p.name, p.state, p.total_lines);
}

/// Print buffer statistics.
///
/// # Errors
/// Returns error if JSON serialization fails.
pub fn print_stats(
    port: &str,
    stats: &BufferStats,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        println!("Statistics for {port}:");
        println!("  Total Lines:    {}", stats.total_lines);
        println!("  Total Bytes:    {}", stats.total_bytes);
        println!("  Database Size:  {} bytes", stats.db_size_bytes);
        if let Some(ts) = stats.last_timestamp_ns {
            let dt = chrono::DateTime::from_timestamp_nanos(ts);
            println!("  Last Activity:  {}", dt.format("%Y-%m-%d %H:%M:%S UTC"));
        }
    }
    Ok(())
}
