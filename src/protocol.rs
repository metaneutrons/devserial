// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Strongly-typed IPC protocol between devserial clients and the daemon.

use serde::{Deserialize, Serialize};

use crate::modem::FileTransferProtocol;
use crate::reader::ConnectionState;
use crate::storage::{BufferStats, StoredLine};

/// Unique identifier for an RPC request/response pair.
pub type RequestId = u64;

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

/// Request payloads supported by the devserial daemon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", content = "params")]
pub enum RequestPayload {
    /// Liveness check.
    Ping,
    /// List all currently managed ports.
    ListPorts,
    /// Open a serial port with optional configuration.
    OpenPort {
        name: String,
        baudrate: Option<u32>,
        data_bits: Option<u8>,
        parity: Option<String>,
        stop_bits: Option<u8>,
        flow_control: Option<String>,
    },
    /// Close a managed serial port.
    ClosePort { name: String },
    /// Read lines from a port buffer.
    ReadLines {
        port: String,
        start_id: Option<i64>,
        count: Option<u32>,
        tail: Option<u32>,
    },
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
    /// Search buffer for a substring or regex pattern.
    Search {
        port: String,
        query: String,
        is_regex: bool,
        start_ns: Option<i64>,
        end_ns: Option<i64>,
        limit: Option<u32>,
    },
    /// Export buffered lines to a file.
    Export {
        port: String,
        output_path: String,
        file_format: Option<String>,
        start_line: Option<i64>,
        end_line: Option<i64>,
    },
    /// Clear buffered lines, optionally creating an archive DB first.
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
    /// Request graceful daemon shutdown.
    Shutdown,
}

/// Successful response payloads returned by the daemon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "data")]
pub enum ResponsePayload {
    /// Ping acknowledgment.
    Pong,
    /// List of managed ports.
    PortList(Vec<PortInfoResponse>),
    /// Port opened successfully.
    PortOpened {
        name: String,
        config_summary: String,
    },
    /// Port closed successfully.
    PortClosed { name: String },
    /// Lines retrieved from port buffer.
    Lines(Vec<StoredLine>),
    /// Data written successfully.
    WriteSuccess { bytes_written: usize },
    /// Serial break signal sent successfully.
    BreakSuccess,
    /// File transfer completed successfully.
    TransferSuccess {
        bytes_transferred: u64,
        file_name: String,
        protocol: FileTransferProtocol,
    },
    /// Control signal updated successfully.
    SignalSuccess,
    /// Macro executed successfully.
    MacroSuccess { executed_steps: Vec<String> },
    /// Search results matching query.
    SearchResults(Vec<StoredLine>),
    /// Export completed.
    ExportSuccess { lines_exported: u64, path: String },
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
                baudrate: Some(115_200),
                data_bits: Some(8),
                parity: Some("none".to_string()),
                stop_bits: Some(1),
                flow_control: None,
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
            result: Ok(ResponsePayload::Lines(vec![StoredLine {
                id: 1,
                timestamp_ns: 1_000_000,
                payload: "test line".to_string(),
            }])),
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
}
