// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! MCP transport.
//!
//! Every tool translates its parameters into a [`RequestPayload`], hands it to
//! the [`CommandEngine`] and renders the response for a language model. There is
//! deliberately no operation logic here; that lives in the engine and is shared
//! with the CLI and the daemon.

#![allow(unknown_lints)]
#![allow(clippy::unused_async_trait_impl)]

use std::fmt::Write as _;
use std::sync::Arc;

use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResponse, CallToolResult, ContentBlock, Implementation, ServerCapabilities,
        ServerConfig,
    },
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::engine::{CommandEngine, EngineError};
use crate::export::ExportFormat;
use crate::modem::FileTransferProtocol;
use crate::protocol::{
    LinesPage, PortSettings, ReadWindow, RequestPayload, ResponsePayload, SearchMode,
};
use crate::serial_params::{DataBits, FlowControl, Parity, StopBits};

/// MCP server for serial hardware bridging.
#[derive(Clone)]
pub struct DevSerialServer {
    tool_router: ToolRouter<Self>,
    engine: CommandEngine,
    #[cfg(feature = "monitor")]
    monitors: Arc<std::sync::Mutex<std::collections::HashMap<String, MonitorState>>>,
}

/// State for a running monitor window (feature-gated).
#[cfg(feature = "monitor")]
struct MonitorState {
    handle: crate::monitor::MonitorHandle,
}

impl DevSerialServer {
    /// Create a server over an existing engine.
    #[must_use]
    pub fn new(engine: CommandEngine) -> Self {
        Self {
            tool_router: Self::build_router(),
            engine,
            #[cfg(feature = "monitor")]
            monitors: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Create a server with an in-memory state database, for tests.
    ///
    /// # Panics
    /// Panics if the in-memory state database cannot be created, which would
    /// mean the bundled `SQLite` is broken.
    #[must_use]
    pub fn with_port_manager(port_manager: crate::port_manager::PortManagerHandle) -> Self {
        let state = crate::state::StateDb::open_memory().expect("in-memory state database");
        let engine = CommandEngine::new(
            port_manager,
            Arc::new(std::sync::Mutex::new(state)),
            Arc::new(crate::config::Config::default()),
        );
        Self::new(engine)
    }

    /// The engine backing this server.
    #[must_use]
    pub const fn engine(&self) -> &CommandEngine {
        &self.engine
    }

    /// Combine the always-present tools with the optional ones.
    ///
    /// The ESP tools live in their own router so that a build without the `esp`
    /// feature still produces a complete, compiling router.
    fn build_router() -> ToolRouter<Self> {
        let router = Self::core_tools();
        #[cfg(feature = "esp")]
        let router = router + Self::esp_tools();
        #[cfg(feature = "monitor")]
        let router = router + Self::monitor_tools();
        router
    }

    /// Run an operation and map failures onto MCP errors.
    async fn run(&self, payload: RequestPayload) -> Result<ResponsePayload, rmcp::ErrorData> {
        self.engine
            .execute(payload)
            .await
            .map_err(|e| to_error_data(&e))
    }
}

/// Failure raised while a valid tool call is being executed.
///
/// rmcp treats `ErrorData` returned from a tool handler as a JSON-RPC error,
/// which hides the useful message from MCP clients. Wrapping the engine or
/// validation error makes rmcp encode it as a normal `CallToolResult` with
/// `isError: true`.
#[derive(Debug)]
struct ToolExecutionError(rmcp::ErrorData);

impl From<rmcp::ErrorData> for ToolExecutionError {
    fn from(error: rmcp::ErrorData) -> Self {
        Self(error)
    }
}

impl std::ops::Deref for ToolExecutionError {
    type Target = rmcp::ErrorData;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl IntoCallToolResult for ToolExecutionError {
    fn into_call_tool_result(self) -> Result<CallToolResponse, rmcp::ErrorData> {
        Ok(CallToolResult::error(vec![ContentBlock::text(self.0.message)]).into())
    }
}

/// Map an engine error onto the closest MCP error kind.
fn to_error_data(error: &EngineError) -> rmcp::ErrorData {
    let message = error.to_string();
    match *error {
        EngineError::Invalid(_)
        | EngineError::Param(_)
        | EngineError::Hex(_)
        | EngineError::Regex(_)
        | EngineError::UnknownMacro { .. } => rmcp::ErrorData::invalid_params(message, None),
        _ => rmcp::ErrorData::internal_error(message, None),
    }
}

fn bad_params(message: impl Into<String>) -> rmcp::ErrorData {
    rmcp::ErrorData::invalid_params(message.into(), None)
}

/// Convert an optional raw data-bit count into the validated type.
fn data_bits(value: Option<u8>) -> Result<Option<DataBits>, rmcp::ErrorData> {
    value
        .map(DataBits::try_from)
        .transpose()
        .map_err(|e| bad_params(e.to_string()))
}

/// Convert an optional raw stop-bit count into the validated type.
fn stop_bits(value: Option<u8>) -> Result<Option<StopBits>, rmcp::ErrorData> {
    value
        .map(StopBits::try_from)
        .transpose()
        .map_err(|e| bad_params(e.to_string()))
}

/// Parse an optional RFC 3339 timestamp into nanoseconds.
fn parse_time(value: Option<&str>) -> Result<Option<i64>, rmcp::ErrorData> {
    value
        .map(|text| {
            let datetime = chrono::DateTime::parse_from_rfc3339(text)
                .map_err(|e| bad_params(format!("invalid time '{text}': {e}")))?;
            datetime.timestamp_nanos_opt().ok_or_else(|| {
                bad_params(format!(
                    "invalid time '{text}': timestamp is outside the supported nanosecond range"
                ))
            })
        })
        .transpose()
}

#[cfg(test)]
mod parse_time_tests {
    use super::parse_time;

    #[test]
    fn rfc3339_timestamp_outside_nanosecond_range_is_rejected() {
        let error = parse_time(Some("9999-12-31T23:59:59Z")).unwrap_err();
        assert!(
            error
                .message
                .contains("outside the supported nanosecond range")
        );
    }

    #[test]
    fn rfc3339_timestamp_preserves_nanoseconds() {
        assert_eq!(
            parse_time(Some("2026-01-02T03:04:05.123456789Z")).unwrap(),
            Some(1_767_323_045_123_456_789)
        );
    }
}

// ------------------------------------------------------------------ params

/// Parameters for `serial_read`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(
    description = "Read lines from a serial port's persistent buffer. The response starts with a metadata header: [lines X-Y of Z total]. Use after_line for incremental reads."
)]
pub struct ReadBufferParams {
    /// Serial port name (e.g. `/dev/ttyUSB0`).
    #[schemars(description = "Serial port name (e.g. /dev/ttyUSB0)")]
    pub port_name: String,
    /// Starting line number (1-based). Negative counts from the end.
    #[schemars(
        description = "Starting line ID (1-based, default: 1). Negative counts from the end: -50 = last 50 lines."
    )]
    pub start_line: Option<i64>,
    /// Read lines after this ID (exclusive).
    #[schemars(
        description = "Read lines after this ID (exclusive). Pass the Y value from the previous response header to get only new data."
    )]
    pub after_line: Option<i64>,
    /// Maximum number of lines to return.
    #[schemars(description = "Max lines to return (default: 100, max: 10000)")]
    pub max_lines: Option<u32>,
    /// Whether to include timestamps in the output.
    #[schemars(description = "Include timestamps before each line (default: false)")]
    pub include_timestamps: Option<bool>,
    /// Wait up to this many milliseconds for new data.
    #[schemars(
        description = "Wait up to N ms for new data if the buffer is empty at the requested position (0 = return immediately, default)."
    )]
    pub wait_ms: Option<u64>,
    /// Only return lines newer than this RFC 3339 timestamp.
    #[schemars(
        description = "Only return lines newer than this timestamp (RFC 3339, e.g. 2026-05-10T23:00:00Z)."
    )]
    pub since_time: Option<String>,
}

/// Parameters for a tool that only needs a port name.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Operate on a single serial port")]
pub struct PortParams {
    /// Serial port name.
    #[schemars(description = "Serial port name (e.g. /dev/ttyUSB0)")]
    pub port_name: String,
}

/// Parameters for `serial_search`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(
    description = "Search the serial buffer. Returns matching lines with line numbers and timestamps."
)]
pub struct SearchBufferParams {
    /// Serial port name.
    #[schemars(description = "Serial port name (e.g. /dev/ttyUSB0)")]
    pub port_name: String,
    /// Search query string.
    #[schemars(description = "Search query (pattern or literal text)")]
    pub query: String,
    /// How the query is interpreted.
    pub query_type: Option<SearchMode>,
    /// Start time bound (RFC 3339).
    #[schemars(description = "Optional start time bound (RFC 3339, e.g. 2026-01-15T10:00:00Z)")]
    pub start_time: Option<String>,
    /// End time bound (RFC 3339).
    #[schemars(description = "Optional end time bound (RFC 3339, e.g. 2026-01-15T12:00:00Z)")]
    pub end_time: Option<String>,
    /// Maximum results to return.
    #[schemars(description = "Max results to return (default: 100, max: 1000)")]
    pub max_results: Option<u32>,
}

/// Parameters for `serial_export`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Export the serial buffer to a file")]
pub struct ExportBufferParams {
    /// Serial port name.
    #[schemars(description = "Serial port name (e.g. /dev/ttyUSB0)")]
    pub port_name: String,
    /// Output format.
    pub file_format: Option<ExportFormat>,
    /// Start line (inclusive).
    #[schemars(description = "Start line number (inclusive, default: 1)")]
    pub start_line: Option<i64>,
    /// End line (inclusive).
    #[schemars(description = "End line number (inclusive, default: last line)")]
    pub end_line: Option<i64>,
    /// Output file path.
    #[schemars(description = "Absolute path for the output file")]
    pub output_path: String,
}

/// Parameters for `serial_clear`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Clear a serial port's buffer, optionally archiving it first")]
pub struct ClearBufferParams {
    /// Serial port name.
    #[schemars(description = "Serial port name (e.g. /dev/ttyUSB0)")]
    pub port_name: String,
    /// Whether to archive the current data before clearing.
    #[schemars(
        description = "Archive current data into the configured archive directory before clearing (default: false)"
    )]
    pub archive_current: Option<bool>,
}

/// Parameters for `serial_open`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Open or reconfigure a serial port")]
pub struct ConfigurePortParams {
    /// Serial port path.
    #[schemars(description = "Serial port path (e.g. /dev/ttyUSB0 or COM3)")]
    pub port_name: String,
    /// Baud rate.
    #[schemars(description = "Baud rate (default: 115200)")]
    pub baudrate: Option<u32>,
    /// Data bits (5-8).
    #[schemars(description = "Data bits: 5, 6, 7 or 8 (default: 8)")]
    pub data_bits: Option<u8>,
    /// Parity.
    pub parity: Option<Parity>,
    /// Stop bits (1, 2).
    #[schemars(description = "Stop bits: 1 or 2 (default: 1)")]
    pub stop_bits: Option<u8>,
    /// Flow control.
    pub flow_control: Option<FlowControl>,
}

/// Parameters for `serial_signal`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Set serial port control lines (DTR/RTS)")]
pub struct SetControlLinesParams {
    /// Serial port name.
    #[schemars(description = "Serial port name")]
    pub port_name: String,
    /// DTR line state.
    #[schemars(description = "Set DTR line (true=high, false=low)")]
    pub dtr: Option<bool>,
    /// RTS line state.
    #[schemars(description = "Set RTS line (true=high, false=low)")]
    pub rts: Option<bool>,
}

/// Parameters for `serial_break`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Send an RS-232 BREAK signal by holding TX low")]
pub struct SerialBreakParams {
    /// Serial port name.
    #[schemars(description = "Serial port name")]
    pub port_name: String,
    /// Duration in milliseconds.
    #[schemars(description = "Break signal duration in milliseconds (default: 250)")]
    pub duration_ms: Option<u64>,
}

/// Parameters for `serial_send_file`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Send a file over a serial port using a modem transfer protocol")]
pub struct SerialSendFileParams {
    /// Serial port name.
    #[schemars(description = "Serial port name")]
    pub port_name: String,
    /// Path to the file to send.
    #[schemars(description = "Path to the file to send")]
    pub file_path: String,
    /// Transfer protocol, defaults to ZMODEM.
    pub protocol: Option<FileTransferProtocol>,
}

/// Parameters for `serial_receive_file`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Receive a file over a serial port using a modem transfer protocol")]
pub struct SerialReceiveFileParams {
    /// Serial port name.
    #[schemars(description = "Serial port name")]
    pub port_name: String,
    /// Output directory for the received file.
    #[schemars(description = "Output directory for the received file (default: .)")]
    pub output_dir: Option<String>,
    /// Transfer protocol, defaults to ZMODEM.
    pub protocol: Option<FileTransferProtocol>,
}

/// Parameters for `serial_macro`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Execute a built-in or user-defined macro sequence on a serial port")]
pub struct TriggerMacroParams {
    /// Serial port name.
    #[schemars(description = "Serial port name")]
    pub port_name: String,
    /// Macro name.
    #[schemars(
        description = "Macro name (built-in: 'reset', 'enter_bootloader', 'break', or user-defined)"
    )]
    pub macro_name: String,
}

/// Parameters for `serial_write`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(
    description = "Write data to a serial port. No newline is appended automatically; include \\r\\n in the data if the device expects it."
)]
pub struct WritePortParams {
    /// Serial port name.
    #[schemars(description = "Serial port name")]
    pub port_name: String,
    /// Data to write.
    #[schemars(
        description = "Data to write. UTF-8 text, or hex bytes prefixed with '0x' (e.g. '0x0D0A')"
    )]
    pub data: String,
}

/// Parameters for `serial_esp_flash`.
#[cfg(feature = "esp")]
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Flash firmware to an ESP device via espflash")]
pub struct EspFlashParams {
    /// Serial port name.
    #[schemars(description = "Serial port name (e.g. /dev/ttyUSB0)")]
    pub port_name: String,
    /// Path to the firmware file.
    #[schemars(description = "Path to the firmware file (ELF or .bin)")]
    pub firmware_path: String,
    /// Optional baud rate for flashing.
    #[schemars(description = "Flash baud rate (default: espflash default)")]
    pub baud: Option<u32>,
}

/// Parameters for `serial_esp_write_bin`.
#[cfg(feature = "esp")]
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Write a raw binary file to a flash address on an ESP device")]
pub struct EspWriteBinParams {
    /// Serial port name.
    #[schemars(description = "Serial port name (e.g. /dev/ttyUSB0)")]
    pub port_name: String,
    /// Path to the binary file.
    #[schemars(description = "Path to the binary file (.bin)")]
    pub file_path: String,
    /// Flash address.
    #[schemars(description = "Flash address in hex (e.g. 0x1000, 0x8000, 0x10000)")]
    pub address: String,
}

// ------------------------------------------------------------------ tools

#[tool_router(router = core_tools, vis = "pub")]
impl DevSerialServer {
    /// Read captured serial data.
    #[tool(
        description = "Read captured serial data. Returns a metadata header plus lines. Use after_line for efficient incremental polling.",
        annotations(read_only_hint = true)
    )]
    async fn serial_read(
        &self,
        Parameters(params): Parameters<ReadBufferParams>,
    ) -> Result<String, ToolExecutionError> {
        let window = ReadWindow {
            start_id: params.start_line,
            after_id: params.after_line,
            tail: None,
            since_ns: parse_time(params.since_time.as_deref())?,
            limit: params.max_lines,
            wait_ms: params.wait_ms,
        };

        let response = self
            .run(RequestPayload::ReadLines {
                port: params.port_name,
                window,
            })
            .await?;
        let ResponsePayload::Lines(page) = response else {
            return Err(unexpected().into());
        };

        Ok(render_lines(
            &page,
            params.include_timestamps.unwrap_or(false),
        ))
    }

    /// Port status: connection state and buffer statistics.
    #[tool(
        description = "Get serial port status: connection state, total lines, bytes, last activity and database size.",
        annotations(read_only_hint = true)
    )]
    async fn serial_status(
        &self,
        Parameters(params): Parameters<PortParams>,
    ) -> Result<String, ToolExecutionError> {
        let response = self
            .run(RequestPayload::GetStatus {
                port: params.port_name,
            })
            .await?;
        let ResponsePayload::Status { name, state, stats } = response else {
            return Err(unexpected().into());
        };

        let json = serde_json::json!({
            "port": name,
            "state": format!("{state:?}"),
            "total_lines": stats.total_lines,
            "total_bytes": stats.total_bytes,
            "last_activity": stats
                .last_timestamp_ns
                .map_or_else(|| "never".to_string(), crate::export::format_timestamp),
            "db_size_bytes": stats.db_size_bytes,
        });
        serde_json::to_string_pretty(&json)
            .map_err(|e| to_error_data(&EngineError::Tool(e.to_string())).into())
    }

    /// Search captured serial data.
    #[tool(
        description = "Search serial data by substring, exact match or regular expression, with optional time bounds.",
        annotations(read_only_hint = true)
    )]
    async fn serial_search(
        &self,
        Parameters(params): Parameters<SearchBufferParams>,
    ) -> Result<String, ToolExecutionError> {
        let response = self
            .run(RequestPayload::Search {
                port: params.port_name,
                query: params.query,
                mode: params.query_type.unwrap_or_default(),
                start_ns: parse_time(params.start_time.as_deref())?,
                end_ns: parse_time(params.end_time.as_deref())?,
                limit: params.max_results,
            })
            .await?;
        let ResponsePayload::SearchResults(outcome) = response else {
            return Err(unexpected().into());
        };

        let mut out = String::new();
        for line in &outcome.results {
            let _ = writeln!(
                out,
                "{}:[{}] {}",
                line.id,
                crate::export::format_timestamp(line.timestamp_ns),
                line.payload
            );
        }
        if outcome.results.is_empty() {
            out.push_str("(no matches)\n");
        }
        if outcome.truncated {
            out.push_str(
                "(scan limit reached, later lines were not examined; narrow the time range)\n",
            );
        }
        Ok(out)
    }

    /// Export captured serial data to a file.
    #[tool(
        description = "Export serial data to a file as txt (raw lines), csv (RFC 4180) or jsonl.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn serial_export(
        &self,
        Parameters(params): Parameters<ExportBufferParams>,
    ) -> Result<String, ToolExecutionError> {
        let response = self
            .run(RequestPayload::Export {
                port: params.port_name,
                output_path: params.output_path,
                file_format: params.file_format,
                start_line: params.start_line,
                end_line: params.end_line,
            })
            .await?;
        let ResponsePayload::ExportSuccess {
            lines_exported,
            path,
            file_format,
        } = response
        else {
            return Err(unexpected().into());
        };
        Ok(format!(
            "Exported {lines_exported} lines to {path} (format: {file_format})"
        ))
    }

    /// Clear captured serial data.
    #[tool(
        description = "Clear captured serial data. Optionally archives the database into the configured archive directory first.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn serial_clear(
        &self,
        Parameters(params): Parameters<ClearBufferParams>,
    ) -> Result<String, ToolExecutionError> {
        let response = self
            .run(RequestPayload::Clear {
                port: params.port_name,
                archive_current: params.archive_current,
            })
            .await?;
        let ResponsePayload::ClearSuccess {
            lines_cleared,
            archive_path,
        } = response
        else {
            return Err(unexpected().into());
        };
        Ok(archive_path.map_or_else(
            || format!("Cleared buffer ({lines_cleared} lines removed)"),
            |path| format!("Cleared buffer ({lines_cleared} lines archived to {path})"),
        ))
    }

    /// List system serial ports and managed connections.
    #[tool(
        description = "List system serial ports and managed connections with their state.",
        annotations(read_only_hint = true)
    )]
    async fn serial_list(&self) -> Result<String, ToolExecutionError> {
        let ResponsePayload::HardwarePorts(hardware) =
            self.run(RequestPayload::ListHardware).await?
        else {
            return Err(unexpected().into());
        };
        let ResponsePayload::PortList(managed) = self.run(RequestPayload::ListPorts).await? else {
            return Err(unexpected().into());
        };

        let mut out = String::from("System ports:\n");
        if hardware.is_empty() {
            out.push_str("  (none detected)\n");
        }
        for port in &hardware {
            let _ = writeln!(out, "  {port}");
        }

        out.push_str("\nManaged connections:\n");
        if managed.is_empty() {
            out.push_str("  (none)\n");
        }
        for port in &managed {
            let _ = writeln!(
                out,
                "  {} — {:?} ({} lines)",
                port.name, port.state, port.total_lines
            );
        }
        Ok(out)
    }

    /// Open or reconfigure a serial port.
    #[tool(
        description = "Open a serial port with the given line settings. Idempotent: reconfigures the port when it is already open.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn serial_open(
        &self,
        Parameters(params): Parameters<ConfigurePortParams>,
    ) -> Result<String, ToolExecutionError> {
        let settings = PortSettings {
            baudrate: params.baudrate,
            data_bits: data_bits(params.data_bits)?,
            parity: params.parity,
            stop_bits: stop_bits(params.stop_bits)?,
            flow_control: params.flow_control,
        };

        let response = self
            .run(RequestPayload::OpenPort {
                name: params.port_name,
                settings,
            })
            .await?;

        Ok(match response {
            ResponsePayload::PortOpened {
                name,
                config_summary,
            } => format!("Opened {name} ({config_summary})"),
            ResponsePayload::PortReconfigured {
                name,
                config_summary,
            } => format!("Reconfigured {name} ({config_summary})"),
            _ => return Err(unexpected().into()),
        })
    }

    /// Close a managed serial port.
    #[tool(
        description = "Close a managed serial port and stop capturing from it.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn serial_close(
        &self,
        Parameters(params): Parameters<PortParams>,
    ) -> Result<String, ToolExecutionError> {
        let response = self
            .run(RequestPayload::ClosePort {
                name: params.port_name,
            })
            .await?;
        let ResponsePayload::PortClosed { name } = response else {
            return Err(unexpected().into());
        };
        Ok(format!("Closed {name}"))
    }

    /// Write data to a serial port.
    #[tool(
        description = "Write data to a serial port. Accepts UTF-8 text or hex-encoded bytes prefixed with 0x.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn serial_write(
        &self,
        Parameters(params): Parameters<WritePortParams>,
    ) -> Result<String, ToolExecutionError> {
        let port = params.port_name.clone();
        let response = self
            .run(RequestPayload::WriteData {
                port: params.port_name,
                data: params.data,
                is_hex: false,
            })
            .await?;
        let ResponsePayload::WriteSuccess { bytes_written } = response else {
            return Err(unexpected().into());
        };
        Ok(format!("Wrote {bytes_written} bytes to {port}"))
    }

    /// Set DTR and RTS signals.
    #[tool(
        description = "Set the DTR and/or RTS control lines of a serial port.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn serial_signal(
        &self,
        Parameters(params): Parameters<SetControlLinesParams>,
    ) -> Result<String, ToolExecutionError> {
        let port = params.port_name.clone();
        let response = self
            .run(RequestPayload::SetSignal {
                port: params.port_name,
                dtr: params.dtr,
                rts: params.rts,
            })
            .await?;
        let ResponsePayload::SignalSuccess { applied } = response else {
            return Err(unexpected().into());
        };
        Ok(format!("Set {} on {port}", applied.join(", ")))
    }

    /// Send an RS-232 BREAK pulse.
    #[tool(
        description = "Send an RS-232 BREAK pulse (TX held low) for a duration in milliseconds (default 250).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn serial_break(
        &self,
        Parameters(params): Parameters<SerialBreakParams>,
    ) -> Result<String, ToolExecutionError> {
        let port = params.port_name.clone();
        let response = self
            .run(RequestPayload::SendBreak {
                port: params.port_name,
                duration_ms: params.duration_ms,
            })
            .await?;
        let ResponsePayload::BreakSuccess { duration_ms } = response else {
            return Err(unexpected().into());
        };
        Ok(format!(
            "Sent serial BREAK pulse ({duration_ms}ms) on {port}"
        ))
    }

    /// Send a file over serial.
    #[tool(
        description = "Send a file over a serial port using ZMODEM (default), YMODEM, XMODEM-1K, XMODEM-CRC or XMODEM.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn serial_send_file(
        &self,
        Parameters(params): Parameters<SerialSendFileParams>,
    ) -> Result<String, ToolExecutionError> {
        let port = params.port_name.clone();
        let response = self
            .run(RequestPayload::SendFile {
                port: params.port_name,
                file_path: params.file_path,
                protocol: params.protocol.unwrap_or(FileTransferProtocol::Zmodem),
            })
            .await?;
        let ResponsePayload::TransferSuccess {
            bytes_transferred,
            file_name,
            protocol,
            ..
        } = response
        else {
            return Err(unexpected().into());
        };
        Ok(format!(
            "Sent {bytes_transferred} bytes ('{file_name}') to {port} via {protocol:?}"
        ))
    }

    /// Receive a file over serial.
    #[tool(
        description = "Receive a file over a serial port using ZMODEM (default), YMODEM, XMODEM-1K, XMODEM-CRC or XMODEM.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn serial_receive_file(
        &self,
        Parameters(params): Parameters<SerialReceiveFileParams>,
    ) -> Result<String, ToolExecutionError> {
        let port = params.port_name.clone();
        let response = self
            .run(RequestPayload::ReceiveFile {
                port: params.port_name,
                output_dir: params.output_dir.unwrap_or_else(|| ".".to_string()),
                protocol: params.protocol.unwrap_or(FileTransferProtocol::Zmodem),
            })
            .await?;
        let ResponsePayload::TransferSuccess {
            bytes_transferred,
            file_name,
            path,
            ..
        } = response
        else {
            return Err(unexpected().into());
        };
        let location = path.unwrap_or(file_name);
        Ok(format!(
            "Received {bytes_transferred} bytes from {port}, saved to '{location}'"
        ))
    }

    /// Run a macro sequence.
    #[tool(
        description = "Run a macro sequence on a serial port. Built-in: 'reset', 'enter_bootloader', 'break'; user-defined macros come from the configuration file.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn serial_macro(
        &self,
        Parameters(params): Parameters<TriggerMacroParams>,
    ) -> Result<String, ToolExecutionError> {
        let port = params.port_name.clone();
        let name = params.macro_name.clone();
        let response = self
            .run(RequestPayload::ExecuteMacro {
                port: params.port_name,
                macro_name: params.macro_name,
            })
            .await?;
        let ResponsePayload::MacroSuccess { executed_steps } = response else {
            return Err(unexpected().into());
        };
        Ok(format!(
            "Executed '{name}' on {port} ({} steps: {})",
            executed_steps.len(),
            executed_steps.join(" → ")
        ))
    }
}

#[cfg(feature = "monitor")]
#[tool_router(router = monitor_tools, vis = "pub")]
impl DevSerialServer {
    /// Open a GUI monitor window.
    #[tool(
        description = "Open a native GUI monitor window showing live serial data. The user can send data from that window.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn serial_monitor_open(
        &self,
        Parameters(params): Parameters<PortParams>,
    ) -> Result<String, ToolExecutionError> {
        self.monitor_open_impl(&params.port_name).await
    }

    /// Close a GUI monitor window.
    #[tool(
        description = "Close the native GUI monitor window for a serial port.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn serial_monitor_close(
        &self,
        Parameters(params): Parameters<PortParams>,
    ) -> Result<String, ToolExecutionError> {
        self.monitor_close_impl(&params.port_name).await
    }
}

#[cfg(feature = "esp")]
#[tool_router(router = esp_tools, vis = "pub")]
impl DevSerialServer {
    /// Flash firmware to an ESP device.
    #[tool(
        description = "Flash firmware to an ESP device via espflash. The port is released for the duration and reopened afterwards.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn serial_esp_flash(
        &self,
        Parameters(params): Parameters<EspFlashParams>,
    ) -> Result<String, ToolExecutionError> {
        self.esp_output(RequestPayload::EspFlash {
            port: params.port_name,
            firmware_path: params.firmware_path,
            baud: params.baud,
        })
        .await
    }

    /// Get ESP chip and board information.
    #[tool(
        description = "Get ESP chip and board information (chip type, flash size, MAC).",
        annotations(read_only_hint = true)
    )]
    async fn serial_esp_info(
        &self,
        Parameters(params): Parameters<PortParams>,
    ) -> Result<String, ToolExecutionError> {
        self.esp_output(RequestPayload::EspInfo {
            port: params.port_name,
        })
        .await
    }

    /// Erase the entire flash of an ESP device.
    #[tool(
        description = "Erase the entire flash of an ESP device. This destroys all data on it.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn serial_esp_erase(
        &self,
        Parameters(params): Parameters<PortParams>,
    ) -> Result<String, ToolExecutionError> {
        self.esp_output(RequestPayload::EspErase {
            port: params.port_name,
        })
        .await
    }

    /// Write a raw binary to a flash address.
    #[tool(
        description = "Write a raw binary file to a specific flash address on an ESP device. Use for bootloaders, partition tables or NVS images.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn serial_esp_write_bin(
        &self,
        Parameters(params): Parameters<EspWriteBinParams>,
    ) -> Result<String, ToolExecutionError> {
        self.esp_output(RequestPayload::EspWriteBin {
            port: params.port_name,
            file_path: params.file_path,
            address: params.address,
        })
        .await
    }
}

#[cfg(feature = "esp")]
impl DevSerialServer {
    async fn esp_output(&self, payload: RequestPayload) -> Result<String, ToolExecutionError> {
        let ResponsePayload::EspSuccess(output) = self.run(payload).await? else {
            return Err(unexpected().into());
        };
        Ok(output)
    }
}

// ------------------------------------------------------------------ rendering

fn unexpected() -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error("unexpected response for this operation".to_string(), None)
}

/// Render a page of lines with the header a model can use to continue reading.
fn render_lines(page: &LinesPage, timestamps: bool) -> String {
    use std::fmt::Write as _;

    if page.lines.is_empty() {
        return format!(
            "[no new lines after {}, {} total]",
            page.next_after_id, page.total_lines
        );
    }

    let first = page.lines.first().map_or(0, |l| l.id);
    let last = page.lines.last().map_or(0, |l| l.id);
    let mut out = format!("[lines {first}-{last} of {} total]\n", page.total_lines);
    for line in &page.lines {
        if timestamps {
            let ts = crate::export::format_time_of_day(line.timestamp_ns);
            let _ = writeln!(out, "{ts} {}", line.payload);
        } else {
            out.push_str(&line.payload);
            out.push('\n');
        }
    }
    out
}

// ------------------------------------------------------------------ monitor

#[cfg(feature = "monitor")]
impl DevSerialServer {
    async fn monitor_open_impl(&self, port_name: &str) -> Result<String, ToolExecutionError> {
        // Fails early when the port is not managed.
        let ResponsePayload::Status { state, .. } = self
            .run(RequestPayload::GetStatus {
                port: port_name.to_string(),
            })
            .await?
        else {
            return Err(unexpected().into());
        };

        if self.monitor_is_open(port_name) {
            return Err(bad_params(format!(
                "a monitor window is already open for '{port_name}'"
            ))
            .into());
        }

        let db_path = crate::paths::port_db_path(self.engine.data_dir(), port_name);
        let mut handle = crate::monitor::spawn_monitor(port_name, &db_path, &format!("{state:?}"))
            .map_err(|e| {
                rmcp::ErrorData::internal_error(format!("failed to spawn monitor: {e}"), None)
            })?;

        if let Some(stdout) = handle.take_stdout() {
            self.spawn_event_pump(port_name.to_string(), stdout);
        }
        if let Some(stdin) = handle.take_stdin() {
            self.spawn_change_notifier(port_name.to_string(), stdin);
        }

        self.monitors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(port_name.to_string(), MonitorState { handle });

        Ok(format!("Monitor opened for {port_name}"))
    }

    #[allow(clippy::unused_async)] // symmetry with the feature-less variant
    async fn monitor_close_impl(&self, port_name: &str) -> Result<String, ToolExecutionError> {
        let monitor = self
            .monitors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(port_name);

        match monitor {
            Some(mut state) => {
                state.handle.close();
                Ok(format!("Monitor closed for {port_name}"))
            }
            None => Err(bad_params(format!("no monitor window is open for '{port_name}'")).into()),
        }
    }

    fn monitor_is_open(&self, port_name: &str) -> bool {
        self.monitors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(port_name)
    }

    /// Turn user actions reported by a monitor window into engine operations.
    fn spawn_event_pump(&self, port: String, stdout: std::process::ChildStdout) {
        let engine = self.engine.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};

            let Ok(stdout) = tokio::process::ChildStdout::from_std(stdout) else {
                tracing::warn!(port = %port, "could not read monitor output");
                return;
            };
            let mut lines = BufReader::new(stdout).lines();

            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                match crate::gui_ipc::MonitorEvent::decode(&line) {
                    Ok(event) => {
                        let payload = monitor_event_payload(&port, event);
                        if let Err(e) = engine.execute(payload).await {
                            tracing::warn!(port = %port, error = %e, "monitor action failed");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(port = %port, error = %e, "unreadable monitor event");
                    }
                }
            }
        });
    }

    /// Notify a monitor window when new lines arrive.
    fn spawn_change_notifier(&self, port: String, stdin: std::process::ChildStdin) {
        let engine = self.engine.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;

            let Ok(mut pipe) = tokio::process::ChildStdin::from_std(stdin) else {
                return;
            };
            let mut last_id = 0;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                // The MCP engine may be an IPC proxy. Read the latest retained
                // id through the shared engine instead of the local port manager.
                let Ok(ResponsePayload::Lines(page)) = engine
                    .execute(RequestPayload::ReadLines {
                        port: port.clone(),
                        window: ReadWindow {
                            tail: Some(1),
                            limit: Some(1),
                            ..ReadWindow::default()
                        },
                    })
                    .await
                else {
                    break;
                };
                let current = page.next_after_id;
                if current != last_id {
                    last_id = current;
                    if pipe.write_all(b"\n").await.is_err() {
                        break; // window closed
                    }
                }
            }
        });
    }
}

/// Translate a monitor window action into an engine operation.
#[cfg(feature = "monitor")]
fn monitor_event_payload(port: &str, event: crate::gui_ipc::MonitorEvent) -> RequestPayload {
    use crate::gui_ipc::MonitorEvent;
    match event {
        MonitorEvent::Macro { name } => RequestPayload::ExecuteMacro {
            port: port.to_string(),
            macro_name: name,
        },
        MonitorEvent::Signal { dtr, rts } => RequestPayload::SetSignal {
            port: port.to_string(),
            dtr,
            rts,
        },
        MonitorEvent::Write { hex } => RequestPayload::WriteData {
            port: port.to_string(),
            data: hex,
            is_hex: true,
        },
        MonitorEvent::Reconfigure { settings } => RequestPayload::ReconfigurePort {
            name: port.to_string(),
            settings,
        },
        MonitorEvent::Break { duration_ms } => RequestPayload::SendBreak {
            port: port.to_string(),
            duration_ms,
        },
        MonitorEvent::SendFile { path, protocol } => RequestPayload::SendFile {
            port: port.to_string(),
            file_path: path,
            protocol,
        },
        MonitorEvent::ReceiveFile { dir, protocol } => RequestPayload::ReceiveFile {
            port: port.to_string(),
            output_dir: dir,
            protocol,
        },
    }
}

// ------------------------------------------------------------------ handler

#[tool_handler(router = self.tool_router)]
#[allow(clippy::manual_async_fn)]
impl ServerHandler for DevSerialServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("devserial", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "Serial hardware bridge. I monitor serial ports, persist all data to a queryable \
             database, and provide grep-like search, export, and hardware control tools.",
        )
    }

    #[allow(clippy::manual_async_fn)]
    fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<
        Output = Result<rmcp::model::ListResourcesResult, rmcp::model::ErrorData>,
    > + Send
    + '_ {
        async {
            let ResponsePayload::PortList(ports) = self.run(RequestPayload::ListPorts).await?
            else {
                return Err(unexpected());
            };
            let resources = ports
                .iter()
                .map(|p| {
                    rmcp::model::Resource::new(
                        format!("serial://{}/status", p.name),
                        format!("{} status", p.name),
                    )
                    .with_description(format!("Connection state for {}", p.name))
                    .with_mime_type("application/json")
                })
                .collect();
            Ok(rmcp::model::ListResourcesResult::with_all_items(resources))
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn read_resource(
        &self,
        request: rmcp::model::ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<
        Output = Result<rmcp::model::ReadResourceResponse, rmcp::model::ErrorData>,
    > + Send
    + '_ {
        async move {
            let uri = request.uri.clone();
            let port_name = uri
                .strip_prefix("serial://")
                .and_then(|s| s.strip_suffix("/status"))
                .ok_or_else(|| bad_params(format!("invalid resource URI: {uri}")))?;

            let ResponsePayload::Status { name, state, stats } = self
                .run(RequestPayload::GetStatus {
                    port: port_name.to_string(),
                })
                .await?
            else {
                return Err(unexpected());
            };

            let json = serde_json::json!({
                "port": name,
                "state": format!("{state:?}"),
                "total_lines": stats.total_lines,
                "total_bytes": stats.total_bytes,
                "db_size_bytes": stats.db_size_bytes,
            });

            // rmcp 3 answers with ReadResourceResponse, an enum whose Complete
            // variant carries the old result type. Nothing here ever asks the
            // caller for input, so the conversion is the whole change.
            Ok(rmcp::model::ReadResourceResult::new(vec![
                rmcp::model::ResourceContents::TextResourceContents {
                    uri,
                    mime_type: Some("application/json".into()),
                    text: serde_json::to_string_pretty(&json).unwrap_or_default(),
                    meta: None,
                },
            ])
            .into())
        }
    }
}

/// Upper bounds advertised in tool descriptions, kept next to the protocol
/// constants they mirror so the two cannot drift.
#[cfg(test)]
mod limit_docs {
    use crate::protocol::{
        DEFAULT_READ_LIMIT, DEFAULT_SEARCH_LIMIT, MAX_READ_LIMIT, MAX_SEARCH_LIMIT,
    };
    use crate::serial_params::DEFAULT_BREAK_MS;

    /// Tool descriptions are string literals, so the numbers in them cannot be
    /// interpolated from the constants. This test fails if a constant changes
    /// without its description being updated.
    #[test]
    fn documented_limits_match_the_constants() {
        assert_eq!(
            DEFAULT_READ_LIMIT, 100,
            "update the serial_read description"
        );
        assert_eq!(MAX_READ_LIMIT, 10_000, "update the serial_read description");
        assert_eq!(
            DEFAULT_SEARCH_LIMIT, 100,
            "update the serial_search description"
        );
        assert_eq!(
            MAX_SEARCH_LIMIT, 1000,
            "update the serial_search description"
        );
        assert_eq!(
            DEFAULT_BREAK_MS, 250,
            "update the serial_break descriptions"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PortConfig;
    use crate::port_manager::PortManagerHandle;
    use crate::storage::SqliteStorage;
    use crate::testutil::mock_serial::mock_serial;

    /// A server whose mock port stays alive for as long as the test does.
    ///
    /// Dropping the control closes the mock's channel, and the port is then a
    /// source at its end. The reader now calls that a disconnect, which is
    /// right and made this fixture's flaw visible: every test here used to run
    /// against a port that had died the moment setup returned.
    struct TestServer {
        server: DevSerialServer,
        _ctrl: crate::testutil::mock_serial::MockSerialControl,
    }

    impl std::ops::Deref for TestServer {
        type Target = DevSerialServer;
        fn deref(&self) -> &Self::Target {
            &self.server
        }
    }

    async fn setup() -> TestServer {
        let pm = PortManagerHandle::new();
        let server = DevSerialServer::with_port_manager(pm);
        let (mock, ctrl) = mock_serial(4096);
        let storage = Arc::new(std::sync::Mutex::new(SqliteStorage::open_memory().unwrap()));
        server
            .engine
            .port_manager()
            .open("/dev/mock".into(), mock, PortConfig::default(), storage)
            .await
            .unwrap();
        ctrl.feed_lines(&["[INFO] boot", "[ERROR] failure", "[INFO] done"])
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        TestServer {
            server,
            _ctrl: ctrl,
        }
    }

    #[tokio::test]
    async fn tool_router_reflects_monitor_feature_and_tool_annotations() {
        let server = DevSerialServer::with_port_manager(PortManagerHandle::new());
        let names: Vec<_> = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect();

        #[cfg(feature = "monitor")]
        {
            assert!(names.iter().any(|name| name == "serial_monitor_open"));
            assert!(names.iter().any(|name| name == "serial_monitor_close"));
        }
        #[cfg(not(feature = "monitor"))]
        {
            assert!(!names.iter().any(|name| name == "serial_monitor_open"));
            assert!(!names.iter().any(|name| name == "serial_monitor_close"));
        }

        let read = server.tool_router.get("serial_read").unwrap();
        assert_eq!(
            read.annotations.as_ref().unwrap().read_only_hint,
            Some(true)
        );

        let clear = server.tool_router.get("serial_clear").unwrap();
        let clear_annotations = clear.annotations.as_ref().unwrap();
        assert_eq!(clear_annotations.read_only_hint, Some(false));
        assert_eq!(clear_annotations.destructive_hint, Some(true));
        assert_eq!(clear_annotations.idempotent_hint, Some(false));

        let open = server.tool_router.get("serial_open").unwrap();
        assert_eq!(
            open.annotations.as_ref().unwrap().idempotent_hint,
            Some(true)
        );

        #[cfg(feature = "esp")]
        {
            assert_eq!(
                server
                    .tool_router
                    .get("serial_esp_info")
                    .unwrap()
                    .annotations
                    .as_ref()
                    .unwrap()
                    .read_only_hint,
                Some(true)
            );
            assert_eq!(
                server
                    .tool_router
                    .get("serial_esp_erase")
                    .unwrap()
                    .annotations
                    .as_ref()
                    .unwrap()
                    .destructive_hint,
                Some(true)
            );
        }
    }

    #[test]
    fn execution_failures_become_tool_results() {
        let error = ToolExecutionError(bad_params("port was not found"));
        let response = Result::<String, ToolExecutionError>::Err(error)
            .into_call_tool_result()
            .unwrap();
        let rmcp::model::CallToolResponse::Complete(result) = response else {
            panic!("expected a complete tool result");
        };
        assert_eq!(result.is_error, Some(true));
        assert!(
            result
                .content
                .iter()
                .filter_map(rmcp::model::ContentBlock::as_text)
                .any(|text| text.text.contains("not found"))
        );
        assert_eq!(
            serde_json::to_value(&result).unwrap()["isError"],
            serde_json::Value::Bool(true)
        );
    }

    fn read_params(port: &str) -> ReadBufferParams {
        ReadBufferParams {
            port_name: port.to_string(),
            start_line: None,
            after_line: None,
            max_lines: None,
            include_timestamps: None,
            wait_ms: None,
            since_time: None,
        }
    }

    #[tokio::test]
    async fn read_returns_header_and_lines() {
        let server = setup().await;
        let out = server
            .serial_read(Parameters(read_params("/dev/mock")))
            .await
            .unwrap();
        assert!(out.starts_with("[lines 1-3 of 3 total]"));
        assert!(out.contains("[ERROR] failure"));
    }

    #[tokio::test]
    async fn read_with_timestamps() {
        let server = setup().await;
        let out = server
            .serial_read(Parameters(ReadBufferParams {
                include_timestamps: Some(true),
                ..read_params("/dev/mock")
            }))
            .await
            .unwrap();
        assert!(out.contains(':'), "expected a clock time in the output");
    }

    #[tokio::test]
    async fn read_after_line_reports_no_new_lines() {
        let server = setup().await;
        let out = server
            .serial_read(Parameters(ReadBufferParams {
                after_line: Some(3),
                ..read_params("/dev/mock")
            }))
            .await
            .unwrap();
        assert!(out.contains("no new lines"));
    }

    #[tokio::test]
    async fn unknown_port_is_an_error() {
        let server = setup().await;
        let err = server
            .serial_read(Parameters(read_params("/dev/absent")))
            .await
            .unwrap_err();
        assert!(err.message.contains("not found"));
    }

    #[tokio::test]
    async fn status_is_json() {
        let server = setup().await;
        let out = server
            .serial_status(Parameters(PortParams {
                port_name: "/dev/mock".into(),
            }))
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(json["total_lines"], 3);
        assert_eq!(json["state"], "Connected");
    }

    #[tokio::test]
    async fn search_modes_behave_differently() {
        let server = setup().await;
        let base = SearchBufferParams {
            port_name: "/dev/mock".into(),
            query: "[INFO] boot".into(),
            query_type: None,
            start_time: None,
            end_time: None,
            max_results: None,
        };

        let substring = server.serial_search(Parameters(base)).await.unwrap();
        assert!(substring.contains("[INFO] boot"));

        let regex = server
            .serial_search(Parameters(SearchBufferParams {
                port_name: "/dev/mock".into(),
                query: r"^\[ERROR\]".into(),
                query_type: Some(SearchMode::Regex),
                start_time: None,
                end_time: None,
                max_results: None,
            }))
            .await
            .unwrap();
        assert!(regex.contains("failure"));
        assert!(!regex.contains("boot"));
    }

    #[tokio::test]
    async fn invalid_regex_is_invalid_params() {
        let server = setup().await;
        let err = server
            .serial_search(Parameters(SearchBufferParams {
                port_name: "/dev/mock".into(),
                query: "[".into(),
                query_type: Some(SearchMode::Regex),
                start_time: None,
                end_time: None,
                max_results: None,
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("regex"));
    }

    #[tokio::test]
    async fn export_uses_the_shared_format() {
        let server = setup().await;
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("export.csv");

        let out = server
            .serial_export(Parameters(ExportBufferParams {
                port_name: "/dev/mock".into(),
                file_format: Some(ExportFormat::Csv),
                start_line: None,
                end_line: None,
                output_path: out_path.display().to_string(),
            }))
            .await
            .unwrap();
        assert!(out.contains("Exported 3 lines"));

        let content = std::fs::read_to_string(&out_path).unwrap();
        assert!(content.starts_with("id,timestamp,timestamp_ns,payload\n"));
    }

    #[tokio::test]
    async fn export_rejects_traversal() {
        let server = setup().await;
        let err = server
            .serial_export(Parameters(ExportBufferParams {
                port_name: "/dev/mock".into(),
                file_format: None,
                start_line: None,
                end_line: None,
                output_path: "../evil.txt".into(),
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains(".."));
    }

    #[tokio::test]
    async fn invalid_baud_is_invalid_params() {
        let server = setup().await;
        let err = server
            .serial_open(Parameters(ConfigurePortParams {
                port_name: "/dev/mock".into(),
                baudrate: Some(0),
                data_bits: None,
                parity: None,
                stop_bits: None,
                flow_control: None,
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("baud"));
    }

    #[tokio::test]
    async fn invalid_data_bits_are_rejected_before_reaching_hardware() {
        let server = setup().await;
        let err = server
            .serial_open(Parameters(ConfigurePortParams {
                port_name: "/dev/mock".into(),
                baudrate: None,
                data_bits: Some(9),
                parity: None,
                stop_bits: None,
                flow_control: None,
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("data bits"));
    }

    #[tokio::test]
    async fn signal_needs_at_least_one_line() {
        let server = setup().await;
        let err = server
            .serial_signal(Parameters(SetControlLinesParams {
                port_name: "/dev/mock".into(),
                dtr: None,
                rts: None,
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("dtr"));
    }

    #[tokio::test]
    async fn clear_reports_removed_lines() {
        let server = setup().await;
        let out = server
            .serial_clear(Parameters(ClearBufferParams {
                port_name: "/dev/mock".into(),
                archive_current: None,
            }))
            .await
            .unwrap();
        assert!(out.contains("3 lines removed"));
    }

    #[tokio::test]
    async fn list_shows_managed_ports() {
        let server = setup().await;
        let out = server.serial_list().await.unwrap();
        assert!(out.contains("Managed connections:"));
        assert!(out.contains("/dev/mock"));
    }

    #[tokio::test]
    async fn mock_ports_have_no_hardware_handle() {
        let server = setup().await;
        let err = server
            .serial_break(Parameters(SerialBreakParams {
                port_name: "/dev/mock".into(),
                duration_ms: Some(10),
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("no hardware handle"));
    }

    #[tokio::test]
    async fn macro_on_unknown_name_lists_alternatives() {
        let server = setup().await;
        let err = server
            .serial_macro(Parameters(TriggerMacroParams {
                port_name: "/dev/mock".into(),
                macro_name: "nope".into(),
            }))
            .await
            .unwrap_err();
        assert!(err.message.contains("reset"));
    }

    #[cfg(feature = "monitor")]
    #[test]
    fn monitor_events_map_onto_operations() {
        use crate::gui_ipc::MonitorEvent;

        let payload = monitor_event_payload(
            "/dev/x",
            MonitorEvent::Macro {
                name: "reset".into(),
            },
        );
        assert!(matches!(
            payload,
            RequestPayload::ExecuteMacro { ref macro_name, .. } if macro_name == "reset"
        ));

        let payload = monitor_event_payload("/dev/x", MonitorEvent::Write { hex: "41".into() });
        assert!(matches!(
            payload,
            RequestPayload::WriteData { is_hex: true, .. }
        ));
    }
}
