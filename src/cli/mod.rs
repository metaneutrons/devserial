// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Modular CLI definitions, parser, and router.

pub mod daemon;
pub mod handlers;
pub mod mcp;
pub mod output;

use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::path::PathBuf;

use crate::ipc::{IpcClient, default_socket_path};
use crate::modem::FileTransferProtocol;

/// `DevSerial` — Serial hardware bridge for LLMs, daemons, and developers.
#[derive(Parser)]
#[command(name = "devserial", version, about)]
pub struct Cli {
    /// Custom IPC socket path
    #[arg(long, global = true)]
    pub socket: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run as MCP server (stdio transport) — default when stdin is piped
    Mcp,

    /// Start or manage the background daemon service
    Daemon {
        /// Check if daemon is currently running
        #[arg(long)]
        status: bool,

        /// Stop a running daemon
        #[arg(long)]
        stop: bool,
    },

    /// List managed ports and detected serial hardware
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Open a serial port on the daemon
    Open {
        /// Serial port path (e.g. /dev/ttyUSB0, COM3)
        port: String,
        /// Baud rate
        #[arg(short, long, default_value = "115200")]
        baud: u32,
        /// Data bits (5, 6, 7, 8)
        #[arg(long, default_value = "8")]
        data_bits: u8,
        /// Parity (none, odd, even)
        #[arg(long, default_value = "none")]
        parity: String,
        /// Stop bits (1, 2)
        #[arg(long, default_value = "1")]
        stop_bits: u8,
        /// Flow control (none, software, hardware)
        #[arg(long, default_value = "none")]
        flow_control: String,
    },

    /// Close a managed serial port
    Close {
        /// Serial port path
        port: String,
    },

    /// Read lines from a serial port's buffer
    Read {
        /// Serial port path
        port: String,
        /// Number of recent lines to read (from end)
        #[arg(short, long)]
        tail: Option<u32>,
        /// Starting line ID (1-based)
        #[arg(long)]
        from: Option<i64>,
        /// Maximum number of lines to read
        #[arg(short, long, default_value = "100")]
        limit: u32,
        /// Continuously follow and stream new lines
        #[arg(short, long)]
        follow: bool,
        /// Include nanosecond timestamps in output
        #[arg(long)]
        timestamps: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Write data to a serial port (reads from stdin if data is omitted or '-')
    Write {
        /// Serial port path
        port: String,
        /// Data string to send (reads from stdin if omitted or '-')
        data: Option<String>,
        /// Treat data as hex-encoded bytes (e.g. "0x0D0A" or "414243")
        #[arg(long)]
        hex: bool,
        /// Append newline (\r\n) to data
        #[arg(short, long)]
        newline: bool,
    },

    /// Send an RS-232 serial BREAK signal (TX low pulse)
    Break {
        /// Serial port path
        port: String,
        /// Duration of BREAK pulse in milliseconds (default: 250)
        #[arg(short, long, default_value = "250")]
        duration_ms: u64,
    },

    /// Send a file via modem protocol (XMODEM, YMODEM, ZMODEM)
    Send {
        /// Serial port path
        port: String,
        /// Path to file to send
        file: String,
        /// Protocol to use (default: zmodem)
        #[arg(short, long, value_enum)]
        protocol: Option<FileTransferProtocol>,
    },

    /// Receive a file via modem protocol (XMODEM, YMODEM, ZMODEM)
    Recv {
        /// Serial port path
        port: String,
        /// Output directory to save received file(s)
        #[arg(short, long, default_value = ".")]
        output: String,
        /// Protocol to use (default: zmodem)
        #[arg(short, long, value_enum)]
        protocol: Option<FileTransferProtocol>,
    },

    /// Set DTR and/or RTS control lines
    Signal {
        /// Serial port path
        port: String,
        /// Set DTR line (true = HIGH, false = LOW)
        #[arg(long)]
        dtr: Option<bool>,
        /// Set RTS line (true = HIGH, false = LOW)
        #[arg(long)]
        rts: Option<bool>,
    },

    /// Execute a macro sequence on a port (e.g. 'reset', '`enter_bootloader`')
    Macro {
        /// Serial port path
        port: String,
        /// Macro name
        name: String,
    },

    /// Search buffered serial data (substring or regex)
    Search {
        /// Serial port path
        port: String,
        /// Search pattern
        query: String,
        /// Use regular expression search
        #[arg(short, long)]
        regex: bool,
        /// Maximum results to return
        #[arg(short, long, default_value = "100")]
        limit: u32,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Export buffered lines to a file (txt, csv, jsonl)
    Export {
        /// Serial port path
        port: String,
        /// Output file path
        output: String,
        /// File format: txt, csv, or jsonl
        #[arg(short, long, default_value = "txt")]
        format: String,
        /// Starting line ID
        #[arg(long)]
        start: Option<i64>,
        /// Ending line ID
        #[arg(long)]
        end: Option<i64>,
    },

    /// Clear buffered serial data
    Clear {
        /// Serial port path
        port: String,
        /// Create a timestamped archive database before clearing
        #[arg(long)]
        archive: bool,
    },

    /// Show buffer and port statistics
    Stats {
        /// Serial port path
        port: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Flash firmware to an ESP device (requires espflash)
    #[cfg(feature = "esp")]
    Flash {
        /// Serial port path
        port: String,
        /// Path to firmware file (ELF or .bin)
        firmware: String,
        /// Baud rate for flashing
        #[arg(short, long)]
        baud: Option<u32>,
        /// Open live monitor after flashing
        #[arg(short, long)]
        monitor: bool,
    },

    /// Open a standalone GUI serial monitor
    #[cfg(feature = "monitor")]
    Monitor {
        /// Serial port path (e.g. /dev/ttyUSB0)
        port: String,
        /// Baud rate
        #[arg(short, long, default_value = "115200")]
        baud: u32,
    },

    /// Open a TUI serial monitor in the terminal
    #[cfg(feature = "tui")]
    Tui {
        /// Serial port path (e.g. /dev/ttyUSB0)
        port: String,
        /// Baud rate
        #[arg(short, long, default_value = "115200")]
        baud: u32,
    },

    /// Internal: monitor subprocess (used by MCP `serial_monitor_open`)
    #[command(hide = true)]
    #[cfg(feature = "monitor")]
    MonitorSubprocess {
        #[arg(long)]
        port: String,
        #[arg(long)]
        db: String,
        #[arg(long, default_value = "")]
        info: String,
    },
}

/// Run CLI application.
///
/// # Errors
/// Returns error on execution failure.
#[allow(clippy::too_many_lines)]
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let socket_path = cli.socket.unwrap_or_else(default_socket_path);
    let client = IpcClient::new(socket_path.clone());

    match cli.command {
        None | Some(Command::Mcp) => {
            if std::io::stdin().is_terminal() && cli.command.is_none() {
                use clap::CommandFactory;
                Cli::command().print_help()?;
                println!();
                return Ok(());
            }
            mcp::run_mcp()?;
        }

        Some(Command::Daemon { status, stop }) => {
            if status || stop {
                handlers::handle_daemon_control(&client, &socket_path, status, stop)?;
            } else {
                daemon::run_daemon(socket_path)?;
            }
        }

        Some(Command::List { json }) => {
            handlers::handle_list(&client, json)?;
        }

        Some(Command::Open {
            port,
            baud,
            data_bits,
            parity,
            stop_bits,
            flow_control,
        }) => {
            handlers::handle_open(
                &client,
                &port,
                baud,
                data_bits,
                &parity,
                stop_bits,
                &flow_control,
            )?;
        }

        Some(Command::Close { port }) => {
            handlers::handle_close(&client, &port)?;
        }

        Some(Command::Read {
            port,
            tail,
            from,
            limit,
            follow,
            timestamps,
            json,
        }) => {
            handlers::handle_read(&client, &port, tail, from, limit, follow, timestamps, json)?;
        }

        Some(Command::Write {
            port,
            data,
            hex,
            newline,
        }) => {
            handlers::handle_write(&client, &port, data, hex, newline)?;
        }

        Some(Command::Break { port, duration_ms }) => {
            handlers::handle_break(&client, &port, Some(duration_ms))?;
        }

        Some(Command::Send {
            port,
            file,
            protocol,
        }) => {
            handlers::handle_send_file(&client, &port, &file, protocol)?;
        }

        Some(Command::Recv {
            port,
            output,
            protocol,
        }) => {
            handlers::handle_recv_file(&client, &port, &output, protocol)?;
        }

        Some(Command::Signal { port, dtr, rts }) => {
            handlers::handle_signal(&client, &port, dtr, rts)?;
        }

        Some(Command::Macro { port, name }) => {
            handlers::handle_macro(&client, &port, &name)?;
        }

        Some(Command::Search {
            port,
            query,
            regex,
            limit,
            json,
        }) => {
            handlers::handle_search(&client, &port, &query, regex, limit, json)?;
        }

        Some(Command::Export {
            port,
            output,
            format,
            start,
            end,
        }) => {
            handlers::handle_export(&client, &port, &output, &format, start, end)?;
        }

        Some(Command::Clear { port, archive }) => {
            handlers::handle_clear(&client, &port, archive)?;
        }

        Some(Command::Stats { port, json }) => {
            handlers::handle_stats(&client, &port, json)?;
        }

        #[cfg(feature = "esp")]
        Some(Command::Flash {
            port,
            firmware,
            baud,
            monitor,
        }) => {
            handlers::handle_flash(&client, &port, &firmware, baud, monitor)?;
        }

        #[cfg(feature = "monitor")]
        Some(Command::Monitor { port, baud }) => {
            crate::standalone::run_monitor_standalone(&port, baud)?;
        }

        #[cfg(feature = "tui")]
        Some(Command::Tui { port, baud }) => {
            crate::standalone::run_tui_standalone(&port, baud)?;
        }

        #[cfg(feature = "monitor")]
        Some(Command::MonitorSubprocess { port, db, info }) => {
            crate::monitor::run_monitor(&port, std::path::Path::new(&db), &info)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        }
    }

    Ok(())
}
