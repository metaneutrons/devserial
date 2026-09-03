// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Command line surface: definitions, parser and router.
//!
//! Handlers return errors instead of terminating the process, so the library
//! stays usable from tests and other front ends. The exit code is set once, in
//! `main`.

pub mod bootstrap;
pub mod daemon;
pub mod handlers;
pub mod mcp;
pub mod output;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::export::ExportFormat;
use crate::modem::FileTransferProtocol;
use crate::protocol::SearchMode;
use crate::serial_params::{DEFAULT_BAUD, DEFAULT_BREAK_MS, FlowControl, Parity};

/// Errors reported by the command line front end.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error(transparent)]
    Ipc(#[from] crate::ipc::IpcError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Params(#[from] crate::serial_params::ParamError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Message(String),
}

impl CliError {
    /// Build an error from any message.
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// Serial line options shared by the commands that open a port.
#[derive(Parser, Debug, Clone, Copy)]
pub struct LineOptions {
    /// Baud rate
    #[arg(short, long, default_value_t = DEFAULT_BAUD)]
    pub baud: u32,
    /// Data bits (5, 6, 7, 8)
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(5..=8))]
    pub data_bits: u8,
    /// Parity
    #[arg(long, value_enum, default_value_t = Parity::None)]
    pub parity: Parity,
    /// Stop bits (1, 2)
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=2))]
    pub stop_bits: u8,
    /// Flow control
    #[arg(long, value_enum, default_value_t = FlowControl::None)]
    pub flow_control: FlowControl,
}

impl LineOptions {
    /// Convert into a port configuration, validating the numeric fields.
    ///
    /// # Errors
    /// Returns an error if a value is out of range.
    pub fn to_port_config(
        self,
    ) -> Result<crate::config::PortConfig, crate::serial_params::ParamError> {
        Ok(crate::config::PortConfig {
            baudrate: crate::serial_params::validate_baud(self.baud)?,
            data_bits: self.data_bits.try_into()?,
            parity: self.parity,
            stop_bits: self.stop_bits.try_into()?,
            flow_control: self.flow_control,
            ..crate::config::PortConfig::default()
        })
    }

    /// Convert into protocol settings.
    ///
    /// # Errors
    /// Returns an error if a value is out of range.
    pub fn to_settings(
        self,
    ) -> Result<crate::protocol::PortSettings, crate::serial_params::ParamError> {
        Ok(crate::protocol::PortSettings {
            baudrate: Some(crate::serial_params::validate_baud(self.baud)?),
            data_bits: Some(self.data_bits.try_into()?),
            parity: Some(self.parity),
            stop_bits: Some(self.stop_bits.try_into()?),
            flow_control: Some(self.flow_control),
        })
    }
}

/// `DevSerial`, a serial hardware bridge for LLMs, daemons and developers.
#[derive(Parser)]
#[command(name = "devserial", version, about)]
pub struct Cli {
    /// Path to a configuration file (default: ./devserial.toml, then the user config directory)
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Custom IPC endpoint (Unix socket path or Windows named pipe)
    #[arg(long, global = true)]
    pub socket: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run as MCP server (stdio transport), the default when stdin is piped
    Mcp,

    /// Start or manage the background daemon service
    Daemon {
        /// Check if the daemon is currently running
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
        #[command(flatten)]
        line: LineOptions,
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
        /// Number of recent lines to read (from the end)
        #[arg(short, long)]
        tail: Option<u32>,
        /// Starting line ID (1-based, negative counts from the end)
        #[arg(long, allow_hyphen_values = true)]
        from: Option<i64>,
        /// Read lines after this ID (exclusive)
        #[arg(long)]
        after: Option<i64>,
        /// Only lines newer than this RFC 3339 timestamp
        #[arg(long, value_name = "RFC3339")]
        since: Option<String>,
        /// Maximum number of lines to read
        #[arg(short, long, default_value_t = crate::protocol::DEFAULT_READ_LIMIT)]
        limit: u32,
        /// Wait up to this many milliseconds for new data
        #[arg(long, default_value_t = 0)]
        wait_ms: u64,
        /// Continuously follow and stream new lines
        #[arg(short, long)]
        follow: bool,
        /// Include timestamps in the output
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
        /// Append a newline (\r\n) to the data
        #[arg(short, long)]
        newline: bool,
    },

    /// Send an RS-232 serial BREAK signal (TX low pulse)
    Break {
        /// Serial port path
        port: String,
        /// Duration of the BREAK pulse in milliseconds
        #[arg(short, long, default_value_t = DEFAULT_BREAK_MS)]
        duration_ms: u64,
    },

    /// Send a file via modem protocol (XMODEM, YMODEM, ZMODEM)
    Send {
        /// Serial port path
        port: String,
        /// Path to the file to send
        file: String,
        /// Protocol to use
        #[arg(short, long, value_enum, default_value_t = FileTransferProtocol::Zmodem)]
        protocol: FileTransferProtocol,
    },

    /// Receive a file via modem protocol (XMODEM, YMODEM, ZMODEM)
    Recv {
        /// Serial port path
        port: String,
        /// Output directory to save the received file in
        #[arg(short, long, default_value = ".")]
        output: String,
        /// Protocol to use
        #[arg(short, long, value_enum, default_value_t = FileTransferProtocol::Zmodem)]
        protocol: FileTransferProtocol,
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

    /// Execute a macro sequence on a port (e.g. `reset`, `enter_bootloader`)
    Macro {
        /// Serial port path
        port: String,
        /// Macro name
        name: String,
    },

    /// Search buffered serial data
    Search {
        /// Serial port path
        port: String,
        /// Search pattern
        query: String,
        /// How the query is interpreted
        #[arg(short, long, value_enum, default_value_t = SearchMode::Substring)]
        mode: SearchMode,
        /// Only lines at or after this RFC 3339 timestamp
        #[arg(long, value_name = "RFC3339")]
        start: Option<String>,
        /// Only lines at or before this RFC 3339 timestamp
        #[arg(long, value_name = "RFC3339")]
        end: Option<String>,
        /// Maximum results to return
        #[arg(short, long, default_value_t = crate::protocol::DEFAULT_SEARCH_LIMIT)]
        limit: u32,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Export buffered lines to a file
    Export {
        /// Serial port path
        port: String,
        /// Output file path
        output: String,
        /// File format
        #[arg(short, long, value_enum, default_value_t = ExportFormat::Txt)]
        format: ExportFormat,
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
        /// Archive the database into the configured archive directory first
        #[arg(long)]
        archive: bool,
    },

    /// Show connection state and buffer statistics
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
        /// Path to the firmware file (ELF or .bin)
        firmware: String,
        /// Baud rate for flashing
        #[arg(short, long)]
        baud: Option<u32>,
        /// Follow the serial output after flashing
        #[arg(short, long)]
        monitor: bool,
    },

    /// Open the GUI port connection manager
    ///
    /// Starting `devserial` without arguments does the same, but only from a
    /// terminal. A desktop launcher has no terminal on stdin, so it needs this
    /// explicit form.
    #[cfg(feature = "monitor")]
    Gui,

    /// Open a standalone GUI serial monitor
    #[cfg(feature = "monitor")]
    Monitor {
        /// Serial port path (e.g. /dev/ttyUSB0)
        port: String,
        #[command(flatten)]
        line: LineOptions,
    },

    /// Open a TUI serial monitor in the terminal
    #[cfg(feature = "tui")]
    Tui {
        /// Serial port path (e.g. /dev/ttyUSB0)
        port: String,
        #[command(flatten)]
        line: LineOptions,
    },

    /// Display version, author, license and repository info
    About,

    /// Internal: monitor subprocess (used by the MCP tool `serial_monitor_open`)
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

/// Parse the command line and run the requested command.
///
/// # Errors
/// Returns the command's error; `main` turns it into an exit code.
pub fn run() -> Result<(), CliError> {
    let cli = Cli::parse();
    dispatch(cli)
}

fn dispatch(cli: Cli) -> Result<(), CliError> {
    let config_path = cli.config.clone();

    // An explicitly named configuration file is validated here, so a typo is
    // reported by the command the user ran rather than by a background daemon.
    if let Some(path) = config_path.as_deref() {
        crate::config::load_config(Some(path))?;
    }

    match cli.command {
        None | Some(Command::Mcp) => {
            let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
            if interactive && !matches!(cli.command, Some(Command::Mcp)) {
                return interactive_default();
            }
            mcp::run_mcp(config_path.as_deref())
        }

        Some(Command::Daemon { status, stop }) => {
            if status || stop {
                let session = handlers::Session::new(cli.socket, config_path)?;
                handlers::daemon_control(&session, status, stop)
            } else {
                daemon::run_daemon(cli.socket, config_path.as_deref())
            }
        }

        Some(Command::About) => {
            output::print_about();
            Ok(())
        }

        #[cfg(feature = "monitor")]
        Some(Command::Gui) => crate::standalone::run_monitor_gui_app(),

        #[cfg(feature = "monitor")]
        Some(Command::Monitor { port, line }) => crate::standalone::run_monitor_standalone(
            &port,
            &line.to_port_config()?,
            config_path.as_deref(),
        ),

        #[cfg(feature = "tui")]
        Some(Command::Tui { port, line }) => crate::standalone::run_tui_standalone(
            &port,
            &line.to_port_config()?,
            config_path.as_deref(),
        ),

        #[cfg(feature = "monitor")]
        Some(Command::MonitorSubprocess { port, db, info }) => {
            crate::monitor::run_monitor(&port, std::path::Path::new(&db), &info)
                .map_err(CliError::Message)
        }

        Some(command) => {
            let session = handlers::Session::new(cli.socket, config_path)?;
            handlers::run_remote(&session, command)
        }
    }
}

/// What happens when the binary is started without arguments in a terminal.
fn interactive_default() -> Result<(), CliError> {
    #[cfg(feature = "monitor")]
    {
        crate::standalone::run_monitor_gui_app()
    }
    #[cfg(not(feature = "monitor"))]
    {
        use clap::CommandFactory;
        Cli::command()
            .print_help()
            .map_err(|e| CliError::msg(e.to_string()))?;
        println!();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn line_options_reject_out_of_range_values() {
        let err = Cli::try_parse_from(["devserial", "open", "/dev/x", "--data-bits", "9"]);
        assert!(err.is_err());
        let err = Cli::try_parse_from(["devserial", "open", "/dev/x", "--stop-bits", "3"]);
        assert!(err.is_err());
    }

    #[test]
    fn parity_is_validated_by_the_parser() {
        assert!(
            Cli::try_parse_from(["devserial", "open", "/dev/x", "--parity", "sideways"]).is_err()
        );
        assert!(Cli::try_parse_from(["devserial", "open", "/dev/x", "--parity", "even"]).is_ok());
    }

    #[test]
    fn export_format_is_validated_by_the_parser() {
        assert!(
            Cli::try_parse_from(["devserial", "export", "/dev/x", "out", "--format", "xml"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["devserial", "export", "/dev/x", "out", "--format", "jsonl"])
                .is_ok()
        );
    }

    #[test]
    fn line_options_convert_into_a_port_config() {
        let cli = Cli::try_parse_from([
            "devserial",
            "open",
            "/dev/x",
            "--baud",
            "9600",
            "--parity",
            "odd",
            "--data-bits",
            "7",
            "--stop-bits",
            "2",
            "--flow-control",
            "hardware",
        ])
        .unwrap();
        let Some(Command::Open { line, .. }) = cli.command else {
            panic!("expected open");
        };
        let config = line.to_port_config().unwrap();
        assert_eq!(config.baudrate, 9600);
        assert_eq!(config.parity, Parity::Odd);
        assert_eq!(config.data_bits.get(), 7);
        assert_eq!(config.stop_bits.get(), 2);
        assert_eq!(config.flow_control, FlowControl::Hardware);
        assert_eq!(config.framing_summary(), "9600 7O2 (RTS/CTS)");
    }

    #[test]
    fn a_negative_start_line_is_accepted() {
        // The engine counts back from the end for negative IDs, so the parser
        // has to let the value through instead of reading it as a flag.
        let cli = Cli::try_parse_from(["devserial", "read", "/dev/x", "--from", "-20"]).unwrap();
        let Some(Command::Read { from, .. }) = cli.command else {
            panic!("expected read");
        };
        assert_eq!(from, Some(-20));
    }

    #[test]
    fn zero_baud_is_rejected_on_conversion() {
        let options = LineOptions {
            baud: 0,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: 1,
            flow_control: FlowControl::None,
        };
        assert!(options.to_port_config().is_err());
    }
}
