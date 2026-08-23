// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::sync::Arc;

use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::port_manager::PortManagerHandle;

/// Maximum lines returned by `serial_read`.
const MAX_READ_LINES: u32 = 10_000;
/// Default lines returned by `serial_read`.
const DEFAULT_READ_LINES: u32 = 100;
/// Maximum results returned by `serial_search`.
const MAX_SEARCH_RESULTS: u32 = 1000;
/// Default results returned by `serial_search`.
const DEFAULT_SEARCH_RESULTS: u32 = 100;

/// MCP server for serial hardware bridging.
#[derive(Clone)]
pub struct DevSerialServer {
    #[allow(dead_code)] // Used by #[tool_router] proc macro
    tool_router: ToolRouter<Self>,
    port_manager: PortManagerHandle,
    config: crate::config::Config,
    state_db: Arc<std::sync::Mutex<crate::state::StateDb>>,
    #[cfg(feature = "monitor")]
    monitors: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, MonitorState>>>,
}

/// State for a running monitor (feature-gated).
#[cfg(feature = "monitor")]
struct MonitorState {
    handle: crate::monitor::MonitorHandle,
}

impl DevSerialServer {
    /// Create a new server with config.
    ///
    /// # Panics
    /// Panics if `config.db` cannot be opened in the data directory.
    #[must_use]
    pub fn new(port_manager: PortManagerHandle, config: crate::config::Config) -> Self {
        let state_db =
            crate::state::StateDb::open(&config.global.data_dir).expect("failed to open config.db");
        Self {
            tool_router: Self::tool_router(),
            port_manager,
            config,
            state_db: Arc::new(std::sync::Mutex::new(state_db)),
            #[cfg(feature = "monitor")]
            monitors: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Create a new server with default config (for testing).
    ///
    /// # Panics
    /// Panics if the in-memory state database cannot be created.
    #[must_use]
    pub fn with_port_manager(port_manager: PortManagerHandle) -> Self {
        let state_db = crate::state::StateDb::open_memory().expect("failed to open memory state");
        Self {
            tool_router: Self::tool_router(),
            port_manager,
            config: crate::config::Config::default(),
            state_db: Arc::new(std::sync::Mutex::new(state_db)),
            #[cfg(feature = "monitor")]
            monitors: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Get the configured data directory.
    fn data_dir(&self) -> std::path::PathBuf {
        self.config.global.data_dir.clone()
    }

    /// Insert a separator/log line into a port's buffer.
    async fn log_to_buffer(&self, port_name: &str, message: &str) {
        if let Ok(storage) = self.port_manager.get_storage(port_name).await {
            let ts = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
            let msg = message.to_string();
            tokio::task::spawn_blocking(move || {
                let s = storage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                s.insert_lines(&[(ts, &msg)]).ok();
            })
            .await
            .ok();
        }
    }
}

/// Parameters for `serial_read`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(
    description = "Read lines from a serial port's persistent buffer. Response includes metadata header: [lines X-Y of Z total]. Use after_line for incremental reads without tracking IDs manually."
)]
pub struct ReadBufferParams {
    /// Serial port name (e.g. `/dev/ttyUSB0`).
    #[schemars(description = "Serial port name (e.g. /dev/ttyUSB0)")]
    pub port_name: String,
    /// Starting line number (1-based). Negative = from end (-50 = last 50 lines). Default: 1.
    #[schemars(
        description = "Starting line ID (1-based, default: 1). Negative counts from end: -50 = last 50 lines. Mutually exclusive with after_line."
    )]
    pub start_line: Option<i64>,
    /// Read lines after this ID (exclusive). Ideal for incremental polling — pass the last ID you received.
    #[schemars(
        description = "Read lines after this ID (exclusive). Use the Y value from the previous response header '[lines X-Y of Z total]' to get only new data since your last read."
    )]
    pub after_line: Option<i64>,
    /// Maximum number of lines to return. Defaults to 100, max 10000.
    #[schemars(description = "Max lines to return (default: 100, max: 10000)")]
    pub max_lines: Option<u32>,
    /// Whether to include timestamps in output.
    #[schemars(description = "Include ISO timestamps before each line (default: false)")]
    pub include_timestamps: Option<bool>,
    /// Wait up to this many milliseconds for new data before returning empty. 0 = no wait (default).
    #[schemars(
        description = "Wait up to N ms for new data if buffer is empty at requested position (0 = return immediately, default). Useful to avoid empty polling loops."
    )]
    pub wait_ms: Option<u64>,
    /// Only return lines with timestamps after this ISO 8601 time.
    #[schemars(
        description = "Only return lines newer than this timestamp (ISO 8601, e.g. 2026-05-10T23:00:00Z). Useful for 'show me everything since the last flash'."
    )]
    pub since_time: Option<String>,
}

/// Parameters for `serial_status`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(
    description = "Get port status: connection state, total lines buffered, total bytes, last activity timestamp, and DB size."
)]
pub struct GetStreamStatsParams {
    /// Serial port name (e.g. `/dev/ttyUSB0`).
    #[schemars(description = "Serial port name (e.g. /dev/ttyUSB0)")]
    pub port_name: String,
}

/// Parameters for `serial_search`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(
    description = "Search serial buffer with grep-like queries. Returns matching lines with line numbers and timestamps. Use time bounds to narrow large buffers."
)]
pub struct SearchBufferParams {
    /// Serial port name.
    #[schemars(description = "Serial port name (e.g. /dev/ttyUSB0)")]
    pub port_name: String,
    /// Search query string.
    #[schemars(description = "Search query (pattern or literal text)")]
    pub query: String,
    /// Query type: exact, substring, or regex.
    #[schemars(
        description = "Query type: 'exact' (full match), 'substring' (contains, default), or 'regex' (Rust regex syntax)"
    )]
    pub query_type: Option<String>,
    /// Start time bound (ISO 8601).
    #[schemars(description = "Optional start time bound (ISO 8601, e.g. 2026-01-15T10:00:00Z)")]
    pub start_time: Option<String>,
    /// End time bound (ISO 8601).
    #[schemars(description = "Optional end time bound (ISO 8601, e.g. 2026-01-15T12:00:00Z)")]
    pub end_time: Option<String>,
    /// Maximum results to return.
    #[schemars(description = "Max results to return (default: 100, max: 1000)")]
    pub max_results: Option<u32>,
}

/// Parameters for `serial_export`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Export serial buffer to a file")]
pub struct ExportBufferParams {
    /// Serial port name.
    #[schemars(description = "Serial port name (e.g. /dev/ttyUSB0)")]
    pub port_name: String,
    /// Output format: txt, csv, or jsonl.
    #[schemars(description = "File format: 'txt', 'csv', or 'jsonl' (default: txt)")]
    pub file_format: Option<String>,
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
#[schemars(description = "Clear a serial port's buffer with optional archive")]
pub struct ClearBufferParams {
    /// Serial port name.
    #[schemars(description = "Serial port name (e.g. /dev/ttyUSB0)")]
    pub port_name: String,
    /// Whether to archive the current data before clearing.
    #[schemars(description = "Archive current data before clearing (default: false)")]
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
    #[schemars(description = "Data bits: 5, 6, 7, or 8 (default: 8)")]
    pub data_bits: Option<u8>,
    /// Parity: none, odd, even.
    #[schemars(description = "Parity: 'none', 'odd', or 'even' (default: none)")]
    pub parity: Option<String>,
    /// Stop bits (1, 2).
    #[schemars(description = "Stop bits: 1 or 2 (default: 1)")]
    pub stop_bits: Option<u8>,
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
#[schemars(
    description = "Send an RS-232 serial BREAK signal (holding TX low for a specified duration)"
)]
pub struct SerialBreakParams {
    /// Serial port name.
    #[schemars(description = "Serial port name")]
    pub port_name: String,
    /// Duration in milliseconds (default: 250ms).
    #[schemars(description = "Break signal duration in milliseconds (default: 250)")]
    pub duration_ms: Option<u64>,
}

/// Parameters for `serial_send_file`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(
    description = "Send a file over serial port using modem transfer protocols (XMODEM, YMODEM, ZMODEM)"
)]
pub struct SerialSendFileParams {
    /// Serial port name.
    #[schemars(description = "Serial port name")]
    pub port_name: String,
    /// Absolute path to the file to send.
    #[schemars(description = "Path to file to send")]
    pub file_path: String,
    /// Transfer protocol ('zmodem', 'ymodem', 'xmodem-1k', 'xmodem-crc', 'xmodem'). Default: 'zmodem'.
    #[schemars(
        description = "Transfer protocol (zmodem, ymodem, xmodem-1k, xmodem-crc, xmodem). Default: zmodem"
    )]
    pub protocol: Option<String>,
}

/// Parameters for `serial_receive_file`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(
    description = "Receive a file over serial port using modem transfer protocols (XMODEM, YMODEM, ZMODEM)"
)]
pub struct SerialReceiveFileParams {
    /// Serial port name.
    #[schemars(description = "Serial port name")]
    pub port_name: String,
    /// Output directory where received file will be stored. Default: current directory.
    #[schemars(description = "Output directory for received file (default: .)")]
    pub output_dir: Option<String>,
    /// Transfer protocol ('zmodem', 'ymodem', 'xmodem-1k', 'xmodem-crc', 'xmodem'). Default: 'zmodem'.
    #[schemars(
        description = "Transfer protocol (zmodem, ymodem, xmodem-1k, xmodem-crc, xmodem). Default: zmodem"
    )]
    pub protocol: Option<String>,
}

/// Parameters for `serial_macro`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Execute a predefined or user-defined macro sequence on a serial port")]
pub struct TriggerMacroParams {
    /// Serial port name.
    #[schemars(description = "Serial port name")]
    pub port_name: String,
    /// Macro name (built-in: `reset`, `enter_bootloader`, `break`).
    #[schemars(
        description = "Macro name (built-in: 'reset', 'enter_bootloader', 'break', or user-defined)"
    )]
    pub macro_name: String,
}

/// Parameters for `serial_close`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Close a managed serial port")]
pub struct ClosePortParams {
    /// Serial port name.
    #[schemars(description = "Serial port name to close")]
    pub port_name: String,
}

/// Parameters for `serial_write`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(
    description = "Write data to a serial port. No newline is appended automatically — include \\r\\n in the data if needed."
)]
pub struct WritePortParams {
    /// Serial port name.
    #[schemars(description = "Serial port name")]
    pub port_name: String,
    /// Data to write. UTF-8 text, or hex-encoded bytes prefixed with 0x.
    #[schemars(
        description = "Data to write. UTF-8 text (include \\r\\n if needed), or hex bytes prefixed with '0x' (e.g. '0x0D0A')"
    )]
    pub data: String,
}

/// Parameters for `serial_monitor_open`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Open a GUI monitor window for a serial port")]
pub struct MonitorOpenParams {
    /// Serial port name (must already be open).
    #[schemars(description = "Serial port name (must be open via serial_open first)")]
    pub port_name: String,
}

/// Parameters for `serial_monitor_close`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Close the GUI monitor window for a serial port")]
pub struct MonitorCloseParams {
    /// Serial port name.
    #[schemars(description = "Serial port name whose monitor to close")]
    pub port_name: String,
}

/// Parameters for `serial_esp_flash`.
#[cfg(feature = "esp")]
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Flash firmware to an ESP device via espflash")]
pub struct EspFlashParams {
    /// Serial port name.
    #[schemars(description = "Serial port name (e.g. /dev/ttyUSB0)")]
    pub port_name: String,
    /// Path to the firmware file (ELF or binary).
    #[schemars(description = "Path to firmware file (ELF or .bin)")]
    pub firmware_path: String,
    /// Optional baud rate for flashing.
    #[schemars(description = "Flash baud rate (default: espflash default, typically 460800)")]
    pub baud: Option<u32>,
}

/// Parameters for `serial_esp_info`.
#[cfg(feature = "esp")]
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Get ESP chip/board information")]
pub struct EspInfoParams {
    /// Serial port name.
    #[schemars(description = "Serial port name (e.g. /dev/ttyUSB0)")]
    pub port_name: String,
}

/// Parameters for `serial_esp_erase`.
#[cfg(feature = "esp")]
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Erase entire flash of an ESP device")]
pub struct EspEraseParams {
    /// Serial port name.
    #[schemars(description = "Serial port name (e.g. /dev/ttyUSB0)")]
    pub port_name: String,
}

/// Parameters for `serial_esp_write_bin`.
#[cfg(feature = "esp")]
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Write a raw binary file to a specific flash address on an ESP device")]
pub struct EspWriteBinParams {
    /// Serial port name.
    #[schemars(description = "Serial port name (e.g. /dev/ttyUSB0)")]
    pub port_name: String,
    /// Path to the binary file.
    #[schemars(description = "Path to binary file (.bin)")]
    pub file_path: String,
    /// Flash address (hex, e.g. 0x1000).
    #[schemars(description = "Flash address in hex (e.g. 0x1000, 0x8000, 0x10000)")]
    pub address: String,
}

/// Decode a hex string (without 0x prefix) into bytes.
fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.replace(' ', "");
    if s.len() % 2 != 0 {
        return Err("odd number of hex digits".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

#[tool_router]
impl DevSerialServer {
    /// Read lines from a serial port's captured data.
    #[tool(
        description = "Read captured serial data. Returns metadata header + lines. Use after_line for efficient incremental polling."
    )]
    async fn serial_read(
        &self,
        Parameters(params): Parameters<ReadBufferParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let max = params
            .max_lines
            .unwrap_or(DEFAULT_READ_LINES)
            .min(MAX_READ_LINES);
        let timestamps = params.include_timestamps.unwrap_or(false);
        let wait_ms = params.wait_ms.unwrap_or(0);

        let storage = self
            .port_manager
            .get_storage(&params.port_name)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        // Resolve since_time to nanoseconds
        let since_ns = params
            .since_time
            .as_deref()
            .map(parse_iso_to_ns)
            .transpose()?;

        // Resolve start position
        let start = if since_ns.is_some() {
            1 // will use time-based query instead
        } else if let Some(after) = params.after_line {
            after + 1
        } else {
            let raw = params.start_line.unwrap_or(1);
            if raw < 0 {
                let total = i64::try_from(
                    storage
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get_stats()
                        .map_or(0, |s| s.total_lines),
                )
                .unwrap_or(0);
                (total + raw + 1).max(1)
            } else {
                raw.max(1)
            }
        };

        // Read with optional wait
        let lines = tokio::task::spawn_blocking({
            let storage = Arc::clone(&storage);
            move || {
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
                loop {
                    let s = storage
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let result = if let Some(ns) = since_ns {
                        s.search_time_range(ns, i64::MAX, max)?
                    } else {
                        s.read_lines(start, max)?
                    };
                    if !result.is_empty() || wait_ms == 0 {
                        return Ok(result);
                    }
                    drop(s);
                    if std::time::Instant::now() >= deadline {
                        return Ok(Vec::new());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        })
        .await
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
        .map_err(|e: crate::storage::StorageError| {
            rmcp::ErrorData::internal_error(e.to_string(), None)
        })?;

        // Get total for metadata
        let total_lines = storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_stats()
            .map_or(0, |s| s.total_lines);

        // Build response with metadata header
        let first_id = lines.first().map_or(0, |l| l.id);
        let last_id = lines.last().map_or(0, |l| l.id);
        let count = lines.len();

        let mut output = format!("[lines {first_id}-{last_id} of {total_lines} total]\n");

        for l in &lines {
            if timestamps {
                use std::fmt::Write;
                let ts =
                    chrono::DateTime::from_timestamp_nanos(l.timestamp_ns).format("%H:%M:%S%.3f");
                let _ = writeln!(output, "{ts} {}", l.payload);
            } else {
                output.push_str(&l.payload);
                output.push('\n');
            }
        }

        if count == 0 {
            output = format!("[no new lines after {start}, {total_lines} total]");
        }

        Ok(output)
    }

    /// Get port status: connection state and buffer statistics.
    #[tool(
        description = "Get serial port status: connection state, total lines, bytes, last activity, DB size."
    )]
    async fn serial_status(
        &self,
        Parameters(params): Parameters<GetStreamStatsParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let state = self
            .port_manager
            .get_state(&params.port_name)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let storage = self
            .port_manager
            .get_storage(&params.port_name)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let stats = tokio::task::spawn_blocking(move || {
            let s = storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.get_stats()
        })
        .await
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let last_activity = stats.last_timestamp_ns.map_or_else(
            || "never".to_string(),
            |ns| {
                chrono::DateTime::from_timestamp_nanos(ns)
                    .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                    .to_string()
            },
        );

        let result = serde_json::json!({
            "port": params.port_name,
            "state": format!("{state:?}"),
            "total_lines": stats.total_lines,
            "total_bytes": stats.total_bytes,
            "last_activity": last_activity,
            "db_size_bytes": stats.db_size_bytes,
        });

        serde_json::to_string_pretty(&result)
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))
    }

    /// Search captured serial data with grep-like queries (exact, substring, regex) with optional time bounds.
    #[tool(
        description = "Search serial data. Supports exact, substring, and regex queries with optional time bounds."
    )]
    async fn serial_search(
        &self,
        Parameters(params): Parameters<SearchBufferParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let max = params
            .max_results
            .unwrap_or(DEFAULT_SEARCH_RESULTS)
            .min(MAX_SEARCH_RESULTS);
        let query_type = params.query_type.as_deref().unwrap_or("substring");

        let time_range =
            parse_time_range(params.start_time.as_deref(), params.end_time.as_deref())?;

        let storage = self
            .port_manager
            .get_storage(&params.port_name)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let query = params.query.clone();
        let qt = query_type.to_string();

        let results = tokio::task::spawn_blocking(move || {
            let s = storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match qt.as_str() {
                "exact" => {
                    // Use substring search then filter for exact match
                    let candidates = s.search_substring(&query, time_range, max * 10)?;
                    Ok(candidates
                        .into_iter()
                        .filter(|l| l.payload == query)
                        .take(max as usize)
                        .collect::<Vec<_>>())
                }
                "regex" => {
                    let re = regex::Regex::new(&query)
                        .map_err(crate::storage::StorageError::InvalidRegex)?;
                    // Narrow by time range if provided, then filter with regex
                    let candidates = if let Some(tr) = time_range {
                        s.search_time_range(tr.start_ns, tr.end_ns, max * 10)?
                    } else {
                        s.read_lines(1, max * 10)?
                    };
                    Ok(candidates
                        .into_iter()
                        .filter(|l| re.is_match(&l.payload))
                        .take(max as usize)
                        .collect::<Vec<_>>())
                }
                _ => {
                    // substring (default)
                    s.search_substring(&query, time_range, max)
                }
            }
        })
        .await
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
        .map_err(|e: crate::storage::StorageError| {
            rmcp::ErrorData::internal_error(e.to_string(), None)
        })?;

        let output: Vec<String> = results
            .iter()
            .map(|l| {
                let ts = chrono::DateTime::from_timestamp_nanos(l.timestamp_ns)
                    .format("%Y-%m-%dT%H:%M:%S%.3fZ");
                format!("{}:[{}] {}", l.id, ts, l.payload)
            })
            .collect();

        Ok(output.join("\n"))
    }

    /// Export captured serial data to a file in txt, csv, or jsonl format.
    #[tool(
        description = "Export serial data to a file. Supports txt (raw lines), csv (RFC 4180), and jsonl formats."
    )]
    async fn serial_export(
        &self,
        Parameters(params): Parameters<ExportBufferParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let format = params.file_format.as_deref().unwrap_or("txt");
        let output_path = std::path::PathBuf::from(&params.output_path);

        // Validate output path
        if output_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(rmcp::ErrorData::internal_error(
                "invalid output path: path traversal not allowed".to_string(),
                None,
            ));
        }
        if let Some(parent) = output_path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                return Err(rmcp::ErrorData::internal_error(
                    format!("parent directory does not exist: {}", parent.display()),
                    None,
                ));
            }
        }

        let start = params.start_line.unwrap_or(1);
        let end = params.end_line.unwrap_or(i64::MAX);

        let storage = self
            .port_manager
            .get_storage(&params.port_name)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let fmt = format.to_string();
        let path = output_path.clone();

        let count = tokio::task::spawn_blocking(move || -> Result<u64, String> {
            use std::io::Write;

            let s = storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let lines = s.export_range(start, end).map_err(|e| e.to_string())?;
            drop(s);
            let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
            let mut writer = std::io::BufWriter::new(file);

            if fmt == "csv" {
                writeln!(writer, "line_number,timestamp,payload").map_err(|e| e.to_string())?;
            }

            let mut count = 0u64;
            for line in &lines {
                match fmt.as_str() {
                    "csv" => {
                        let ts = chrono::DateTime::from_timestamp_nanos(line.timestamp_ns)
                            .format("%Y-%m-%dT%H:%M:%S%.3fZ");
                        let escaped = line.payload.replace('"', "\"\"");
                        writeln!(writer, "{},{},\"{}\"", line.id, ts, escaped)
                            .map_err(|e| e.to_string())?;
                    }
                    "jsonl" => {
                        let obj = serde_json::json!({
                            "line": line.id,
                            "timestamp": chrono::DateTime::from_timestamp_nanos(line.timestamp_ns)
                                .format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
                            "payload": line.payload,
                        });
                        writeln!(
                            writer,
                            "{}",
                            serde_json::to_string(&obj).unwrap_or_default()
                        )
                        .map_err(|e| e.to_string())?;
                    }
                    _ => {
                        // txt
                        writeln!(writer, "{}", line.payload).map_err(|e| e.to_string())?;
                    }
                }
                count += 1;
            }
            writer.flush().map_err(|e| e.to_string())?;
            Ok(count)
        })
        .await
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
        .map_err(|e| rmcp::ErrorData::internal_error(e, None))?;

        let abs_path = std::fs::canonicalize(&output_path).unwrap_or(output_path);

        Ok(format!(
            "Exported {count} lines to {} (format: {format})",
            abs_path.display()
        ))
    }

    /// Clear captured serial data, optionally archiving first.
    #[tool(description = "Clear captured serial data. Optionally archives before clearing.")]
    async fn serial_clear(
        &self,
        Parameters(params): Parameters<ClearBufferParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let archive = params.archive_current.unwrap_or(false);

        let storage = self
            .port_manager
            .get_storage(&params.port_name)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let port_name = params.port_name.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let s = storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let stats = s.get_stats().map_err(|e| e.to_string())?;

            let archive_path = if archive {
                let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
                let sanitized = port_name.replace(['/', '\\'], "_");
                let path = std::path::PathBuf::from(format!("./{sanitized}_{ts}.db"));
                Some(path)
            } else {
                None
            };

            s.clear(archive_path.as_deref())
                .map_err(|e| e.to_string())?;
            drop(s);

            archive_path.as_ref().map_or_else(
                || {
                    Ok(format!(
                        "Cleared buffer ({} lines removed)",
                        stats.total_lines
                    ))
                },
                |path| {
                    Ok(format!(
                        "Cleared buffer ({} lines archived to {})",
                        stats.total_lines,
                        path.display()
                    ))
                },
            )
        })
        .await
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
        .map_err(|e| rmcp::ErrorData::internal_error(e, None))?;

        Ok(result)
    }

    /// List available serial ports and managed connections.
    #[tool(description = "List system serial ports and managed connections with their state.")]
    async fn serial_list(&self) -> Result<String, rmcp::ErrorData> {
        use std::fmt::Write;

        // Get system ports
        let system_ports: Vec<String> = serial2_tokio::SerialPort::available_ports()
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();

        // Get managed ports
        let managed = self.port_manager.list().await;

        let mut output = String::from("System ports:\n");
        if system_ports.is_empty() {
            output.push_str("  (none detected)\n");
        } else {
            for p in &system_ports {
                let _ = writeln!(output, "  {p}");
            }
        }

        output.push_str("\nManaged connections:\n");
        if managed.is_empty() {
            output.push_str("  (none)\n");
        } else {
            for p in &managed {
                let _ = writeln!(
                    output,
                    "  {} — {:?} ({} lines)",
                    p.name, p.state, p.total_lines
                );
            }
        }

        Ok(output)
    }

    /// Open or reconfigure a serial port.
    #[tool(
        description = "Open a serial port with specified parameters. Idempotent — reconfigures if already open."
    )]
    async fn serial_open(
        &self,
        Parameters(params): Parameters<ConfigurePortParams>,
    ) -> Result<String, rmcp::ErrorData> {
        if let Some(baud) = params.baudrate {
            if baud == 0 {
                return Err(rmcp::ErrorData::internal_error(
                    "invalid baud rate: must be > 0".to_string(),
                    None,
                ));
            }
        }

        // Use TOML config as base if available, override with explicit params
        let base = self
            .config
            .ports
            .get(&params.port_name)
            .cloned()
            .unwrap_or_default();
        let config = crate::config::PortConfig {
            baudrate: params.baudrate.unwrap_or(base.baudrate),
            data_bits: params.data_bits.unwrap_or(base.data_bits),
            parity: params.parity.unwrap_or(base.parity),
            stop_bits: params.stop_bits.unwrap_or(base.stop_bits),
            ..base
        };

        let data_dir = self.data_dir();
        self.port_manager
            .open_serial(params.port_name.clone(), config.clone(), data_dir)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        // Persist state for auto-reopen on restart
        if let Ok(db) = self.state_db.lock() {
            db.port_opened(&params.port_name, &config).ok();
        }

        Ok(format!(
            "Opened {} ({}baud, {}{}{})",
            params.port_name,
            config.baudrate,
            config.data_bits,
            config.parity.chars().next().unwrap_or('N'),
            config.stop_bits
        ))
    }

    /// Close a managed serial port.
    #[tool(description = "Close a managed serial port and flush its buffer.")]
    async fn serial_close(
        &self,
        Parameters(params): Parameters<ClosePortParams>,
    ) -> Result<String, rmcp::ErrorData> {
        self.port_manager
            .close(&params.port_name)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        if let Ok(db) = self.state_db.lock() {
            db.port_closed(&params.port_name).ok();
        }

        Ok(format!("Closed {}", params.port_name))
    }

    /// Write data to a serial port.
    #[tool(
        description = "Write data to a serial port. Supports UTF-8 text or hex-encoded bytes (prefix with 0x)."
    )]
    async fn serial_write(
        &self,
        Parameters(params): Parameters<WritePortParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let data = if params.data.starts_with("0x") || params.data.starts_with("0X") {
            hex_decode(&params.data[2..])
                .map_err(|e| rmcp::ErrorData::internal_error(format!("invalid hex: {e}"), None))?
        } else {
            params.data.as_bytes().to_vec()
        };

        let len = data.len();
        self.port_manager
            .write(&params.port_name, data)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        Ok(format!("Wrote {len} bytes to {}", params.port_name))
    }

    /// Set DTR/RTS signals on a serial port.
    #[tool(description = "Set DTR and/or RTS signals on a serial port.")]
    async fn serial_signal(
        &self,
        Parameters(params): Parameters<SetControlLinesParams>,
    ) -> Result<String, rmcp::ErrorData> {
        if params.dtr.is_none() && params.rts.is_none() {
            return Err(rmcp::ErrorData::internal_error(
                "at least one of dtr or rts must be specified".to_string(),
                None,
            ));
        }

        let port = self
            .port_manager
            .get_serial_port(&params.port_name)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let guard = port.lock().await;
        let mut actions = Vec::new();

        if let Some(dtr) = params.dtr {
            guard.set_dtr(dtr).map_err(|e| {
                rmcp::ErrorData::internal_error(format!("failed to set DTR: {e}"), None)
            })?;
            actions.push(format!("DTR={}", if dtr { "HIGH" } else { "LOW" }));
        }
        if let Some(rts) = params.rts {
            guard.set_rts(rts).map_err(|e| {
                rmcp::ErrorData::internal_error(format!("failed to set RTS: {e}"), None)
            })?;
            actions.push(format!("RTS={}", if rts { "HIGH" } else { "LOW" }));
        }
        drop(guard);

        Ok(format!(
            "Set {} on {}",
            actions.join(", "),
            params.port_name
        ))
    }

    /// Send an RS-232 serial BREAK pulse.
    #[tool(
        description = "Send an RS-232 serial BREAK pulse (TX low) for a duration in milliseconds (default: 250ms)."
    )]
    async fn serial_break(
        &self,
        Parameters(params): Parameters<SerialBreakParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let port = self
            .port_manager
            .get_serial_port(&params.port_name)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let duration_ms = params.duration_ms.unwrap_or(250);
        let duration = std::time::Duration::from_millis(duration_ms);

        let guard = port.lock().await;
        guard.set_break(true).map_err(|e| {
            rmcp::ErrorData::internal_error(format!("failed to set break: {e}"), None)
        })?;
        tokio::time::sleep(duration).await;
        guard.set_break(false).map_err(|e| {
            rmcp::ErrorData::internal_error(format!("failed to clear break: {e}"), None)
        })?;
        drop(guard);

        self.log_to_buffer(
            &params.port_name,
            &format!("━━━ SERIAL BREAK ({duration_ms}ms) SENT ━━━"),
        )
        .await;

        Ok(format!(
            "Sent serial BREAK pulse ({duration_ms}ms) on {}",
            params.port_name
        ))
    }

    /// Send a file over serial using XMODEM, YMODEM, or ZMODEM.
    #[tool(
        description = "Send a file over serial port using modem transfer protocols (ZMODEM, YMODEM, XMODEM-1K, XMODEM-CRC, XMODEM)."
    )]
    async fn serial_send_file(
        &self,
        Parameters(params): Parameters<SerialSendFileParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let proto = match params
            .protocol
            .as_deref()
            .unwrap_or("zmodem")
            .to_lowercase()
            .as_str()
        {
            "xmodem" => crate::modem::FileTransferProtocol::Xmodem,
            "xmodem-crc" => crate::modem::FileTransferProtocol::XmodemCrc,
            "xmodem-1k" | "xmodem1k" => crate::modem::FileTransferProtocol::Xmodem1k,
            "ymodem" => crate::modem::FileTransferProtocol::Ymodem,
            _ => crate::modem::FileTransferProtocol::Zmodem,
        };

        let path = std::path::Path::new(&params.file_path);
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file.bin")
            .to_string();

        let data = tokio::fs::read(&params.file_path)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(format!("read file error: {e}"), None))?;

        let port = self
            .port_manager
            .get_serial_port(&params.port_name)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let mut guard = port.lock().await;
        let bytes_sent = match proto {
            crate::modem::FileTransferProtocol::Xmodem => {
                crate::modem::xmodem::send(&mut *guard, &data, false, |_, _| {})
                    .await
                    .map_err(|e| rmcp::ErrorData::internal_error(e, None))?
            }
            crate::modem::FileTransferProtocol::XmodemCrc => {
                crate::modem::xmodem::send(&mut *guard, &data, false, |_, _| {})
                    .await
                    .map_err(|e| rmcp::ErrorData::internal_error(e, None))?
            }
            crate::modem::FileTransferProtocol::Xmodem1k => {
                crate::modem::xmodem::send(&mut *guard, &data, true, |_, _| {})
                    .await
                    .map_err(|e| rmcp::ErrorData::internal_error(e, None))?
            }
            crate::modem::FileTransferProtocol::Ymodem => {
                crate::modem::ymodem::send_file(&mut *guard, &filename, &data, |_, _| {})
                    .await
                    .map_err(|e| rmcp::ErrorData::internal_error(e, None))?
            }
            crate::modem::FileTransferProtocol::Zmodem => {
                crate::modem::zmodem::send_file(&mut *guard, &filename, &data, |_, _| {})
                    .await
                    .map_err(|e| rmcp::ErrorData::internal_error(e, None))?
            }
        };
        drop(guard);

        self.log_to_buffer(
            &params.port_name,
            &format!("━━━ TRANSFER SEND {proto:?} '{filename}' ({bytes_sent} bytes) → OK ━━━"),
        )
        .await;

        Ok(format!(
            "Successfully sent {} bytes ('{}') to {} via {:?}",
            bytes_sent, filename, params.port_name, proto
        ))
    }

    /// Receive a file over serial using XMODEM, YMODEM, or ZMODEM.
    #[tool(
        description = "Receive a file over serial port using modem transfer protocols (ZMODEM, YMODEM, XMODEM-1K, XMODEM-CRC, XMODEM)."
    )]
    async fn serial_receive_file(
        &self,
        Parameters(params): Parameters<SerialReceiveFileParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let proto = match params
            .protocol
            .as_deref()
            .unwrap_or("zmodem")
            .to_lowercase()
            .as_str()
        {
            "xmodem" => crate::modem::FileTransferProtocol::Xmodem,
            "xmodem-crc" => crate::modem::FileTransferProtocol::XmodemCrc,
            "xmodem-1k" | "xmodem1k" => crate::modem::FileTransferProtocol::Xmodem1k,
            "ymodem" => crate::modem::FileTransferProtocol::Ymodem,
            _ => crate::modem::FileTransferProtocol::Zmodem,
        };

        let port = self
            .port_manager
            .get_serial_port(&params.port_name)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let mut guard = port.lock().await;
        let (filename, data) = match proto {
            crate::modem::FileTransferProtocol::Xmodem => {
                let bytes = crate::modem::xmodem::receive(&mut *guard, false, |_| {})
                    .await
                    .map_err(|e| rmcp::ErrorData::internal_error(e, None))?;
                ("xmodem_recv.bin".to_string(), bytes)
            }
            crate::modem::FileTransferProtocol::XmodemCrc => {
                let bytes = crate::modem::xmodem::receive(&mut *guard, true, |_| {})
                    .await
                    .map_err(|e| rmcp::ErrorData::internal_error(e, None))?;
                ("xmodem_recv.bin".to_string(), bytes)
            }
            crate::modem::FileTransferProtocol::Xmodem1k => {
                let bytes = crate::modem::xmodem::receive(&mut *guard, true, |_| {})
                    .await
                    .map_err(|e| rmcp::ErrorData::internal_error(e, None))?;
                ("xmodem1k_recv.bin".to_string(), bytes)
            }
            crate::modem::FileTransferProtocol::Ymodem => {
                crate::modem::ymodem::receive_file(&mut *guard, |_, _| {})
                    .await
                    .map_err(|e| rmcp::ErrorData::internal_error(e, None))?
            }
            crate::modem::FileTransferProtocol::Zmodem => {
                crate::modem::zmodem::receive_file(&mut *guard, |_, _| {})
                    .await
                    .map_err(|e| rmcp::ErrorData::internal_error(e, None))?
            }
        };
        drop(guard);

        let out_dir_str = params.output_dir.as_deref().unwrap_or(".");
        let out_dir = std::path::Path::new(out_dir_str);
        if !out_dir.exists() {
            tokio::fs::create_dir_all(out_dir).await.map_err(|e| {
                rmcp::ErrorData::internal_error(format!("create dir error: {e}"), None)
            })?;
        }

        let out_path = out_dir.join(&filename);
        tokio::fs::write(&out_path, &data)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(format!("write file error: {e}"), None))?;

        self.log_to_buffer(
            &params.port_name,
            &format!(
                "━━━ TRANSFER RECV {proto:?} '{filename}' ({} bytes) → OK ━━━",
                data.len()
            ),
        )
        .await;

        Ok(format!(
            "Successfully received {} bytes ('{}') from {} saved to '{}'",
            data.len(),
            filename,
            params.port_name,
            out_path.display()
        ))
    }

    /// Run a macro sequence on a serial port.
    #[tool(
        description = "Run a macro sequence on a serial port. Built-in: 'reset', 'enter_bootloader', 'break'. User-defined macros from config."
    )]
    async fn serial_macro(
        &self,
        Parameters(params): Parameters<TriggerMacroParams>,
    ) -> Result<String, rmcp::ErrorData> {
        let port = self
            .port_manager
            .get_serial_port(&params.port_name)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let steps: Vec<crate::config::MacroStep> =
            if let Some(user_macro) = self.config.macros.get(&params.macro_name) {
                user_macro.steps.clone()
            } else {
                match params.macro_name.as_str() {
                    "reset" => vec![
                        crate::config::MacroStep::Dtr { value: false },
                        crate::config::MacroStep::Delay { ms: 100 },
                        crate::config::MacroStep::Dtr { value: true },
                    ],
                    "enter_bootloader" => vec![
                        crate::config::MacroStep::Rts { value: true },
                        crate::config::MacroStep::Dtr { value: false },
                        crate::config::MacroStep::Delay { ms: 50 },
                        crate::config::MacroStep::Dtr { value: true },
                        crate::config::MacroStep::Delay { ms: 50 },
                        crate::config::MacroStep::Rts { value: false },
                    ],
                    "break" => vec![crate::config::MacroStep::Write {
                        value: "0x00".into(),
                    }],
                    other => {
                        let available: Vec<&str> = self
                            .config
                            .macros
                            .keys()
                            .map(String::as_str)
                            .chain(["reset", "enter_bootloader", "break"])
                            .collect();
                        return Err(rmcp::ErrorData::internal_error(
                            format!(
                                "unknown macro '{other}'. Available: {}",
                                available.join(", ")
                            ),
                            None,
                        ));
                    }
                }
            };

        let mut executed = Vec::new();
        for step in &steps {
            match step {
                crate::config::MacroStep::Dtr { value } => {
                    port.lock().await.set_dtr(*value).map_err(|e| {
                        rmcp::ErrorData::internal_error(format!("DTR error: {e}"), None)
                    })?;
                    executed.push(format!("DTR={}", if *value { "HIGH" } else { "LOW" }));
                }
                crate::config::MacroStep::Rts { value } => {
                    port.lock().await.set_rts(*value).map_err(|e| {
                        rmcp::ErrorData::internal_error(format!("RTS error: {e}"), None)
                    })?;
                    executed.push(format!("RTS={}", if *value { "HIGH" } else { "LOW" }));
                }
                crate::config::MacroStep::Delay { ms } => {
                    tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
                    executed.push(format!("delay {ms}ms"));
                }
                crate::config::MacroStep::Write { value } => {
                    let data = if value.starts_with("0x") || value.starts_with("0X") {
                        hex_decode(&value[2..]).unwrap_or_default()
                    } else {
                        value.as_bytes().to_vec()
                    };
                    port.lock().await.write_all(&data).await.map_err(|e| {
                        rmcp::ErrorData::internal_error(format!("write error: {e}"), None)
                    })?;
                    executed.push(format!("write {} bytes", data.len()));
                }
            }
        }

        // Log separator in buffer
        let msg = format!(
            "━━━ MACRO '{}' executed ({}) ━━━",
            params.macro_name,
            executed.join(" → ")
        );
        self.log_to_buffer(&params.port_name, &msg).await;

        Ok(format!(
            "Executed '{}' on {} ({} steps: {})",
            params.macro_name,
            params.port_name,
            executed.len(),
            executed.join(" → ")
        ))
    }

    /// Open a GUI monitor window for a serial port.
    #[tool(
        description = "Open a native GUI monitor window showing real-time serial data. User can send data from the window."
    )]
    async fn serial_monitor_open(
        &self,
        Parameters(params): Parameters<MonitorOpenParams>,
    ) -> Result<String, rmcp::ErrorData> {
        self.monitor_open_impl(&params.port_name).await
    }

    /// Close the GUI monitor window for a serial port.
    #[tool(description = "Close the native GUI monitor window for a serial port.")]
    async fn serial_monitor_close(
        &self,
        Parameters(params): Parameters<MonitorCloseParams>,
    ) -> Result<String, rmcp::ErrorData> {
        self.monitor_close_impl(&params.port_name).await
    }

    /// Flash firmware to an ESP device.
    #[cfg(feature = "esp")]
    #[tool(
        description = "Flash firmware to an ESP device via espflash. Requires espflash installed."
    )]
    async fn serial_esp_flash(
        &self,
        Parameters(params): Parameters<EspFlashParams>,
    ) -> Result<String, rmcp::ErrorData> {
        if !crate::esp::is_available() {
            return Err(rmcp::ErrorData::internal_error(
                "espflash not found in PATH. Install with: cargo install espflash".to_string(),
                None,
            ));
        }
        let reopen = self.release_port_for_esp(&params.port_name).await;
        let result =
            crate::esp::flash(&params.port_name, &params.firmware_path, params.baud, false).await;
        if let Some((config, data_dir)) = reopen {
            self.reopen_port_after_esp(&params.port_name, config, data_dir)
                .await;
        }
        let status = if result.is_ok() { "OK" } else { "FAILED" };
        self.log_to_buffer(
            &params.port_name,
            &format!("━━━ FLASH {} → {} ━━━", params.firmware_path, status),
        )
        .await;
        result.map_err(|e| rmcp::ErrorData::internal_error(e, None))
    }

    /// Get ESP chip/board information.
    #[cfg(feature = "esp")]
    #[tool(
        description = "Get ESP chip and board information (chip type, flash size, MAC address)."
    )]
    async fn serial_esp_info(
        &self,
        Parameters(params): Parameters<EspInfoParams>,
    ) -> Result<String, rmcp::ErrorData> {
        if !crate::esp::is_available() {
            return Err(rmcp::ErrorData::internal_error(
                "espflash not found in PATH. Install with: cargo install espflash".to_string(),
                None,
            ));
        }
        let reopen = self.release_port_for_esp(&params.port_name).await;
        let result = crate::esp::board_info(&params.port_name).await;
        if let Some((config, data_dir)) = reopen {
            self.reopen_port_after_esp(&params.port_name, config, data_dir)
                .await;
        }
        result.map_err(|e| rmcp::ErrorData::internal_error(e, None))
    }

    /// Erase entire flash of an ESP device.
    #[cfg(feature = "esp")]
    #[tool(description = "Erase entire flash of an ESP device. WARNING: destroys all data.")]
    async fn serial_esp_erase(
        &self,
        Parameters(params): Parameters<EspEraseParams>,
    ) -> Result<String, rmcp::ErrorData> {
        if !crate::esp::is_available() {
            return Err(rmcp::ErrorData::internal_error(
                "espflash not found in PATH. Install with: cargo install espflash".to_string(),
                None,
            ));
        }
        let reopen = self.release_port_for_esp(&params.port_name).await;
        let result = crate::esp::erase_flash(&params.port_name).await;
        if let Some((config, data_dir)) = reopen {
            self.reopen_port_after_esp(&params.port_name, config, data_dir)
                .await;
        }
        let status = if result.is_ok() { "OK" } else { "FAILED" };
        self.log_to_buffer(
            &params.port_name,
            &format!("━━━ ERASE FLASH → {status} ━━━"),
        )
        .await;
        result.map_err(|e| rmcp::ErrorData::internal_error(e, None))
    }

    /// Write a raw binary to a specific flash address.
    #[cfg(feature = "esp")]
    #[tool(
        description = "Write a raw binary file to a specific flash address on an ESP device. Use for bootloaders, partition tables, or NVS images."
    )]
    async fn serial_esp_write_bin(
        &self,
        Parameters(params): Parameters<EspWriteBinParams>,
    ) -> Result<String, rmcp::ErrorData> {
        if !crate::esp::is_available() {
            return Err(rmcp::ErrorData::internal_error(
                "espflash not found in PATH. Install with: cargo install espflash".to_string(),
                None,
            ));
        }
        let reopen = self.release_port_for_esp(&params.port_name).await;
        let result =
            crate::esp::write_bin(&params.port_name, &params.file_path, &params.address).await;
        if let Some((config, data_dir)) = reopen {
            self.reopen_port_after_esp(&params.port_name, config, data_dir)
                .await;
        }
        let status = if result.is_ok() { "OK" } else { "FAILED" };
        self.log_to_buffer(
            &params.port_name,
            &format!(
                "━━━ WRITE-BIN {} @ {} → {status} ━━━",
                params.file_path, params.address
            ),
        )
        .await;
        result.map_err(|e| rmcp::ErrorData::internal_error(e, None))
    }
}

fn parse_time_range(
    start: Option<&str>,
    end: Option<&str>,
) -> Result<Option<crate::storage::TimeRange>, rmcp::ErrorData> {
    match (start, end) {
        (Some(s), Some(e)) => {
            let start_ns = parse_iso_to_ns(s)?;
            let end_ns = parse_iso_to_ns(e)?;
            Ok(Some(crate::storage::TimeRange { start_ns, end_ns }))
        }
        (Some(s), None) => {
            let start_ns = parse_iso_to_ns(s)?;
            Ok(Some(crate::storage::TimeRange {
                start_ns,
                end_ns: i64::MAX,
            }))
        }
        (None, Some(e)) => {
            let end_ns = parse_iso_to_ns(e)?;
            Ok(Some(crate::storage::TimeRange {
                start_ns: 0,
                end_ns,
            }))
        }
        (None, None) => Ok(None),
    }
}

fn parse_iso_to_ns(s: &str) -> Result<i64, rmcp::ErrorData> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp_nanos_opt().unwrap_or(0))
        .map_err(|e| rmcp::ErrorData::internal_error(format!("invalid time format: {e}"), None))
}

#[cfg(feature = "esp")]
impl DevSerialServer {
    /// Close the port (and monitor) if managed, returning config for reopening.
    async fn release_port_for_esp(
        &self,
        port_name: &str,
    ) -> Option<(crate::config::PortConfig, std::path::PathBuf)> {
        // Check if port is managed
        if self.port_manager.get_state(port_name).await.is_err() {
            return None;
        }

        // Close monitor if open
        #[cfg(feature = "monitor")]
        {
            let mut monitors = self
                .monitors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(mut m) = monitors.remove(port_name) {
                m.handle.close();
                tracing::info!(port = %port_name, "closed monitor for espflash");
            }
        }

        // Close port and get its actual config
        let data_dir = self.data_dir();
        self.port_manager
            .close_with_config(port_name)
            .await
            .ok()
            .map(|config| {
                tracing::info!(port = %port_name, "released port for espflash");
                (config, data_dir)
            })
    }

    /// Reopen the port after espflash finishes.
    async fn reopen_port_after_esp(
        &self,
        port_name: &str,
        config: crate::config::PortConfig,
        data_dir: std::path::PathBuf,
    ) {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if let Err(e) = self
            .port_manager
            .open_serial(port_name.to_string(), config, data_dir)
            .await
        {
            tracing::warn!(port = %port_name, error = %e, "failed to reopen port after espflash");
        } else {
            tracing::info!(port = %port_name, "reopened port after espflash");
        }
    }
}

#[cfg(feature = "monitor")]
impl DevSerialServer {
    #[allow(clippy::too_many_lines)]
    async fn monitor_open_impl(&self, port_name: &str) -> Result<String, rmcp::ErrorData> {
        let state = self
            .port_manager
            .get_state(port_name)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        {
            let monitors = self
                .monitors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if monitors.contains_key(port_name) {
                return Err(rmcp::ErrorData::internal_error(
                    format!("monitor already open for '{port_name}'"),
                    None,
                ));
            }
        }

        let storage = self
            .port_manager
            .get_storage(port_name)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let db_path = {
            let s = storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.db_path().to_path_buf()
        };

        let mut handle = crate::monitor::spawn_monitor(port_name, &db_path, &format!("{state:?}"))
            .map_err(|e| {
                rmcp::ErrorData::internal_error(format!("failed to spawn monitor: {e}"), None)
            })?;

        // Read commands from monitor's stdout pipe
        if let Some(stdout) = handle.take_stdout() {
            let pm = self.port_manager.clone();
            let pn = port_name.to_string();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let reader = BufReader::new(tokio::io::BufReader::new(
                    tokio::process::ChildStdout::from_std(stdout).unwrap(),
                ));
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(macro_name) = line.strip_prefix("__macro:") {
                        if let Ok(port) = pm.get_serial_port(&pn).await {
                            match macro_name.trim() {
                                "reset" => {
                                    port.lock().await.set_dtr(false).ok();
                                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                    port.lock().await.set_dtr(true).ok();
                                }
                                "enter_bootloader" => {
                                    let g = port.lock().await;
                                    g.set_rts(true).ok();
                                    g.set_dtr(false).ok();
                                    drop(g);
                                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                    port.lock().await.set_dtr(true).ok();
                                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                    port.lock().await.set_rts(false).ok();
                                }
                                _ => {}
                            }
                        }
                    } else if let Some(sig) = line.strip_prefix("__signal:") {
                        if let Ok(port) = pm.get_serial_port(&pn).await {
                            let g = port.lock().await;
                            match sig.trim() {
                                "dtr:1" => {
                                    g.set_dtr(true).ok();
                                }
                                "dtr:0" => {
                                    g.set_dtr(false).ok();
                                }
                                "rts:1" => {
                                    g.set_rts(true).ok();
                                }
                                "rts:0" => {
                                    g.set_rts(false).ok();
                                }
                                _ => {}
                            }
                        }
                    } else if let Some(hex) = line.strip_prefix("__data:") {
                        let data: Vec<u8> = (0..hex.len())
                            .step_by(2)
                            .filter_map(|i| {
                                hex.get(i..i + 2)
                                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                            })
                            .collect();
                        pm.write(&pn, data).await.ok();
                    }
                }
            });
        }

        {
            let mut monitors = self
                .monitors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let notify_tx = handle.take_stdin().expect("stdin pipe");

            // Spawn notification task: polls DB for new lines, writes byte to stdin pipe
            let storage_notify = Arc::clone(&storage);
            let mut notify_pipe =
                tokio::process::ChildStdin::from_std(notify_tx).expect("async stdin");
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let mut last_count: u64 = 0;
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    let count = storage_notify
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get_stats()
                        .map_or(0, |s| s.total_lines);
                    if count != last_count {
                        last_count = count;
                        if notify_pipe.write_all(b"\n").await.is_err() {
                            break; // monitor closed
                        }
                    }
                }
            });

            monitors.insert(port_name.to_string(), MonitorState { handle });
        }

        Ok(format!("Monitor opened for {port_name}"))
    }

    async fn monitor_close_impl(&self, port_name: &str) -> Result<String, rmcp::ErrorData> {
        self.port_manager
            .get_state(port_name)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let monitor = {
            let mut monitors = self
                .monitors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            monitors.remove(port_name)
        };

        match monitor {
            Some(mut s) => {
                s.handle.close();
                Ok(format!("Monitor closed for {port_name}"))
            }
            None => Err(rmcp::ErrorData::internal_error(
                format!("no monitor open for '{port_name}'"),
                None,
            )),
        }
    }
}

#[cfg(not(feature = "monitor"))]
impl DevSerialServer {
    async fn monitor_open_impl(&self, _port_name: &str) -> Result<String, rmcp::ErrorData> {
        tokio::task::yield_now().await;
        Err(rmcp::ErrorData::internal_error(
            "monitor feature not enabled (rebuild with --features monitor)".to_string(),
            None,
        ))
    }

    async fn monitor_close_impl(&self, _port_name: &str) -> Result<String, rmcp::ErrorData> {
        tokio::task::yield_now().await;
        Err(rmcp::ErrorData::internal_error(
            "monitor feature not enabled (rebuild with --features monitor)".to_string(),
            None,
        ))
    }
}

#[tool_handler]
#[allow(clippy::manual_async_fn)]
impl ServerHandler for DevSerialServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
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
            let ports = self.port_manager.list().await;
            let resources: Vec<rmcp::model::Resource> = ports
                .iter()
                .map(|p| rmcp::model::Annotated {
                    raw: rmcp::model::RawResource {
                        uri: format!("serial://{}/status", p.name),
                        name: format!("{} status", p.name),
                        title: None,
                        description: Some(format!("Connection state for {}", p.name)),
                        mime_type: Some("application/json".into()),
                        size: None,
                        icons: None,
                        meta: None,
                    },
                    annotations: None,
                })
                .collect();
            Ok(rmcp::model::ListResourcesResult {
                resources,
                next_cursor: None,
                meta: None,
            })
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn read_resource(
        &self,
        request: rmcp::model::ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<
        Output = Result<rmcp::model::ReadResourceResult, rmcp::model::ErrorData>,
    > + Send
    + '_ {
        async move {
            // Parse URI: serial://{port_name}/status
            let uri = &request.uri;
            let port_name = uri
                .strip_prefix("serial://")
                .and_then(|s| s.strip_suffix("/status"))
                .ok_or_else(|| {
                    rmcp::ErrorData::internal_error(format!("invalid resource URI: {uri}"), None)
                })?;

            let conn_state = self
                .port_manager
                .get_state(port_name)
                .await
                .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

            let storage = self
                .port_manager
                .get_storage(port_name)
                .await
                .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

            let buf_stats = {
                let s = storage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                s.get_stats()
                    .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
            };

            let json = serde_json::json!({
                "port": port_name,
                "state": format!("{conn_state:?}"),
                "total_lines": buf_stats.total_lines,
                "total_bytes": buf_stats.total_bytes,
                "db_size_bytes": buf_stats.db_size_bytes,
            });

            Ok(rmcp::model::ReadResourceResult::new(vec![
                rmcp::model::ResourceContents::TextResourceContents {
                    uri: uri.clone(),
                    mime_type: Some("application/json".into()),
                    text: serde_json::to_string_pretty(&json).unwrap_or_default(),
                    meta: None,
                },
            ]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PortConfig;
    use crate::storage::SqliteStorage;
    use crate::testutil::mock_serial::mock_serial;
    use std::sync::Arc;
    use std::time::Duration;

    async fn setup() -> DevSerialServer {
        let mgr = PortManagerHandle::new();
        let (mock, ctrl) = mock_serial(100);
        let storage = Arc::new(std::sync::Mutex::new(SqliteStorage::open_memory().unwrap()));

        mgr.open("test_port".into(), mock, PortConfig::default(), storage)
            .await
            .unwrap();

        ctrl.feed_lines(&["line 1", "line 2", "line 3"]).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        DevSerialServer::with_port_manager(mgr)
    }

    #[tokio::test]
    async fn test_serial_read_basic() {
        let server = setup().await;
        let params = ReadBufferParams {
            port_name: "test_port".into(),
            start_line: None,
            max_lines: Some(10),
            include_timestamps: Some(false),
            after_line: None,
            wait_ms: None,
            since_time: None,
        };

        let result = server.serial_read(Parameters(params)).await.unwrap();
        assert!(result.contains("line 1"));
        assert!(result.contains("line 2"));
        assert!(result.contains("line 3"));
    }

    #[tokio::test]
    async fn test_serial_read_with_timestamps() {
        let server = setup().await;
        let params = ReadBufferParams {
            port_name: "test_port".into(),
            start_line: None,
            max_lines: Some(10),
            include_timestamps: Some(true),
            after_line: None,
            wait_ms: None,
            since_time: None,
        };

        let result = server.serial_read(Parameters(params)).await.unwrap();
        assert!(result.contains(':')); // timestamp HH:MM:SS
        assert!(result.contains("line 1"));
    }

    #[tokio::test]
    async fn test_serial_read_nonexistent_port() {
        let mgr = PortManagerHandle::new();
        let server = DevSerialServer::with_port_manager(mgr);
        let params = ReadBufferParams {
            port_name: "nonexistent".into(),
            start_line: None,
            max_lines: None,
            include_timestamps: None,
            after_line: None,
            wait_ms: None,
            since_time: None,
        };

        let result = server.serial_read(Parameters(params)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_serial_status() {
        let server = setup().await;
        let params = GetStreamStatsParams {
            port_name: "test_port".into(),
        };

        let result = server.serial_status(Parameters(params)).await.unwrap();
        assert!(result.contains("total_lines"));
        assert!(result.contains("Connected"));
    }

    #[tokio::test]
    async fn test_serial_read_empty_result() {
        let server = setup().await;
        let params = ReadBufferParams {
            port_name: "test_port".into(),
            start_line: Some(9999),
            max_lines: Some(10),
            include_timestamps: None,
            after_line: None,
            wait_ms: None,
            since_time: None,
        };

        let result = server.serial_read(Parameters(params)).await.unwrap();
        assert!(result.contains("no new lines"));
    }

    async fn setup_search() -> DevSerialServer {
        let mgr = PortManagerHandle::new();
        let (mock, ctrl) = mock_serial(1000);
        let storage = Arc::new(std::sync::Mutex::new(SqliteStorage::open_memory().unwrap()));

        mgr.open("test_port".into(), mock, PortConfig::default(), storage)
            .await
            .unwrap();

        // Feed mixed content
        let lines: Vec<String> = (0..100)
            .map(|i| {
                if i % 10 == 0 {
                    format!("[ERROR] failure at step {i}")
                } else {
                    format!("[INFO] normal operation {i}")
                }
            })
            .collect();
        for line in &lines {
            ctrl.feed_lines(&[line.as_str()]).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;

        DevSerialServer::with_port_manager(mgr)
    }

    #[tokio::test]
    async fn test_serial_search_substring() {
        let server = setup_search().await;
        let params = SearchBufferParams {
            port_name: "test_port".into(),
            query: "[ERROR]".into(),
            query_type: Some("substring".into()),
            start_time: None,
            end_time: None,
            max_results: None,
        };

        let result = server.serial_search(Parameters(params)).await.unwrap();
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 10);
        assert!(lines.iter().all(|l| l.contains("[ERROR]")));
    }

    #[tokio::test]
    async fn test_serial_search_regex() {
        let server = setup_search().await;
        let params = SearchBufferParams {
            port_name: "test_port".into(),
            query: r"\[ERROR\].*step \d0".into(),
            query_type: Some("regex".into()),
            start_time: None,
            end_time: None,
            max_results: None,
        };

        let result = server.serial_search(Parameters(params)).await.unwrap();
        // Matches step 0, 10, 20, ..., 90
        assert!(!result.is_empty());
        assert!(result.contains("[ERROR]"));
    }

    #[tokio::test]
    async fn test_serial_search_invalid_regex() {
        let server = setup_search().await;
        let params = SearchBufferParams {
            port_name: "test_port".into(),
            query: "[unclosed".into(),
            query_type: Some("regex".into()),
            start_time: None,
            end_time: None,
            max_results: None,
        };

        let result = server.serial_search(Parameters(params)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_serial_search_no_matches() {
        let server = setup_search().await;
        let params = SearchBufferParams {
            port_name: "test_port".into(),
            query: "NONEXISTENT_PATTERN".into(),
            query_type: None,
            start_time: None,
            end_time: None,
            max_results: None,
        };

        let result = server.serial_search(Parameters(params)).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_serial_search_max_results() {
        let server = setup_search().await;
        let params = SearchBufferParams {
            port_name: "test_port".into(),
            query: "[INFO]".into(),
            query_type: None,
            start_time: None,
            end_time: None,
            max_results: Some(5),
        };

        let result = server.serial_search(Parameters(params)).await.unwrap();
        assert_eq!(result.lines().count(), 5);
    }

    #[tokio::test]
    async fn test_serial_export_txt() {
        let server = setup().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.txt");

        let params = ExportBufferParams {
            port_name: "test_port".into(),
            file_format: Some("txt".into()),
            start_line: None,
            end_line: None,
            output_path: path.to_str().unwrap().into(),
        };

        let result = server.serial_export(Parameters(params)).await.unwrap();
        assert!(result.contains("3 lines"));

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("line 1"));
        assert!(content.contains("line 3"));
    }

    #[tokio::test]
    async fn test_serial_export_csv() {
        let server = setup().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.csv");

        let params = ExportBufferParams {
            port_name: "test_port".into(),
            file_format: Some("csv".into()),
            start_line: None,
            end_line: None,
            output_path: path.to_str().unwrap().into(),
        };

        let result = server.serial_export(Parameters(params)).await.unwrap();
        assert!(result.contains("3 lines"));

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("line_number,timestamp,payload"));
        assert!(content.contains("\"line 1\""));
    }

    #[tokio::test]
    async fn test_serial_export_jsonl() {
        let server = setup().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.jsonl");

        let params = ExportBufferParams {
            port_name: "test_port".into(),
            file_format: Some("jsonl".into()),
            start_line: None,
            end_line: None,
            output_path: path.to_str().unwrap().into(),
        };

        let result = server.serial_export(Parameters(params)).await.unwrap();
        assert!(result.contains("3 lines"));

        let content = std::fs::read_to_string(&path).unwrap();
        for line in content.lines() {
            let _: serde_json::Value = serde_json::from_str(line).unwrap();
        }
    }

    #[tokio::test]
    async fn test_serial_export_path_traversal_rejected() {
        let server = setup().await;
        let params = ExportBufferParams {
            port_name: "test_port".into(),
            file_format: None,
            start_line: None,
            end_line: None,
            output_path: "../../etc/passwd".into(),
        };

        let result = server.serial_export(Parameters(params)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_serial_export_nonexistent_parent() {
        let server = setup().await;
        let params = ExportBufferParams {
            port_name: "test_port".into(),
            file_format: None,
            start_line: None,
            end_line: None,
            output_path: "/nonexistent_dir_xyz/file.txt".into(),
        };

        let result = server.serial_export(Parameters(params)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_serial_clear_no_archive() {
        let server = setup().await;
        let params = ClearBufferParams {
            port_name: "test_port".into(),
            archive_current: Some(false),
        };

        let result = server.serial_clear(Parameters(params)).await.unwrap();
        assert!(result.contains("3 lines removed"));

        // Verify buffer is empty
        let stats_params = GetStreamStatsParams {
            port_name: "test_port".into(),
        };
        let stats = server
            .serial_status(Parameters(stats_params))
            .await
            .unwrap();
        assert!(stats.contains("\"total_lines\": 0"));
    }

    #[tokio::test]
    async fn test_serial_clear_nonexistent_port() {
        let mgr = PortManagerHandle::new();
        let server = DevSerialServer::with_port_manager(mgr);
        let params = ClearBufferParams {
            port_name: "nope".into(),
            archive_current: None,
        };

        let result = server.serial_clear(Parameters(params)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_serial_list() {
        let server = setup().await;
        let result = server.serial_list().await.unwrap();
        assert!(result.contains("Managed connections:"));
        assert!(result.contains("test_port"));
    }

    #[tokio::test]
    async fn test_serial_open_invalid_baud() {
        let mgr = PortManagerHandle::new();
        let server = DevSerialServer::with_port_manager(mgr);
        let params = ConfigurePortParams {
            port_name: "/dev/ttyUSB0".into(),
            baudrate: Some(0),
            data_bits: None,
            parity: None,
            stop_bits: None,
        };

        let result = server.serial_open(Parameters(params)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_serial_signal_nonexistent() {
        let mgr = PortManagerHandle::new();
        let server = DevSerialServer::with_port_manager(mgr);
        let params = SetControlLinesParams {
            port_name: "nope".into(),
            dtr: Some(true),
            rts: None,
        };

        let result = server.serial_signal(Parameters(params)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_serial_break_nonexistent() {
        let mgr = PortManagerHandle::new();
        let server = DevSerialServer::with_port_manager(mgr);
        let params = SerialBreakParams {
            port_name: "nonexistent".into(),
            duration_ms: Some(100),
        };

        let result = server.serial_break(Parameters(params)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_serial_send_file_nonexistent() {
        let mgr = PortManagerHandle::new();
        let server = DevSerialServer::with_port_manager(mgr);
        let params = SerialSendFileParams {
            port_name: "nonexistent".into(),
            file_path: "/nonexistent/test.bin".into(),
            protocol: Some("zmodem".into()),
        };

        let result = server.serial_send_file(Parameters(params)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_serial_receive_file_nonexistent() {
        let mgr = PortManagerHandle::new();
        let server = DevSerialServer::with_port_manager(mgr);
        let params = SerialReceiveFileParams {
            port_name: "nonexistent".into(),
            output_dir: Some("/tmp".into()),
            protocol: Some("zmodem".into()),
        };

        let result = server.serial_receive_file(Parameters(params)).await;
        assert!(result.is_err());
    }
}
