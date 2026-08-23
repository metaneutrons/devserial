// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Handlers for individual CLI subcommands communicating with daemon via IPC.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use crate::cli::output::{print_line, print_port_list, print_stats};
use crate::ipc::IpcClient;
use crate::modem::FileTransferProtocol;
use crate::protocol::{RequestPayload, ResponsePayload};

/// Handle daemon control flags (--status, --stop).
///
/// # Errors
/// Returns error on communication failure.
pub fn handle_daemon_control(
    client: &IpcClient,
    socket_path: &Path,
    status: bool,
    stop: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    if status {
        let alive = rt.block_on(client.is_alive());
        if alive {
            println!("Daemon is RUNNING (socket: {})", socket_path.display());
        } else {
            println!("Daemon is STOPPED (socket: {})", socket_path.display());
        }
        return Ok(());
    }

    if stop {
        let res = rt.block_on(client.send(RequestPayload::Shutdown));
        match res {
            Ok(_) => println!("Daemon shutdown command sent."),
            Err(e) => eprintln!("Failed to stop daemon: {e}"),
        }
    }
    Ok(())
}

/// Handle `list` subcommand.
///
/// # Errors
/// Returns error on communication or formatting failure.
pub fn handle_list(client: &IpcClient, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let _ = rt.block_on(client.ensure_daemon());

    let managed = rt.block_on(client.send(RequestPayload::ListPorts)).ok();
    let hw_ports = serial2_tokio::SerialPort::available_ports().unwrap_or_default();
    print_port_list(managed, &hw_ports, json)
}

/// Handle `open` subcommand.
///
/// # Errors
/// Returns error on connection or daemon failure.
#[allow(clippy::too_many_arguments)]
pub fn handle_open(
    client: &IpcClient,
    port: &str,
    baud: u32,
    data_bits: u8,
    parity: &str,
    stop_bits: u8,
    flow_control: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(client.ensure_daemon())
        .map_err(|e| format!("daemon start error: {e}"))?;

    let req = RequestPayload::OpenPort {
        name: port.to_string(),
        baudrate: Some(baud),
        data_bits: Some(data_bits),
        parity: Some(parity.to_string()),
        stop_bits: Some(stop_bits),
        flow_control: Some(flow_control.to_string()),
    };

    match rt.block_on(client.send(req)) {
        Ok(ResponsePayload::PortOpened {
            name,
            config_summary,
        }) => {
            println!("Opened {name} ({config_summary})");
            Ok(())
        }
        Ok(other) => {
            println!("{other:?}");
            Ok(())
        }
        Err(e) => {
            eprintln!("Error opening port: {e}");
            std::process::exit(1);
        }
    }
}

/// Handle `close` subcommand.
///
/// # Errors
/// Returns error on connection or daemon failure.
pub fn handle_close(client: &IpcClient, port: &str) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(client.ensure_daemon())
        .map_err(|e| format!("daemon start error: {e}"))?;

    match rt.block_on(client.send(RequestPayload::ClosePort {
        name: port.to_string(),
    })) {
        Ok(ResponsePayload::PortClosed { name }) => {
            println!("Closed {name}");
            Ok(())
        }
        Ok(other) => {
            println!("{other:?}");
            Ok(())
        }
        Err(e) => {
            eprintln!("Error closing port: {e}");
            std::process::exit(1);
        }
    }
}

/// Handle `read` subcommand.
///
/// # Errors
/// Returns error on connection or daemon failure.
#[allow(clippy::too_many_arguments)]
pub fn handle_read(
    client: &IpcClient,
    port: &str,
    tail: Option<u32>,
    from: Option<i64>,
    limit: u32,
    follow: bool,
    timestamps: bool,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(client.ensure_daemon())
        .map_err(|e| format!("daemon start error: {e}"))?;

    if follow {
        let mut last_id = from.unwrap_or(0);
        if last_id == 0 {
            let req = RequestPayload::ReadLines {
                port: port.to_string(),
                start_id: None,
                count: None,
                tail: Some(tail.unwrap_or(20)),
            };
            if let Ok(ResponsePayload::Lines(lines)) = rt.block_on(client.send(req)) {
                for l in lines {
                    print_line(&l, timestamps, json);
                    last_id = last_id.max(l.id);
                }
            }
        }

        loop {
            std::thread::sleep(Duration::from_millis(50));
            let req = RequestPayload::ReadLines {
                port: port.to_string(),
                start_id: Some(last_id + 1),
                count: Some(500),
                tail: None,
            };
            if let Ok(ResponsePayload::Lines(lines)) = rt.block_on(client.send(req)) {
                for l in lines {
                    print_line(&l, timestamps, json);
                    last_id = last_id.max(l.id);
                }
            }
        }
    } else {
        let req = RequestPayload::ReadLines {
            port: port.to_string(),
            start_id: from,
            count: Some(limit),
            tail,
        };
        match rt.block_on(client.send(req)) {
            Ok(ResponsePayload::Lines(lines)) => {
                for l in lines {
                    print_line(&l, timestamps, json);
                }
                Ok(())
            }
            Ok(other) => {
                println!("{other:?}");
                Ok(())
            }
            Err(e) => {
                eprintln!("Error reading port: {e}");
                std::process::exit(1);
            }
        }
    }
}

/// Handle `write` subcommand.
///
/// # Errors
/// Returns error on reading input or communication failure.
pub fn handle_write(
    client: &IpcClient,
    port: &str,
    data: Option<String>,
    hex: bool,
    newline: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut payload = match data {
        Some(d) if d == "-" => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        }
        Some(d) => d,
        None => {
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                eprintln!("Error: no data provided to write. Pass as argument or pipe via stdin.");
                std::process::exit(1);
            }
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        }
    };

    if newline && !hex {
        payload.push_str("\r\n");
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(client.ensure_daemon())
        .map_err(|e| format!("daemon start error: {e}"))?;

    let req = RequestPayload::WriteData {
        port: port.to_string(),
        data: payload,
        is_hex: hex,
    };

    match rt.block_on(client.send(req)) {
        Ok(ResponsePayload::WriteSuccess { bytes_written }) => {
            println!("Wrote {bytes_written} bytes to {port}");
            Ok(())
        }
        Ok(other) => {
            println!("{other:?}");
            Ok(())
        }
        Err(e) => {
            eprintln!("Error writing to port: {e}");
            std::process::exit(1);
        }
    }
}

/// Handle `break` subcommand.
///
/// # Errors
/// Returns error on communication failure.
pub fn handle_break(
    client: &IpcClient,
    port: &str,
    duration_ms: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(client.ensure_daemon())
        .map_err(|e| format!("daemon start error: {e}"))?;

    let req = RequestPayload::SendBreak {
        port: port.to_string(),
        duration_ms,
    };

    match rt.block_on(client.send(req)) {
        Ok(ResponsePayload::BreakSuccess) => {
            println!(
                "Sent serial BREAK pulse ({}ms) on {port}",
                duration_ms.unwrap_or(250)
            );
            Ok(())
        }
        Ok(other) => {
            println!("{other:?}");
            Ok(())
        }
        Err(e) => {
            eprintln!("Error sending serial break: {e}");
            std::process::exit(1);
        }
    }
}

/// Handle `send` subcommand (X/Y/ZMODEM).
///
/// # Errors
/// Returns error on transmission failure.
pub fn handle_send_file(
    client: &IpcClient,
    port: &str,
    file_path: &str,
    protocol: Option<FileTransferProtocol>,
) -> Result<(), Box<dyn std::error::Error>> {
    let proto = protocol.unwrap_or(FileTransferProtocol::Zmodem);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(client.ensure_daemon())
        .map_err(|e| format!("daemon start error: {e}"))?;

    println!("Sending '{file_path}' to {port} using {proto:?}...");

    let req = RequestPayload::SendFile {
        port: port.to_string(),
        file_path: file_path.to_string(),
        protocol: proto,
    };

    match rt.block_on(client.send(req)) {
        Ok(ResponsePayload::TransferSuccess {
            bytes_transferred,
            file_name,
            protocol,
        }) => {
            println!("Successfully sent {bytes_transferred} bytes ({file_name}) via {protocol:?}");
            Ok(())
        }
        Ok(other) => {
            println!("{other:?}");
            Ok(())
        }
        Err(e) => {
            eprintln!("Error sending file: {e}");
            std::process::exit(1);
        }
    }
}

/// Handle `recv` subcommand (X/Y/ZMODEM).
///
/// # Errors
/// Returns error on transmission failure.
pub fn handle_recv_file(
    client: &IpcClient,
    port: &str,
    output_dir: &str,
    protocol: Option<FileTransferProtocol>,
) -> Result<(), Box<dyn std::error::Error>> {
    let proto = protocol.unwrap_or(FileTransferProtocol::Zmodem);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(client.ensure_daemon())
        .map_err(|e| format!("daemon start error: {e}"))?;

    println!("Waiting to receive file from {port} into '{output_dir}' using {proto:?}...");

    let req = RequestPayload::ReceiveFile {
        port: port.to_string(),
        output_dir: output_dir.to_string(),
        protocol: proto,
    };

    match rt.block_on(client.send(req)) {
        Ok(ResponsePayload::TransferSuccess {
            bytes_transferred,
            file_name,
            protocol,
        }) => {
            println!(
                "Successfully received {bytes_transferred} bytes as '{file_name}' via {protocol:?}"
            );
            Ok(())
        }
        Ok(other) => {
            println!("{other:?}");
            Ok(())
        }
        Err(e) => {
            eprintln!("Error receiving file: {e}");
            std::process::exit(1);
        }
    }
}

/// Handle `signal` subcommand.
///
/// # Errors
/// Returns error on communication failure.
pub fn handle_signal(
    client: &IpcClient,
    port: &str,
    dtr: Option<bool>,
    rts: Option<bool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(client.ensure_daemon())
        .map_err(|e| format!("daemon start error: {e}"))?;

    let req = RequestPayload::SetSignal {
        port: port.to_string(),
        dtr,
        rts,
    };

    match rt.block_on(client.send(req)) {
        Ok(ResponsePayload::SignalSuccess) => {
            println!("Signal updated on {port}");
            Ok(())
        }
        Ok(other) => {
            println!("{other:?}");
            Ok(())
        }
        Err(e) => {
            eprintln!("Error setting signals: {e}");
            std::process::exit(1);
        }
    }
}

/// Handle `macro` subcommand.
///
/// # Errors
/// Returns error on communication failure.
pub fn handle_macro(
    client: &IpcClient,
    port: &str,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(client.ensure_daemon())
        .map_err(|e| format!("daemon start error: {e}"))?;

    let req = RequestPayload::ExecuteMacro {
        port: port.to_string(),
        macro_name: name.to_string(),
    };

    match rt.block_on(client.send(req)) {
        Ok(ResponsePayload::MacroSuccess { executed_steps }) => {
            println!(
                "Executed macro '{name}' on {port} ({} steps: {})",
                executed_steps.len(),
                executed_steps.join(" → ")
            );
            Ok(())
        }
        Ok(other) => {
            println!("{other:?}");
            Ok(())
        }
        Err(e) => {
            eprintln!("Error executing macro: {e}");
            std::process::exit(1);
        }
    }
}

/// Handle `search` subcommand.
///
/// # Errors
/// Returns error on search or formatting failure.
pub fn handle_search(
    client: &IpcClient,
    port: &str,
    query: &str,
    regex: bool,
    limit: u32,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(client.ensure_daemon())
        .map_err(|e| format!("daemon start error: {e}"))?;

    let req = RequestPayload::Search {
        port: port.to_string(),
        query: query.to_string(),
        is_regex: regex,
        start_ns: None,
        end_ns: None,
        limit: Some(limit),
    };

    match rt.block_on(client.send(req)) {
        Ok(ResponsePayload::SearchResults(results)) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                println!("Found {} matching lines:", results.len());
                for r in results {
                    println!("  [{}] {}", r.id, r.payload);
                }
            }
            Ok(())
        }
        Ok(other) => {
            println!("{other:?}");
            Ok(())
        }
        Err(e) => {
            eprintln!("Error searching: {e}");
            std::process::exit(1);
        }
    }
}

/// Handle `export` subcommand.
///
/// # Errors
/// Returns error on export execution.
pub fn handle_export(
    client: &IpcClient,
    port: &str,
    output: &str,
    format: &str,
    start: Option<i64>,
    end: Option<i64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(client.ensure_daemon())
        .map_err(|e| format!("daemon start error: {e}"))?;

    let req = RequestPayload::Export {
        port: port.to_string(),
        output_path: output.to_string(),
        file_format: Some(format.to_string()),
        start_line: start,
        end_line: end,
    };

    match rt.block_on(client.send(req)) {
        Ok(ResponsePayload::ExportSuccess {
            lines_exported,
            path,
        }) => {
            println!("Exported {lines_exported} lines to {path}");
            Ok(())
        }
        Ok(other) => {
            println!("{other:?}");
            Ok(())
        }
        Err(e) => {
            eprintln!("Error exporting: {e}");
            std::process::exit(1);
        }
    }
}

/// Handle `clear` subcommand.
///
/// # Errors
/// Returns error on clear execution.
pub fn handle_clear(
    client: &IpcClient,
    port: &str,
    archive: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(client.ensure_daemon())
        .map_err(|e| format!("daemon start error: {e}"))?;

    let req = RequestPayload::Clear {
        port: port.to_string(),
        archive_current: Some(archive),
    };

    match rt.block_on(client.send(req)) {
        Ok(ResponsePayload::ClearSuccess {
            lines_cleared,
            archive_path,
        }) => {
            if let Some(arch) = archive_path {
                println!("Cleared buffer on {port} ({lines_cleared} lines archived to {arch})");
            } else {
                println!("Cleared buffer on {port} ({lines_cleared} lines removed)");
            }
            Ok(())
        }
        Ok(other) => {
            println!("{other:?}");
            Ok(())
        }
        Err(e) => {
            eprintln!("Error clearing buffer: {e}");
            std::process::exit(1);
        }
    }
}

/// Handle `stats` subcommand.
///
/// # Errors
/// Returns error on statistics retrieval or formatting.
pub fn handle_stats(
    client: &IpcClient,
    port: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(client.ensure_daemon())
        .map_err(|e| format!("daemon start error: {e}"))?;

    let req = RequestPayload::GetStats {
        port: port.to_string(),
    };

    match rt.block_on(client.send(req)) {
        Ok(ResponsePayload::Stats(stats)) => print_stats(port, &stats, json),
        Ok(other) => {
            println!("{other:?}");
            Ok(())
        }
        Err(e) => {
            eprintln!("Error getting stats: {e}");
            std::process::exit(1);
        }
    }
}

/// Handle `flash` subcommand.
///
/// # Errors
/// Returns error on flashing failure.
#[cfg(feature = "esp")]
pub fn handle_flash(
    client: &IpcClient,
    port: &str,
    firmware: &str,
    baud: Option<u32>,
    monitor: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let _ = rt.block_on(client.ensure_daemon());

    println!("Flashing firmware {firmware} to {port}...");
    let req = RequestPayload::EspFlash {
        port: port.to_string(),
        firmware_path: firmware.to_string(),
        baud,
    };

    match rt.block_on(client.send(req)) {
        Ok(ResponsePayload::EspSuccess(output)) => {
            print!("{output}");
            if monitor {
                println!("\n━━━ Switching to live monitor (Ctrl+C to exit) ━━━\n");
                let mut last_id = 0;
                loop {
                    std::thread::sleep(Duration::from_millis(50));
                    let read_req = RequestPayload::ReadLines {
                        port: port.to_string(),
                        start_id: Some(last_id + 1),
                        count: Some(500),
                        tail: None,
                    };
                    if let Ok(ResponsePayload::Lines(lines)) = rt.block_on(client.send(read_req)) {
                        for l in lines {
                            println!("{}", l.payload);
                            last_id = last_id.max(l.id);
                        }
                    }
                }
            }
            Ok(())
        }
        Ok(other) => {
            println!("{other:?}");
            Ok(())
        }
        Err(e) => {
            eprintln!("Flash Error: {e}");
            std::process::exit(1);
        }
    }
}
