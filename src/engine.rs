// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! The one place where devserial operations are carried out.
//!
//! Transports (CLI, IPC daemon, MCP server) translate their input into a
//! [`RequestPayload`], hand it to [`CommandEngine::execute`] and render the
//! [`ResponsePayload`] they get back. No transport contains operation logic of
//! its own, which is what keeps their behaviour identical.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::{Config, MacroStep, PortConfig};
use crate::export::{self, ExportFormat};
use crate::modem::FileTransferProtocol;
use crate::paths;
use crate::port_manager::{PortManagerError, PortManagerHandle};
use crate::protocol::{
    DEFAULT_READ_LIMIT, DEFAULT_SEARCH_LIMIT, LinesPage, MAX_READ_LIMIT, MAX_SEARCH_LIMIT,
    MAX_SEARCH_SCAN, PortInfoResponse, PortSettings, ReadWindow, RequestPayload, ResponsePayload,
    SearchMode, SearchOutcome,
};
use crate::serial_params::{self, DEFAULT_BREAK_MS};
use crate::state::StateDb;
use crate::storage::{SqliteStorage, StorageError, StoredLine, TimeRange};

/// How long the daemon waits before reopening a port after an ESP operation.
#[cfg(feature = "esp")]
const ESP_REOPEN_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// Everything that can go wrong while executing an operation.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Port(#[from] PortManagerError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Export(#[from] export::ExportError),
    #[error(transparent)]
    Hex(#[from] crate::hex::HexError),
    #[error(transparent)]
    Param(#[from] serial_params::ParamError),
    #[error("invalid regex: {0}")]
    Regex(#[from] regex::Error),
    #[error("io error on '{path}': {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid argument: {0}")]
    Invalid(String),
    #[error("unknown macro '{name}'. Available: {available}")]
    UnknownMacro { name: String, available: String },
    #[error("file transfer failed: {0}")]
    Transfer(String),
    #[error("{0}")]
    Tool(String),
    #[error("internal task failed: {0}")]
    Task(String),
}

impl EngineError {
    fn io(path: impl AsRef<Path>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.as_ref().display().to_string(),
            source,
        }
    }

    fn invalid(msg: impl Into<String>) -> Self {
        Self::Invalid(msg.into())
    }
}

impl From<tokio::task::JoinError> for EngineError {
    fn from(value: tokio::task::JoinError) -> Self {
        Self::Task(value.to_string())
    }
}

/// Shared storage handle for one port.
type SharedStorage = Arc<Mutex<SqliteStorage>>;

/// Core command dispatcher for devserial operations.
#[derive(Clone)]
pub struct CommandEngine {
    port_manager: PortManagerHandle,
    state_db: Arc<Mutex<StateDb>>,
    config: Arc<Config>,
}

impl CommandEngine {
    /// Create a new command engine.
    ///
    /// The data and archive directories come from the configuration, so there
    /// is no second place where they could disagree.
    #[must_use]
    pub const fn new(
        port_manager: PortManagerHandle,
        state_db: Arc<Mutex<StateDb>>,
        config: Arc<Config>,
    ) -> Self {
        Self {
            port_manager,
            state_db,
            config,
        }
    }

    /// Access the underlying port manager handle.
    #[must_use]
    pub const fn port_manager(&self) -> &PortManagerHandle {
        &self.port_manager
    }

    /// Access the underlying configuration.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Directory holding capture databases.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.config.global.data_dir
    }

    /// Directory holding archived databases.
    #[must_use]
    pub fn archive_dir(&self) -> &Path {
        &self.config.global.archive_dir
    }

    /// Execute a request payload and return a response payload.
    ///
    /// # Errors
    /// Returns the operation's error.
    pub async fn execute(&self, payload: RequestPayload) -> Result<ResponsePayload, EngineError> {
        match payload {
            RequestPayload::Ping => Ok(ResponsePayload::Pong),
            RequestPayload::ListPorts => self.list_ports().await,
            RequestPayload::ListHardware => Ok(ResponsePayload::HardwarePorts(hardware_ports())),
            RequestPayload::OpenPort { name, settings } => self.open_port(&name, settings).await,
            RequestPayload::ReconfigurePort { name, settings } => {
                self.reconfigure_port(&name, settings).await
            }
            RequestPayload::ClosePort { name } => self.close_port(&name).await,
            RequestPayload::ReadLines { port, window } => self.read_lines(&port, window).await,
            RequestPayload::GetStatus { port } => self.status(&port).await,
            RequestPayload::WriteData { port, data, is_hex } => {
                self.write_data(&port, &data, is_hex).await
            }
            RequestPayload::SendBreak { port, duration_ms } => {
                self.send_break(&port, duration_ms).await
            }
            RequestPayload::SendFile {
                port,
                file_path,
                protocol,
            } => self.send_file(&port, &file_path, protocol).await,
            RequestPayload::ReceiveFile {
                port,
                output_dir,
                protocol,
            } => self.receive_file(&port, &output_dir, protocol).await,
            RequestPayload::SetSignal { port, dtr, rts } => self.set_signal(&port, dtr, rts).await,
            RequestPayload::ExecuteMacro { port, macro_name } => {
                self.execute_macro(&port, &macro_name).await
            }
            RequestPayload::Search {
                port,
                query,
                mode,
                start_ns,
                end_ns,
                limit,
            } => {
                self.search(&port, &query, mode, start_ns, end_ns, limit)
                    .await
            }
            RequestPayload::Export {
                port,
                output_path,
                file_format,
                start_line,
                end_line,
            } => {
                self.export(&port, &output_path, file_format, start_line, end_line)
                    .await
            }
            RequestPayload::Clear {
                port,
                archive_current,
            } => self.clear(&port, archive_current.unwrap_or(false)).await,
            RequestPayload::GetStats { port } => self.stats(&port).await,
            #[cfg(feature = "esp")]
            RequestPayload::EspFlash {
                port,
                firmware_path,
                baud,
            } => {
                let log = format!("FLASH {firmware_path}");
                self.esp_operation(&port, Some(log), |port| async move {
                    crate::esp::flash(&port, &firmware_path, baud).await
                })
                .await
            }
            #[cfg(feature = "esp")]
            RequestPayload::EspInfo { port } => {
                self.esp_operation(&port, None, |port| async move {
                    crate::esp::board_info(&port).await
                })
                .await
            }
            #[cfg(feature = "esp")]
            RequestPayload::EspErase { port } => {
                self.esp_operation(&port, Some("ERASE FLASH".to_string()), |port| async move {
                    crate::esp::erase_flash(&port).await
                })
                .await
            }
            #[cfg(feature = "esp")]
            RequestPayload::EspWriteBin {
                port,
                file_path,
                address,
            } => {
                let log = format!("WRITE-BIN {file_path} @ {address}");
                self.esp_operation(&port, Some(log), |port| async move {
                    crate::esp::write_bin(&port, &file_path, &address).await
                })
                .await
            }
            RequestPayload::Shutdown => Ok(ResponsePayload::ShutdownAck),
        }
    }

    // ------------------------------------------------------------ ports

    async fn list_ports(&self) -> Result<ResponsePayload, EngineError> {
        let ports = self
            .port_manager
            .list()
            .await
            .into_iter()
            .map(|p| PortInfoResponse {
                name: p.name,
                state: p.state,
                total_lines: p.total_lines,
            })
            .collect();
        Ok(ResponsePayload::PortList(ports))
    }

    /// Merge requested settings onto the configured base for a port.
    fn merge_settings(
        &self,
        name: &str,
        settings: PortSettings,
    ) -> Result<PortConfig, EngineError> {
        let base = self.config.port(name);
        if let Some(baud) = settings.baudrate {
            serial_params::validate_baud(baud)?;
        }
        Ok(PortConfig {
            baudrate: settings.baudrate.unwrap_or(base.baudrate),
            data_bits: settings.data_bits.unwrap_or(base.data_bits),
            parity: settings.parity.unwrap_or(base.parity),
            stop_bits: settings.stop_bits.unwrap_or(base.stop_bits),
            flow_control: settings.flow_control.unwrap_or(base.flow_control),
            ..base
        })
    }

    /// Open a port, or reconfigure it when it is already open.
    async fn open_port(
        &self,
        name: &str,
        settings: PortSettings,
    ) -> Result<ResponsePayload, EngineError> {
        if self.port_manager.get_state(name).await.is_ok() {
            return self.reconfigure_port(name, settings).await;
        }

        let config = self.merge_settings(name, settings)?;
        self.port_manager
            .open_serial(
                name.to_string(),
                config.clone(),
                self.data_dir().to_path_buf(),
            )
            .await?;
        self.remember_open_port(name, &config);

        Ok(ResponsePayload::PortOpened {
            name: name.to_string(),
            config_summary: config.framing_summary(),
        })
    }

    async fn reconfigure_port(
        &self,
        name: &str,
        settings: PortSettings,
    ) -> Result<ResponsePayload, EngineError> {
        let current = self.port_manager.get_config(name).await?;
        if let Some(baud) = settings.baudrate {
            serial_params::validate_baud(baud)?;
        }
        let config = PortConfig {
            baudrate: settings.baudrate.unwrap_or(current.baudrate),
            data_bits: settings.data_bits.unwrap_or(current.data_bits),
            parity: settings.parity.unwrap_or(current.parity),
            stop_bits: settings.stop_bits.unwrap_or(current.stop_bits),
            flow_control: settings.flow_control.unwrap_or(current.flow_control),
            ..current
        };

        self.port_manager.reconfigure(name, config.clone()).await?;
        self.remember_open_port(name, &config);
        let summary = config.framing_summary();
        self.log_to_buffer(name, &format!("━━━ RECONFIGURED: {summary} ━━━"))
            .await;

        Ok(ResponsePayload::PortReconfigured {
            name: name.to_string(),
            config_summary: summary,
        })
    }

    async fn close_port(&self, name: &str) -> Result<ResponsePayload, EngineError> {
        self.port_manager.close(name).await?;
        if let Ok(db) = self.state_db.lock()
            && let Err(e) = db.port_closed(name)
        {
            tracing::warn!(port = %name, error = %e, "could not update persisted state");
        }
        Ok(ResponsePayload::PortClosed {
            name: name.to_string(),
        })
    }

    fn remember_open_port(&self, name: &str, config: &PortConfig) {
        if let Ok(db) = self.state_db.lock()
            && let Err(e) = db.port_opened(name, config)
        {
            tracing::warn!(port = %name, error = %e, "could not persist port state");
        }
    }

    // ------------------------------------------------------------ reading

    async fn storage(&self, port: &str) -> Result<SharedStorage, EngineError> {
        Ok(self.port_manager.get_storage(port).await?)
    }

    /// Run a blocking storage operation off the async runtime.
    async fn with_storage<T, F>(&self, port: &str, op: F) -> Result<T, EngineError>
    where
        T: Send + 'static,
        F: FnOnce(&SqliteStorage) -> Result<T, StorageError> + Send + 'static,
    {
        let storage = self.storage(port).await?;
        let result = tokio::task::spawn_blocking(move || {
            let guard = storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            op(&guard)
        })
        .await??;
        Ok(result)
    }

    async fn read_lines(
        &self,
        port: &str,
        window: ReadWindow,
    ) -> Result<ResponsePayload, EngineError> {
        let limit = window
            .limit
            .unwrap_or(DEFAULT_READ_LIMIT)
            .clamp(1, MAX_READ_LIMIT);
        let deadline = window
            .wait_ms
            .filter(|ms| *ms > 0)
            .map(|ms| tokio::time::Instant::now() + std::time::Duration::from_millis(ms));

        loop {
            let page = self.read_once(port, window, limit).await?;
            if !page.lines.is_empty() {
                return Ok(ResponsePayload::Lines(page));
            }
            match deadline {
                Some(deadline) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                _ => return Ok(ResponsePayload::Lines(page)),
            }
        }
    }

    async fn read_once(
        &self,
        port: &str,
        window: ReadWindow,
        limit: u32,
    ) -> Result<LinesPage, EngineError> {
        self.with_storage(port, move |storage| {
            let total_lines = storage.line_count()?;
            let lines = if let Some(since) = window.since_ns {
                storage.search_time_range(since, i64::MAX, limit)?
            } else {
                let start = resolve_start(&window, total_lines);
                storage.read_lines(start, limit)?
            };
            let next_after_id = lines.last().map_or_else(
                || {
                    window
                        .after_id
                        .unwrap_or_else(|| i64::try_from(total_lines).unwrap_or(i64::MAX))
                },
                |l| l.id,
            );
            Ok(LinesPage {
                lines,
                total_lines,
                next_after_id,
            })
        })
        .await
    }

    async fn status(&self, port: &str) -> Result<ResponsePayload, EngineError> {
        let connection = self.port_manager.get_state(port).await?;
        let buffer = self.with_storage(port, SqliteStorage::get_stats).await?;
        Ok(ResponsePayload::Status {
            name: port.to_string(),
            state: connection,
            stats: buffer,
        })
    }

    async fn stats(&self, port: &str) -> Result<ResponsePayload, EngineError> {
        let stats = self.with_storage(port, SqliteStorage::get_stats).await?;
        Ok(ResponsePayload::Stats(stats))
    }

    async fn search(
        &self,
        port: &str,
        query: &str,
        mode: SearchMode,
        start_ns: Option<i64>,
        end_ns: Option<i64>,
        limit: Option<u32>,
    ) -> Result<ResponsePayload, EngineError> {
        let limit = limit
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .clamp(1, MAX_SEARCH_LIMIT);
        let time_range = time_range(start_ns, end_ns);
        let query = query.to_string();

        // Compiling the regex before touching storage keeps the error message
        // about the pattern rather than about a background task.
        let regex = match mode {
            SearchMode::Regex => Some(regex::Regex::new(&query)?),
            SearchMode::Substring | SearchMode::Exact => None,
        };

        let outcome = self
            .with_storage(port, move |storage| match mode {
                SearchMode::Substring => {
                    storage
                        .search_substring(&query, time_range, limit)
                        .map(|results| SearchOutcome {
                            results,
                            truncated: false,
                        })
                }
                SearchMode::Exact => storage
                    .scan_filtered(1, time_range, limit, MAX_SEARCH_SCAN, &|line| {
                        line.payload == query
                    })
                    .map(|(results, truncated)| SearchOutcome { results, truncated }),
                SearchMode::Regex => {
                    let re = regex.expect("regex compiled for regex mode");
                    storage
                        .scan_filtered(1, time_range, limit, MAX_SEARCH_SCAN, &|line| {
                            re.is_match(&line.payload)
                        })
                        .map(|(results, truncated)| SearchOutcome { results, truncated })
                }
            })
            .await?;

        Ok(ResponsePayload::SearchResults(outcome))
    }

    async fn export(
        &self,
        port: &str,
        output_path: &str,
        file_format: Option<ExportFormat>,
        start_line: Option<i64>,
        end_line: Option<i64>,
    ) -> Result<ResponsePayload, EngineError> {
        let path = PathBuf::from(output_path);
        validate_output_path(&path)?;

        let format = file_format.unwrap_or_default();
        let start = start_line.unwrap_or(1);
        let end = end_line.unwrap_or(i64::MAX);
        if end < start {
            return Err(EngineError::invalid(format!(
                "end line {end} is before start line {start}"
            )));
        }

        let target = path.clone();
        let lines_exported = self
            .with_storage(port, move |storage| {
                let lines = storage.export_range(start, end)?;
                let file = std::fs::File::create(&target)?;
                let mut writer = std::io::BufWriter::new(file);
                export::write_all(&mut writer, &lines, format).map_err(|e| match e {
                    export::ExportError::Io(io) => StorageError::Io(io),
                    other @ export::ExportError::UnknownFormat(_) => {
                        StorageError::Io(std::io::Error::other(other.to_string()))
                    }
                })
            })
            .await?;

        let absolute = std::fs::canonicalize(&path).unwrap_or(path);
        Ok(ResponsePayload::ExportSuccess {
            lines_exported,
            path: absolute.display().to_string(),
            file_format: format,
        })
    }

    async fn clear(&self, port: &str, archive: bool) -> Result<ResponsePayload, EngineError> {
        let archive_path = archive
            .then(|| paths::archive_path(self.archive_dir(), port, &paths::archive_timestamp()));
        let target = archive_path.clone();

        let lines_cleared = self
            .with_storage(port, move |storage| {
                let count = storage.line_count()?;
                storage.clear(target.as_deref())?;
                Ok(count)
            })
            .await?;

        Ok(ResponsePayload::ClearSuccess {
            lines_cleared,
            archive_path: archive_path.map(|p| p.display().to_string()),
        })
    }

    // ------------------------------------------------------------ writing

    async fn write_data(
        &self,
        port: &str,
        data: &str,
        is_hex: bool,
    ) -> Result<ResponsePayload, EngineError> {
        let bytes = if is_hex || crate::hex::has_prefix(data) {
            crate::hex::decode(data)?
        } else {
            data.as_bytes().to_vec()
        };

        let bytes_written = bytes.len();
        self.port_manager.write(port, bytes).await?;

        // Recorded for every transport, so the GUI history is complete no
        // matter which client sent the command.
        if let Ok(storage) = self.storage(port).await {
            let command = data.to_string();
            tokio::task::spawn_blocking(move || {
                let guard = storage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Err(e) = guard.append_send_history(&command) {
                    tracing::debug!(error = %e, "could not append send history");
                }
            })
            .await?;
        }

        Ok(ResponsePayload::WriteSuccess { bytes_written })
    }

    async fn send_break(
        &self,
        port: &str,
        duration_ms: Option<u64>,
    ) -> Result<ResponsePayload, EngineError> {
        let duration_ms = duration_ms.unwrap_or(DEFAULT_BREAK_MS);
        let handle = self.port_manager.get_serial_port(port).await?;

        let guard = handle.lock().await;
        guard
            .set_break(true)
            .map_err(|e| EngineError::Tool(format!("failed to set serial break: {e}")))?;
        tokio::time::sleep(std::time::Duration::from_millis(duration_ms)).await;
        let cleared = guard.set_break(false);
        drop(guard);
        cleared.map_err(|e| EngineError::Tool(format!("failed to clear serial break: {e}")))?;

        self.log_to_buffer(
            port,
            &format!("━━━ SERIAL BREAK ({duration_ms}ms) SENT ━━━"),
        )
        .await;

        Ok(ResponsePayload::BreakSuccess { duration_ms })
    }

    async fn set_signal(
        &self,
        port: &str,
        dtr: Option<bool>,
        rts: Option<bool>,
    ) -> Result<ResponsePayload, EngineError> {
        if dtr.is_none() && rts.is_none() {
            return Err(EngineError::invalid(
                "at least one of dtr or rts must be specified",
            ));
        }

        let handle = self.port_manager.get_serial_port(port).await?;
        let mut applied = Vec::new();

        let guard = handle.lock().await;
        if let Some(value) = dtr {
            guard
                .set_dtr(value)
                .map_err(|e| EngineError::Tool(format!("failed to set DTR: {e}")))?;
            applied.push(format!("DTR={}", level(value)));
        }
        if let Some(value) = rts {
            guard
                .set_rts(value)
                .map_err(|e| EngineError::Tool(format!("failed to set RTS: {e}")))?;
            applied.push(format!("RTS={}", level(value)));
        }
        drop(guard);

        Ok(ResponsePayload::SignalSuccess { applied })
    }

    async fn execute_macro(
        &self,
        port: &str,
        macro_name: &str,
    ) -> Result<ResponsePayload, EngineError> {
        let steps =
            self.config
                .macro_steps(macro_name)
                .ok_or_else(|| EngineError::UnknownMacro {
                    name: macro_name.to_string(),
                    available: self.config.available_macros().join(", "),
                })?;

        let handle = self.port_manager.get_serial_port(port).await?;
        let mut executed = Vec::new();

        for step in steps {
            match step {
                MacroStep::Dtr { value } => {
                    handle
                        .lock()
                        .await
                        .set_dtr(value)
                        .map_err(|e| EngineError::Tool(format!("DTR error: {e}")))?;
                    executed.push(format!("DTR={}", level(value)));
                }
                MacroStep::Rts { value } => {
                    handle
                        .lock()
                        .await
                        .set_rts(value)
                        .map_err(|e| EngineError::Tool(format!("RTS error: {e}")))?;
                    executed.push(format!("RTS={}", level(value)));
                }
                MacroStep::Delay { ms } => {
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    executed.push(format!("delay {ms}ms"));
                }
                MacroStep::Write { value } => {
                    // An unparseable step aborts the macro instead of silently
                    // sending nothing.
                    let bytes = if crate::hex::has_prefix(&value) {
                        crate::hex::decode(&value)?
                    } else {
                        value.as_bytes().to_vec()
                    };
                    handle
                        .lock()
                        .await
                        .write_all(&bytes)
                        .await
                        .map_err(|e| EngineError::Tool(format!("write error: {e}")))?;
                    executed.push(format!("write {} bytes", bytes.len()));
                }
            }
        }

        self.log_to_buffer(
            port,
            &format!(
                "━━━ MACRO '{macro_name}' executed ({}) ━━━",
                executed.join(" → ")
            ),
        )
        .await;

        Ok(ResponsePayload::MacroSuccess {
            executed_steps: executed,
        })
    }

    // ------------------------------------------------------------ transfers

    async fn send_file(
        &self,
        port: &str,
        file_path: &str,
        protocol: FileTransferProtocol,
    ) -> Result<ResponsePayload, EngineError> {
        let path = Path::new(file_path);
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file.bin")
            .to_string();

        let data = tokio::fs::read(file_path)
            .await
            .map_err(|e| EngineError::io(file_path, e))?;

        let handle = self.port_manager.get_serial_port(port).await?;
        let mut guard = handle.lock().await;
        let bytes = crate::modem::send(&mut *guard, protocol, &file_name, &data)
            .await
            .map_err(EngineError::Transfer);
        drop(guard);
        let bytes_transferred = u64::try_from(bytes?).unwrap_or(u64::MAX);

        self.log_to_buffer(
            port,
            &format!(
                "━━━ TRANSFER SEND {protocol:?} '{file_name}' ({bytes_transferred} bytes) → OK ━━━"
            ),
        )
        .await;

        Ok(ResponsePayload::TransferSuccess {
            bytes_transferred,
            file_name,
            protocol,
            path: Some(path.display().to_string()),
        })
    }

    async fn receive_file(
        &self,
        port: &str,
        output_dir: &str,
        protocol: FileTransferProtocol,
    ) -> Result<ResponsePayload, EngineError> {
        let handle = self.port_manager.get_serial_port(port).await?;
        let mut guard = handle.lock().await;
        let received = crate::modem::receive(&mut *guard, protocol)
            .await
            .map_err(EngineError::Transfer);
        drop(guard);
        let (file_name, data) = received?;

        let dir = Path::new(output_dir);
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| EngineError::io(dir, e))?;
        let out_path = dir.join(&file_name);
        tokio::fs::write(&out_path, &data)
            .await
            .map_err(|e| EngineError::io(&out_path, e))?;

        let bytes_transferred = u64::try_from(data.len()).unwrap_or(u64::MAX);
        self.log_to_buffer(
            port,
            &format!(
                "━━━ TRANSFER RECV {protocol:?} '{file_name}' ({bytes_transferred} bytes) → OK ━━━"
            ),
        )
        .await;

        Ok(ResponsePayload::TransferSuccess {
            bytes_transferred,
            file_name,
            protocol,
            path: Some(out_path.display().to_string()),
        })
    }

    // ------------------------------------------------------------ esp

    /// Run an espflash operation with the port temporarily released.
    ///
    /// The availability check, the release, the reopen and the buffer note used
    /// to be repeated for every ESP command.
    #[cfg(feature = "esp")]
    async fn esp_operation<F, Fut>(
        &self,
        port: &str,
        log: Option<String>,
        op: F,
    ) -> Result<ResponsePayload, EngineError>
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = Result<String, String>>,
    {
        if !crate::esp::is_available().await {
            return Err(EngineError::Tool(
                "espflash not found in PATH. Install with: cargo install espflash".to_string(),
            ));
        }

        let reopen = self.release_port(port).await;
        let result = op(port.to_string()).await;
        if let Some(config) = reopen {
            self.reopen_port(port, config).await;
        }

        if let Some(log) = log {
            let status = if result.is_ok() { "OK" } else { "FAILED" };
            self.log_to_buffer(port, &format!("━━━ {log} → {status} ━━━"))
                .await;
        }

        result
            .map(ResponsePayload::EspSuccess)
            .map_err(EngineError::Tool)
    }

    /// Close a managed port so an external tool can use it.
    #[cfg(feature = "esp")]
    async fn release_port(&self, port: &str) -> Option<PortConfig> {
        if self.port_manager.get_state(port).await.is_err() {
            return None;
        }
        match self.port_manager.close_with_config(port).await {
            Ok(config) => {
                tracing::info!(port = %port, "released port for external tool");
                Some(config)
            }
            Err(e) => {
                tracing::warn!(port = %port, error = %e, "could not release port");
                None
            }
        }
    }

    /// Reopen a port previously released by [`Self::release_port`].
    #[cfg(feature = "esp")]
    async fn reopen_port(&self, port: &str, config: PortConfig) {
        tokio::time::sleep(ESP_REOPEN_DELAY).await;
        match self
            .port_manager
            .open_serial(port.to_string(), config, self.data_dir().to_path_buf())
            .await
        {
            Ok(()) => tracing::info!(port = %port, "reopened port"),
            Err(e) => tracing::warn!(port = %port, error = %e, "failed to reopen port"),
        }
    }

    /// Insert an informational message or event marker into a port's buffer.
    pub async fn log_to_buffer(&self, port: &str, message: &str) {
        let Ok(storage) = self.storage(port).await else {
            return;
        };
        let message = message.to_string();
        let task = tokio::task::spawn_blocking(move || {
            let guard = storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
            guard.insert_lines(&[(now, &message)])
        });
        if let Ok(Err(e)) = task.await {
            tracing::debug!(error = %e, "could not write buffer marker");
        }
    }
}

/// Serial ports the operating system reports, one entry per physical device.
#[must_use]
pub fn hardware_ports() -> Vec<String> {
    crate::port_manager::available_ports()
}

const fn level(value: bool) -> &'static str {
    if value { "HIGH" } else { "LOW" }
}

fn time_range(start_ns: Option<i64>, end_ns: Option<i64>) -> Option<TimeRange> {
    match (start_ns, end_ns) {
        (None, None) => None,
        (start, end) => Some(TimeRange {
            start_ns: start.unwrap_or(0),
            end_ns: end.unwrap_or(i64::MAX),
        }),
    }
}

/// Translate a read window into a first line id.
fn resolve_start(window: &ReadWindow, total_lines: u64) -> i64 {
    let total = i64::try_from(total_lines).unwrap_or(i64::MAX);
    if let Some(tail) = window.tail {
        let tail = i64::from(tail);
        return (total - tail + 1).max(1);
    }
    if let Some(after) = window.after_id {
        return after.saturating_add(1).max(1);
    }
    match window.start_id {
        Some(start) if start < 0 => (total + start + 1).max(1),
        Some(start) => start.max(1),
        None => 1,
    }
}

/// Reject output paths that escape upwards or point at a missing directory.
fn validate_output_path(path: &Path) -> Result<(), EngineError> {
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(EngineError::invalid(
            "output path must not contain '..' components",
        ));
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        return Err(EngineError::invalid(format!(
            "parent directory does not exist: {}",
            parent.display()
        )));
    }
    Ok(())
}

/// Convenience alias used by transports that need a plain string error.
impl From<EngineError> for String {
    fn from(value: EngineError) -> Self {
        value.to_string()
    }
}

/// Read a `StoredLine` list as plain payload strings.
#[must_use]
pub fn payloads(lines: &[StoredLine]) -> Vec<&str> {
    lines.iter().map(|l| l.payload.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PortSettings;
    use crate::serial_params::Parity;
    use crate::testutil::mock_serial::mock_serial;

    fn make_test_engine() -> (CommandEngine, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.global.data_dir = dir.path().to_path_buf();
        config.global.archive_dir = dir.path().join("archive");
        let pm = PortManagerHandle::new();
        let state_db = Arc::new(Mutex::new(StateDb::open_memory().unwrap()));
        let engine = CommandEngine::new(pm, state_db, Arc::new(config));
        (engine, dir)
    }

    async fn open_mock(
        engine: &CommandEngine,
        name: &str,
    ) -> crate::testutil::mock_serial::MockSerialControl {
        let (mock, ctrl) = mock_serial(4096);
        let storage = Arc::new(Mutex::new(SqliteStorage::open_memory().unwrap()));
        engine
            .port_manager()
            .open(name.into(), mock, PortConfig::default(), storage)
            .await
            .unwrap();
        ctrl
    }

    #[tokio::test]
    async fn test_engine_ping_and_list() {
        let (engine, _dir) = make_test_engine();
        let resp = engine.execute(RequestPayload::Ping).await.unwrap();
        assert_eq!(resp, ResponsePayload::Pong);

        let list_resp = engine.execute(RequestPayload::ListPorts).await.unwrap();
        assert_eq!(list_resp, ResponsePayload::PortList(vec![]));
    }

    #[tokio::test]
    async fn test_engine_mock_port_lifecycle() {
        let (engine, _dir) = make_test_engine();
        let ctrl = open_mock(&engine, "mock1").await;

        let list = engine.execute(RequestPayload::ListPorts).await.unwrap();
        let ResponsePayload::PortList(ports) = list else {
            panic!("expected PortList");
        };
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].name, "mock1");

        ctrl.feed_lines(&["line 1", "line 2", "line 3"]).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let read = engine
            .execute(RequestPayload::ReadLines {
                port: "mock1".into(),
                window: ReadWindow {
                    start_id: Some(1),
                    limit: Some(10),
                    ..ReadWindow::default()
                },
            })
            .await
            .unwrap();
        let ResponsePayload::Lines(page) = read else {
            panic!("expected Lines");
        };
        assert_eq!(page.lines.len(), 3);
        assert_eq!(page.total_lines, 3);
        assert_eq!(page.next_after_id, 3);
        assert_eq!(page.lines[0].payload, "line 1");

        let search = engine
            .execute(RequestPayload::Search {
                port: "mock1".into(),
                query: "line 2".into(),
                mode: SearchMode::Substring,
                start_ns: None,
                end_ns: None,
                limit: Some(10),
            })
            .await
            .unwrap();
        let ResponsePayload::SearchResults(outcome) = search else {
            panic!("expected SearchResults");
        };
        assert_eq!(outcome.results.len(), 1);
        assert!(!outcome.truncated);

        let stats = engine
            .execute(RequestPayload::GetStats {
                port: "mock1".into(),
            })
            .await
            .unwrap();
        let ResponsePayload::Stats(stats) = stats else {
            panic!("expected Stats");
        };
        assert_eq!(stats.total_lines, 3);

        let closed = engine
            .execute(RequestPayload::ClosePort {
                name: "mock1".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            closed,
            ResponsePayload::PortClosed {
                name: "mock1".into()
            }
        );
    }

    #[tokio::test]
    async fn tail_and_after_id_agree_on_the_same_lines() {
        let (engine, _dir) = make_test_engine();
        let ctrl = open_mock(&engine, "p").await;
        ctrl.feed_lines(&["a", "b", "c", "d", "e"]).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let tail = engine
            .execute(RequestPayload::ReadLines {
                port: "p".into(),
                window: ReadWindow {
                    tail: Some(2),
                    ..ReadWindow::default()
                },
            })
            .await
            .unwrap();
        let ResponsePayload::Lines(page) = tail else {
            panic!()
        };
        assert_eq!(
            page.lines
                .iter()
                .map(|l| l.payload.as_str())
                .collect::<Vec<_>>(),
            vec!["d", "e"]
        );

        let after = engine
            .execute(RequestPayload::ReadLines {
                port: "p".into(),
                window: ReadWindow {
                    after_id: Some(3),
                    ..ReadWindow::default()
                },
            })
            .await
            .unwrap();
        let ResponsePayload::Lines(page) = after else {
            panic!()
        };
        assert_eq!(
            page.lines
                .iter()
                .map(|l| l.payload.as_str())
                .collect::<Vec<_>>(),
            vec!["d", "e"]
        );
    }

    #[tokio::test]
    async fn negative_start_counts_from_the_end() {
        let (engine, _dir) = make_test_engine();
        let ctrl = open_mock(&engine, "p").await;
        ctrl.feed_lines(&["a", "b", "c"]).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let resp = engine
            .execute(RequestPayload::ReadLines {
                port: "p".into(),
                window: ReadWindow {
                    start_id: Some(-1),
                    ..ReadWindow::default()
                },
            })
            .await
            .unwrap();
        let ResponsePayload::Lines(page) = resp else {
            panic!()
        };
        assert_eq!(page.lines.len(), 1);
        assert_eq!(page.lines[0].payload, "c");
    }

    #[tokio::test]
    async fn regex_search_finds_matches_beyond_the_first_page() {
        let (engine, _dir) = make_test_engine();
        let ctrl = open_mock(&engine, "p").await;
        let mut lines: Vec<String> = (0..2000).map(|i| format!("noise {i}")).collect();
        lines.push("NEEDLE 4711".into());
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        ctrl.feed_lines(&refs).await;
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        let resp = engine
            .execute(RequestPayload::Search {
                port: "p".into(),
                query: "^NEEDLE".into(),
                mode: SearchMode::Regex,
                start_ns: None,
                end_ns: None,
                limit: Some(10),
            })
            .await
            .unwrap();
        let ResponsePayload::SearchResults(outcome) = resp else {
            panic!()
        };
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].payload, "NEEDLE 4711");
    }

    #[tokio::test]
    async fn exact_search_does_not_match_substrings() {
        let (engine, _dir) = make_test_engine();
        let ctrl = open_mock(&engine, "p").await;
        ctrl.feed_lines(&["ok", "ok and more"]).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let resp = engine
            .execute(RequestPayload::Search {
                port: "p".into(),
                query: "ok".into(),
                mode: SearchMode::Exact,
                start_ns: None,
                end_ns: None,
                limit: None,
            })
            .await
            .unwrap();
        let ResponsePayload::SearchResults(outcome) = resp else {
            panic!()
        };
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].payload, "ok");
    }

    #[tokio::test]
    async fn invalid_regex_is_reported_as_such() {
        let (engine, _dir) = make_test_engine();
        let _ctrl = open_mock(&engine, "p").await;
        let err = engine
            .execute(RequestPayload::Search {
                port: "p".into(),
                query: "[unclosed".into(),
                mode: SearchMode::Regex,
                start_ns: None,
                end_ns: None,
                limit: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid regex"));
    }

    #[tokio::test]
    async fn export_writes_the_shared_format() {
        let (engine, dir) = make_test_engine();
        let ctrl = open_mock(&engine, "p").await;
        ctrl.feed_lines(&["hello"]).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let out = dir.path().join("out.csv");
        let resp = engine
            .execute(RequestPayload::Export {
                port: "p".into(),
                output_path: out.display().to_string(),
                file_format: Some(ExportFormat::Csv),
                start_line: None,
                end_line: None,
            })
            .await
            .unwrap();
        let ResponsePayload::ExportSuccess { lines_exported, .. } = resp else {
            panic!()
        };
        assert_eq!(lines_exported, 1);

        let content = std::fs::read_to_string(&out).unwrap();
        assert!(content.starts_with("line,timestamp,timestamp_ns,payload\n"));
        assert!(content.contains("\"hello\""));
    }

    #[tokio::test]
    async fn export_rejects_parent_traversal() {
        let (engine, _dir) = make_test_engine();
        let _ctrl = open_mock(&engine, "p").await;
        let err = engine
            .execute(RequestPayload::Export {
                port: "p".into(),
                output_path: "../escape.txt".into(),
                file_format: None,
                start_line: None,
                end_line: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains(".."));
    }

    #[tokio::test]
    async fn clear_archives_into_the_configured_directory() {
        let (engine, dir) = make_test_engine();
        let ctrl = open_mock(&engine, "p").await;
        ctrl.feed_lines(&["one"]).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let resp = engine
            .execute(RequestPayload::Clear {
                port: "p".into(),
                archive_current: Some(true),
            })
            .await
            .unwrap();
        let ResponsePayload::ClearSuccess {
            lines_cleared,
            archive_path,
        } = resp
        else {
            panic!()
        };
        assert_eq!(lines_cleared, 1);
        let archive = archive_path.expect("archive path");
        assert!(
            archive.starts_with(dir.path().join("archive").to_str().unwrap()),
            "archive {archive} must sit in the configured archive dir"
        );
    }

    #[tokio::test]
    async fn unknown_macro_lists_available_ones() {
        let (engine, _dir) = make_test_engine();
        let _ctrl = open_mock(&engine, "p").await;
        let err = engine
            .execute(RequestPayload::ExecuteMacro {
                port: "p".into(),
                macro_name: "nope".into(),
            })
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("reset"));
        assert!(text.contains("enter_bootloader"));
    }

    #[tokio::test]
    async fn invalid_baud_is_rejected() {
        let (engine, _dir) = make_test_engine();
        let err = engine
            .execute(RequestPayload::OpenPort {
                name: "/dev/definitely-not-a-port".into(),
                settings: PortSettings {
                    baudrate: Some(0),
                    ..PortSettings::default()
                },
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("baud"));
    }

    #[tokio::test]
    async fn open_port_uses_configured_defaults_for_known_ports() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.global.data_dir = dir.path().to_path_buf();
        config.ports.insert(
            "/dev/configured".into(),
            PortConfig {
                baudrate: 9600,
                parity: Parity::Even,
                ..PortConfig::default()
            },
        );
        let engine = CommandEngine::new(
            PortManagerHandle::new(),
            Arc::new(Mutex::new(StateDb::open_memory().unwrap())),
            Arc::new(config),
        );

        let merged = engine
            .merge_settings("/dev/configured", PortSettings::default())
            .unwrap();
        assert_eq!(merged.baudrate, 9600);
        assert_eq!(merged.parity, Parity::Even);

        let overridden = engine
            .merge_settings(
                "/dev/configured",
                PortSettings {
                    baudrate: Some(19200),
                    ..PortSettings::default()
                },
            )
            .unwrap();
        assert_eq!(overridden.baudrate, 19200);
        assert_eq!(overridden.parity, Parity::Even);
    }

    #[test]
    fn resolve_start_handles_every_window_shape() {
        let w = |f: fn(&mut ReadWindow)| {
            let mut w = ReadWindow::default();
            f(&mut w);
            w
        };
        assert_eq!(resolve_start(&ReadWindow::default(), 10), 1);
        assert_eq!(resolve_start(&w(|w| w.tail = Some(3)), 10), 8);
        assert_eq!(resolve_start(&w(|w| w.tail = Some(30)), 10), 1);
        assert_eq!(resolve_start(&w(|w| w.after_id = Some(5)), 10), 6);
        assert_eq!(resolve_start(&w(|w| w.start_id = Some(-2)), 10), 9);
        assert_eq!(resolve_start(&w(|w| w.start_id = Some(0)), 10), 1);
        assert_eq!(resolve_start(&w(|w| w.start_id = Some(7)), 10), 7);
    }

    #[test]
    fn time_range_fills_open_ends() {
        assert!(time_range(None, None).is_none());
        let tr = time_range(Some(5), None).unwrap();
        assert_eq!((tr.start_ns, tr.end_ns), (5, i64::MAX));
        let tr = time_range(None, Some(5)).unwrap();
        assert_eq!((tr.start_ns, tr.end_ns), (0, 5));
    }
}
