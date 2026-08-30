// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! The devserial operation vocabulary.
//!
//! Every capability of the product appears here exactly once. The CLI, the IPC
//! daemon and the MCP server are transports over this vocabulary; none of them
//! may implement an operation of its own, because that is how the two sides
//! drifted apart before.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::export::ExportFormat;
use crate::modem::FileTransferProtocol;
use crate::reader::ConnectionState;
use crate::serial_params::{DataBits, FlowControl, Parity, StopBits};
use crate::storage::{BufferStats, StoredLine};

/// Unique identifier for an RPC request/response pair.
pub type RequestId = u64;

/// Default number of lines returned by a read.
pub const DEFAULT_READ_LIMIT: u32 = 100;
/// Hard upper bound on lines returned by a single read.
pub const MAX_READ_LIMIT: u32 = 10_000;
/// Default number of search results.
pub const DEFAULT_SEARCH_LIMIT: u32 = 100;
/// Hard upper bound on search results.
pub const MAX_SEARCH_LIMIT: u32 = 1000;
/// Upper bound on lines inspected by a filtering search.
pub const MAX_SEARCH_SCAN: u64 = 5_000_000;

/// An RPC request envelope sent from client to daemon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcRequest {
    pub id: RequestId,
    pub payload: RequestPayload,
}

/// An RPC response envelope returned from daemon to client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcResponse {
    pub id: RequestId,
    pub result: Result<ResponsePayload, String>,
}

/// Information about a managed serial port.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortInfoResponse {
    pub name: String,
    pub state: ConnectionState,
    pub total_lines: u64,
}

/// Serial line settings, all optional so a caller can change one of them.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortSettings {
    pub baudrate: Option<u32>,
    pub data_bits: Option<DataBits>,
    pub parity: Option<Parity>,
    pub stop_bits: Option<StopBits>,
    pub flow_control: Option<FlowControl>,
}

/// Where a read starts.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadWindow {
    /// First line id to return, 1-based. Negative counts back from the end.
    pub start_id: Option<i64>,
    /// Return lines after this id, exclusive. Ideal for incremental polling.
    pub after_id: Option<i64>,
    /// Return the last `tail` lines.
    pub tail: Option<u32>,
    /// Return only lines newer than this timestamp.
    pub since_ns: Option<i64>,
    /// Maximum number of lines to return.
    pub limit: Option<u32>,
    /// Wait up to this long for data when the window is empty.
    pub wait_ms: Option<u64>,
}

/// A page of captured lines together with the metadata a caller needs to
/// continue reading.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinesPage {
    pub lines: Vec<StoredLine>,
    /// Total number of lines currently buffered.
    pub total_lines: u64,
    /// Id to pass as `after_id` for the next read.
    pub next_after_id: i64,
}

/// How a search query is interpreted.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    clap::ValueEnum,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
#[schemars(description = "Query type: substring (default), exact or regex")]
pub enum SearchMode {
    /// Payload contains the query.
    #[default]
    Substring,
    /// Payload equals the query.
    Exact,
    /// Payload matches the query as a Rust regular expression.
    Regex,
}

impl fmt::Display for SearchMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Substring => "substring",
            Self::Exact => "exact",
            Self::Regex => "regex",
        })
    }
}

impl FromStr for SearchMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "substring" | "contains" => Ok(Self::Substring),
            "exact" => Ok(Self::Exact),
            "regex" | "re" => Ok(Self::Regex),
            other => Err(format!(
                "unknown query type '{other}', expected one of: substring, exact, regex"
            )),
        }
    }
}

/// Search results plus whether the scan was cut short.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchOutcome {
    pub results: Vec<StoredLine>,
    /// True when scanning hit its budget before reaching the end of the buffer.
    pub truncated: bool,
}

/// Request payloads supported by the devserial engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", content = "params")]
pub enum RequestPayload {
    /// Liveness check.
    Ping,
    /// List all currently managed ports.
    ListPorts,
    /// List serial ports the operating system reports.
    ListHardware,
    /// Open a serial port with optional configuration.
    OpenPort {
        name: String,
        settings: PortSettings,
    },
    /// Change settings of an already open port.
    ReconfigurePort {
        name: String,
        settings: PortSettings,
    },
    /// Close a managed serial port.
    ClosePort { name: String },
    /// Read lines from a port buffer.
    ReadLines { port: String, window: ReadWindow },
    /// Connection state and buffer statistics of a port.
    GetStatus { port: String },
    /// Write data to a serial port (UTF-8 text or hex).
    WriteData {
        port: String,
        data: String,
        is_hex: bool,
    },
    /// Send RS-232 serial break signal.
    SendBreak {
        port: String,
        duration_ms: Option<u64>,
    },
    /// Send a file via modem protocol (XMODEM, YMODEM, ZMODEM).
    SendFile {
        port: String,
        file_path: String,
        protocol: FileTransferProtocol,
    },
    /// Receive a file via modem protocol (XMODEM, YMODEM, ZMODEM).
    ReceiveFile {
        port: String,
        output_dir: String,
        protocol: FileTransferProtocol,
    },
    /// Set control signals (DTR, RTS) on a serial port.
    SetSignal {
        port: String,
        dtr: Option<bool>,
        rts: Option<bool>,
    },
    /// Execute a named macro sequence on a port.
    ExecuteMacro { port: String, macro_name: String },
    /// Search the buffer.
    Search {
        port: String,
        query: String,
        mode: SearchMode,
        start_ns: Option<i64>,
        end_ns: Option<i64>,
        limit: Option<u32>,
    },
    /// Export buffered lines to a file.
    Export {
        port: String,
        output_path: String,
        file_format: Option<ExportFormat>,
        start_line: Option<i64>,
        end_line: Option<i64>,
    },
    /// Clear buffered lines, optionally creating an archive first.
    Clear {
        port: String,
        archive_current: Option<bool>,
    },
    /// Get buffer statistics for a port.
    GetStats { port: String },
    /// Flash firmware via espflash.
    #[cfg(feature = "esp")]
    EspFlash {
        port: String,
        firmware_path: String,
        baud: Option<u32>,
    },
    /// Query ESP board info via espflash.
    #[cfg(feature = "esp")]
    EspInfo { port: String },
    /// Erase ESP flash via espflash.
    #[cfg(feature = "esp")]
    EspErase { port: String },
    /// Write a raw binary to a flash address via espflash.
    #[cfg(feature = "esp")]
    EspWriteBin {
        port: String,
        file_path: String,
        address: String,
    },
    /// Request graceful daemon shutdown.
    Shutdown,
}

impl RequestPayload {
    /// Name of the port an operation targets, if it targets one.
    #[must_use]
    pub fn port(&self) -> Option<&str> {
        match self {
            Self::OpenPort { name, .. }
            | Self::ReconfigurePort { name, .. }
            | Self::ClosePort { name } => Some(name),
            Self::ReadLines { port, .. }
            | Self::GetStatus { port }
            | Self::WriteData { port, .. }
            | Self::SendBreak { port, .. }
            | Self::SendFile { port, .. }
            | Self::ReceiveFile { port, .. }
            | Self::SetSignal { port, .. }
            | Self::ExecuteMacro { port, .. }
            | Self::Search { port, .. }
            | Self::Export { port, .. }
            | Self::Clear { port, .. }
            | Self::GetStats { port } => Some(port),
            #[cfg(feature = "esp")]
            Self::EspFlash { port, .. }
            | Self::EspInfo { port }
            | Self::EspErase { port }
            | Self::EspWriteBin { port, .. } => Some(port),
            Self::Ping | Self::ListPorts | Self::ListHardware | Self::Shutdown => None,
        }
    }
}

/// Successful response payloads returned by the engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "data")]
pub enum ResponsePayload {
    /// Ping acknowledgment.
    Pong,
    /// List of managed ports.
    PortList(Vec<PortInfoResponse>),
    /// Serial ports reported by the operating system.
    HardwarePorts(Vec<String>),
    /// Port opened successfully.
    PortOpened {
        name: String,
        config_summary: String,
    },
    /// Port reconfigured successfully.
    PortReconfigured {
        name: String,
        config_summary: String,
    },
    /// Port closed successfully.
    PortClosed { name: String },
    /// Lines retrieved from port buffer.
    Lines(LinesPage),
    /// Connection state and buffer statistics.
    Status {
        name: String,
        state: ConnectionState,
        stats: BufferStats,
    },
    /// Data written successfully.
    WriteSuccess { bytes_written: usize },
    /// Serial break signal sent successfully.
    BreakSuccess { duration_ms: u64 },
    /// File transfer completed successfully.
    TransferSuccess {
        bytes_transferred: u64,
        file_name: String,
        protocol: FileTransferProtocol,
        path: Option<String>,
    },
    /// Control signal updated successfully.
    SignalSuccess { applied: Vec<String> },
    /// Macro executed successfully.
    MacroSuccess { executed_steps: Vec<String> },
    /// Search results matching query.
    SearchResults(SearchOutcome),
    /// Export completed.
    ExportSuccess {
        lines_exported: u64,
        path: String,
        file_format: ExportFormat,
    },
    /// Buffer cleared.
    ClearSuccess {
        lines_cleared: u64,
        archive_path: Option<String>,
    },
    /// Buffer statistics.
    Stats(BufferStats),
    /// ESP tool command output.
    #[cfg(feature = "esp")]
    EspSuccess(String),
    /// Daemon shutdown initiated.
    ShutdownAck,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rpc_request_serde_roundtrip() {
        let req = RpcRequest {
            id: 42,
            payload: RequestPayload::OpenPort {
                name: "/dev/ttyUSB0".to_string(),
                settings: PortSettings {
                    baudrate: Some(115_200),
                    data_bits: Some(DataBits::EIGHT),
                    parity: Some(Parity::None),
                    stop_bits: Some(StopBits::ONE),
                    flow_control: None,
                },
            },
        };

        let json = serde_json::to_string(&req).unwrap();
        let deserialized: RpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, deserialized);
    }

    #[test]
    fn test_rpc_response_serde_roundtrip() {
        let resp = RpcResponse {
            id: 42,
            result: Ok(ResponsePayload::Lines(LinesPage {
                lines: vec![StoredLine {
                    id: 1,
                    timestamp_ns: 1_000_000,
                    payload: "test line".to_string(),
                }],
                total_lines: 1,
                next_after_id: 1,
            })),
        };

        let json = serde_json::to_string(&resp).unwrap();
        let deserialized: RpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, deserialized);
    }

    #[test]
    fn test_rpc_error_response_serde_roundtrip() {
        let resp = RpcResponse {
            id: 99,
            result: Err("port not found".to_string()),
        };

        let json = serde_json::to_string(&resp).unwrap();
        let deserialized: RpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, deserialized);
    }

    #[test]
    fn search_mode_parsing() {
        assert_eq!("exact".parse::<SearchMode>().unwrap(), SearchMode::Exact);
        assert_eq!("REGEX".parse::<SearchMode>().unwrap(), SearchMode::Regex);
        assert!("fuzzy".parse::<SearchMode>().is_err());
    }

    #[test]
    fn every_port_operation_reports_its_port() {
        let payload = RequestPayload::GetStats {
            port: "/dev/x".into(),
        };
        assert_eq!(payload.port(), Some("/dev/x"));
        assert_eq!(RequestPayload::Ping.port(), None);
    }
}
