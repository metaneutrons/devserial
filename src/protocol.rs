// SPDX-License-Identifier: GPL-3.0-or-later
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

/// How an ESP operation enters the ROM loader before synchronization.
///
/// `Auto` lets espflash select USB Serial/JTAG for Espressif PID `0x1001`
/// and the UART-bridge sequence otherwise. `UsbReset` forces the official
/// USB Serial/JTAG control-line sequence. The no-reset modes consume a loader
/// that is already running, for example after BOOT/GPIO35 plus RESET.
#[cfg(feature = "esp")]
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    clap::ValueEnum,
    schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum EspResetBefore {
    /// Detect the reset sequence from the USB identity.
    #[default]
    Auto,
    /// Force the native USB Serial/JTAG sequence used by Espressif tools.
    UsbReset,
    /// Do not change DTR/RTS; synchronize with an already running ROM loader.
    NoReset,
    /// Neither change DTR/RTS nor send the initial synchronization command.
    NoResetNoSync,
}

#[cfg(feature = "esp")]
impl EspResetBefore {
    /// The next choice shown by compact terminal and desktop selectors.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Auto => Self::UsbReset,
            Self::UsbReset => Self::NoReset,
            Self::NoReset => Self::NoResetNoSync,
            Self::NoResetNoSync => Self::Auto,
        }
    }

    /// The preceding choice shown by compact terminal selectors.
    #[must_use]
    pub const fn previous(self) -> Self {
        match self {
            Self::Auto => Self::NoResetNoSync,
            Self::UsbReset => Self::Auto,
            Self::NoReset => Self::UsbReset,
            Self::NoResetNoSync => Self::NoReset,
        }
    }
}

#[cfg(feature = "esp")]
impl fmt::Display for EspResetBefore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::UsbReset => "usb-reset",
            Self::NoReset => "no-reset",
            Self::NoResetNoSync => "no-reset-no-sync",
        })
    }
}

/// Where the pieces of a flashed ESP image go, when `espflash`'s defaults do
/// not fit the board.
///
/// Every field is optional and left out means the default of the `espflash`
/// command: its own bootloader and partition table, the table at the chip's
/// usual offset, and the first application partition as the target. A board
/// with its own layout (a 32 MiB ESP32-P4 with two OTA slots, say) names the
/// partition table here, so the application lands in the partition the board
/// boots from.
#[cfg(feature = "esp")]
#[derive(
    Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, clap::Args,
)]
pub struct EspFlashLayout {
    /// Partition table, as a CSV file or a binary one, on the machine the
    /// daemon runs on.
    #[serde(default)]
    #[arg(long, value_name = "FILE")]
    pub partition_table: Option<String>,
    /// Second stage bootloader image, on the machine the daemon runs on.
    #[serde(default)]
    #[arg(long, value_name = "FILE")]
    pub bootloader: Option<String>,
    /// Flash address of the partition table, such as `0x8000`.
    #[serde(default)]
    #[arg(long, value_name = "ADDRESS")]
    pub partition_table_offset: Option<String>,
    /// Name of the application partition that receives the image, such as
    /// `ota_0`.
    #[serde(default)]
    #[arg(long, value_name = "NAME")]
    pub target_app_partition: Option<String>,
}

#[cfg(feature = "esp")]
impl EspFlashLayout {
    /// The layout a window or a terminal form holds as four lines of text.
    ///
    /// A line that is empty or only spaces is a field left at its default.
    #[must_use]
    pub fn from_text(
        partition_table: &str,
        bootloader: &str,
        partition_table_offset: &str,
        target_app_partition: &str,
    ) -> Self {
        let field = |text: &str| {
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_owned())
        };
        Self {
            partition_table: field(partition_table),
            bootloader: field(bootloader),
            partition_table_offset: field(partition_table_offset),
            target_app_partition: field(target_app_partition),
        }
    }

    /// Whether every field is at its default.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        self.partition_table.is_none()
            && self.bootloader.is_none()
            && self.partition_table_offset.is_none()
            && self.target_app_partition.is_none()
    }
}

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

/// What the daemon reports about its HTTP interface.
#[cfg(feature = "rest")]
///
/// One shape for all three surfaces, so the window, the terminal and the
/// command line describe the same thing in the same words. `reason` is kept
/// after a failed bind, because the surfaces show it until the next attempt and
/// a state without it would look like a listener that simply is not there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestState {
    /// Whether a listener is accepting connections.
    pub listening: bool,
    /// Address the listener holds, or the one that was asked for.
    pub bind: String,
    /// Port the listener holds, or the one that was asked for.
    pub port: u16,
    /// Why the last attempt failed, if it did.
    pub reason: Option<String>,
    /// Whether a bearer token is required.
    pub token_required: bool,
}

#[cfg(feature = "rest")]
impl RestState {
    /// The address a caller reaches, for a surface that shows it.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}:{}/v1", self.bind, self.port)
    }
}

/// Information about a managed serial port.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortInfoResponse {
    pub name: String,
    pub state: ConnectionState,
    pub total_lines: u64,
}

/// Serial line settings, all optional so a caller can change one of them.
#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema,
)]
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

/// How long a lease lasts without renewal (#114).
///
/// Three renewals' worth: a window frozen for a moment keeps its hold, and a
/// client that crashed releases the daemon within three minutes.
pub const LEASE_TTL_SECS: u64 = 180;

/// How often a long-lived client renews its lease.
pub const LEASE_RENEW_SECS: u64 = 60;

/// The kind of long-lived client holding a lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeaseKind {
    /// A window of the desktop monitor.
    Gui,
    /// The terminal monitor.
    Tui,
    /// An MCP server session.
    Mcp,
    /// `devserial read --follow`.
    Follow,
}

impl fmt::Display for LeaseKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Gui => "GUI",
            Self::Tui => "TUI",
            Self::Mcp => "MCP",
            Self::Follow => "read --follow",
        })
    }
}

/// Where the idle period in force came from, highest precedence last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IdleSource {
    /// The built-in default.
    Default,
    /// `[daemon] idle_timeout` in the configuration file.
    Config,
    /// `devserial daemon --idle-timeout` when the daemon was started.
    StartFlag,
    /// Changed on the running daemon.
    Runtime,
}

impl fmt::Display for IdleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Default => "built-in default",
            Self::Config => "configuration",
            Self::StartFlag => "start flag",
            Self::Runtime => "changed at runtime",
        })
    }
}

/// When the daemon would stop by itself, and what holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleState {
    /// The idle period in seconds; `0` means never.
    pub idle_seconds: u64,
    /// Where that period came from.
    pub source: IdleSource,
    /// Seconds until an idle stop, when one is due at all: `None` while a
    /// lease holds the daemon or the period is `0`.
    pub stops_in_seconds: Option<u64>,
    /// The kinds of long-lived client attached, one entry per lease.
    pub leases: Vec<LeaseKind>,
}

/// Request payloads supported by the devserial engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", content = "params")]
pub enum RequestPayload {
    /// Liveness check.
    Ping,
    /// Which build the daemon is, so a client can tell a different one
    /// (#115). Separate from `Ping`, which stays as it is: a client has to be
    /// able to reach an older daemon to warn about it.
    Identify,
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
    /// Delete the lines before or after one line, which stays. Exactly one of
    /// `above` and `below` is given. A copy of the buffer is made first unless
    /// `archive_current` is `false`, and `dry_run` only counts.
    DeleteLines {
        port: String,
        /// Delete the lines with a smaller id than this.
        above: Option<i64>,
        /// Delete the lines with a larger id than this.
        below: Option<i64>,
        /// With `below`: stop at this id, so lines that arrived after the
        /// caller looked are not taken.
        #[serde(default)]
        through: Option<i64>,
        archive_current: Option<bool>,
        dry_run: Option<bool>,
    },
    /// Get buffer statistics for a port.
    GetStats { port: String },
    /// Flash firmware via espflash.
    #[cfg(feature = "esp")]
    EspFlash {
        port: String,
        firmware_path: String,
        baud: Option<u32>,
        #[serde(default)]
        reset: EspResetBefore,
        #[serde(default)]
        layout: EspFlashLayout,
    },
    /// Query ESP board info via espflash.
    #[cfg(feature = "esp")]
    EspInfo {
        port: String,
        #[serde(default)]
        reset: EspResetBefore,
    },
    /// Erase ESP flash via espflash.
    #[cfg(feature = "esp")]
    EspErase {
        port: String,
        #[serde(default)]
        reset: EspResetBefore,
    },
    /// Write a raw binary to a flash address via espflash.
    #[cfg(feature = "esp")]
    EspWriteBin {
        port: String,
        file_path: String,
        address: String,
        #[serde(default)]
        reset: EspResetBefore,
    },
    /// State of the HTTP interface.
    #[cfg(feature = "rest")]
    RestStatus,
    /// Start the HTTP interface, binding before the answer is sent.
    #[cfg(feature = "rest")]
    RestEnable {
        bind: Option<String>,
        port: Option<u16>,
        /// Bearer token for this run, required for a non-loopback bind.
        token: Option<String>,
    },
    /// Stop the HTTP interface.
    #[cfg(feature = "rest")]
    RestDisable,
    /// Keep the daemon from stopping while this long-lived client is attached.
    ///
    /// Sent once a minute. A lease that is not renewed lapses after
    /// [`LEASE_TTL_SECS`]; a renewal creates the lease if the daemon does not
    /// know it, so a client re-registers with a daemon that restarted.
    RenewLease { client: String, kind: LeaseKind },
    /// End a lease before it lapses, when the client quits normally.
    ReleaseLease { client: String },
    /// Change the running daemon's idle period. `0` means never stop by
    /// itself. Lasts until this daemon stops.
    SetIdleTimeout { seconds: u64 },
    /// The daemon's idle period, its source, and what holds it.
    GetLifecycle,
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
            | Self::DeleteLines { port, .. }
            | Self::GetStats { port } => Some(port),
            #[cfg(feature = "esp")]
            Self::EspFlash { port, .. }
            | Self::EspInfo { port, .. }
            | Self::EspErase { port, .. }
            | Self::EspWriteBin { port, .. } => Some(port),
            #[cfg(feature = "rest")]
            Self::RestStatus | Self::RestEnable { .. } | Self::RestDisable => None,
            Self::Ping
            | Self::ListPorts
            | Self::ListHardware
            | Self::RenewLease { .. }
            | Self::ReleaseLease { .. }
            | Self::SetIdleTimeout { .. }
            | Self::GetLifecycle
            | Self::Identify
            | Self::Shutdown => None,
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
    /// Lines deleted around one line, or counted when it was a dry run.
    LinesDeleted {
        /// How many lines were deleted, or would be.
        lines: u64,
        /// Where the copy of the buffer went, when one was made.
        archive_path: Option<String>,
        /// Whether this only counted.
        dry_run: bool,
    },
    /// Buffer statistics.
    Stats(BufferStats),
    /// ESP tool command output.
    #[cfg(feature = "esp")]
    EspSuccess(String),
    /// State of the HTTP interface.
    #[cfg(feature = "rest")]
    RestState(RestState),
    /// The daemon's lifecycle state.
    Lifecycle(LifecycleState),
    /// The daemon's build (#115).
    Identity(crate::identity::BuildIdentity),
    /// Daemon shutdown initiated.
    ShutdownAck,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "esp")]
    #[test]
    fn a_flash_request_without_a_layout_still_reads() {
        // A client older than the layout sends none, and gets the defaults.
        let old = r#"{"action":"EspFlash","params":{"port":"p","firmware_path":"f","baud":null}}"#;
        let RequestPayload::EspFlash { layout, reset, .. } = serde_json::from_str(old).unwrap()
        else {
            panic!("not a flash request");
        };
        assert!(layout.is_default());
        assert_eq!(reset, EspResetBefore::Auto);

        let new = r#"{"action":"EspFlash","params":{"port":"p","firmware_path":"f","baud":null,
            "layout":{"partition_table":"t.csv","target_app_partition":"ota_0"}}}"#;
        let RequestPayload::EspFlash { layout, .. } = serde_json::from_str(new).unwrap() else {
            panic!("not a flash request");
        };
        assert_eq!(layout.partition_table.as_deref(), Some("t.csv"));
        assert_eq!(layout.target_app_partition.as_deref(), Some("ota_0"));
        assert!(!layout.is_default());
    }

    #[cfg(feature = "esp")]
    #[test]
    fn a_layout_typed_in_a_form_leaves_empty_lines_at_the_default() {
        let layout = EspFlashLayout::from_text(" /tmp/p.csv ", "", "   ", "ota_1");
        assert_eq!(layout.partition_table.as_deref(), Some("/tmp/p.csv"));
        assert_eq!(layout.bootloader, None);
        assert_eq!(layout.partition_table_offset, None);
        assert_eq!(layout.target_app_partition.as_deref(), Some("ota_1"));
        assert!(EspFlashLayout::from_text("", " ", "", "\t").is_default());
    }

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

    #[cfg(feature = "esp")]
    #[test]
    fn esp_reset_strategy_is_stable_and_old_requests_default_to_auto() {
        assert_eq!(
            serde_json::to_string(&EspResetBefore::UsbReset).unwrap(),
            "\"usb-reset\""
        );
        let request: RequestPayload = serde_json::from_value(serde_json::json!({
            "action": "EspInfo",
            "params": { "port": "/dev/cu.usbmodem1101" }
        }))
        .unwrap();
        assert_eq!(
            request,
            RequestPayload::EspInfo {
                port: "/dev/cu.usbmodem1101".into(),
                reset: EspResetBefore::Auto,
            }
        );
        assert_eq!(EspResetBefore::Auto.next(), EspResetBefore::UsbReset);
        assert_eq!(
            EspResetBefore::Auto.previous(),
            EspResetBefore::NoResetNoSync
        );
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
