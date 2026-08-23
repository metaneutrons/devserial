// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Core execution engine that dispatches RPC requests to `PortManager` and `SqliteStorage`.
//!
//! Transport-agnostic: shared by MCP server, IPC daemon, and CLI.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::{Config, MacroStep, PortConfig};
use crate::port_manager::PortManagerHandle;
use crate::protocol::{PortInfoResponse, RequestPayload, ResponsePayload};
use crate::state::StateDb;
use crate::storage::{StorageError, TimeRange};

/// Core command dispatcher for devserial operations.
#[derive(Clone)]
pub struct CommandEngine {
    port_manager: PortManagerHandle,
    state_db: Arc<Mutex<StateDb>>,
    config: Arc<Config>,
    data_dir: PathBuf,
}

impl CommandEngine {
    /// Create a new command engine with the given components.
    #[must_use]
    pub const fn new(
        port_manager: PortManagerHandle,
        state_db: Arc<Mutex<StateDb>>,
        config: Arc<Config>,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            port_manager,
            state_db,
            config,
            data_dir,
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

    /// Access the data directory.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Execute a request payload and return a response payload.
    ///
    /// # Errors
    /// Returns error message on failure.
    #[allow(clippy::too_many_lines)]
    pub async fn execute(&self, payload: RequestPayload) -> Result<ResponsePayload, String> {
        match payload {
            RequestPayload::Ping => Ok(ResponsePayload::Pong),
            RequestPayload::ListPorts => self.handle_list_ports().await,
            RequestPayload::OpenPort {
                name,
                baudrate,
                data_bits,
                parity,
                stop_bits,
                flow_control,
            } => {
                self.handle_open_port(
                    &name,
                    baudrate,
                    data_bits,
                    parity.as_deref(),
                    stop_bits,
                    flow_control.as_deref(),
                )
                .await
            }
            RequestPayload::ClosePort { name } => self.handle_close_port(&name).await,
            RequestPayload::ReadLines {
                port,
                start_id,
                count,
                tail,
            } => self.handle_read_lines(&port, start_id, count, tail).await,
            RequestPayload::WriteData { port, data, is_hex } => {
                self.handle_write_data(&port, &data, is_hex).await
            }
            RequestPayload::SendBreak { port, duration_ms } => {
                self.handle_send_break(&port, duration_ms).await
            }
            RequestPayload::SendFile {
                port,
                file_path,
                protocol,
            } => self.handle_send_file(&port, &file_path, protocol).await,
            RequestPayload::ReceiveFile {
                port,
                output_dir,
                protocol,
            } => self.handle_receive_file(&port, &output_dir, protocol).await,
            RequestPayload::SetSignal { port, dtr, rts } => {
                self.handle_set_signal(&port, dtr, rts).await
            }
            RequestPayload::ExecuteMacro { port, macro_name } => {
                self.handle_execute_macro(&port, &macro_name).await
            }
            RequestPayload::Search {
                port,
                query,
                is_regex,
                start_ns,
                end_ns,
                limit,
            } => {
                self.handle_search(&port, &query, is_regex, start_ns, end_ns, limit)
                    .await
            }
            RequestPayload::Export {
                port,
                output_path,
                file_format,
                start_line,
                end_line,
            } => {
                self.handle_export(
                    &port,
                    &output_path,
                    file_format.as_deref(),
                    start_line,
                    end_line,
                )
                .await
            }
            RequestPayload::Clear {
                port,
                archive_current,
            } => {
                self.handle_clear(&port, archive_current.unwrap_or(false))
                    .await
            }
            RequestPayload::GetStats { port } => self.handle_get_stats(&port).await,
            #[cfg(feature = "esp")]
            RequestPayload::EspFlash {
                port,
                firmware_path,
                baud,
            } => self.handle_esp_flash(&port, &firmware_path, baud).await,
            #[cfg(feature = "esp")]
            RequestPayload::EspInfo { port } => self.handle_esp_info(&port).await,
            #[cfg(feature = "esp")]
            RequestPayload::EspErase { port } => self.handle_esp_erase(&port).await,
            RequestPayload::Shutdown => Ok(ResponsePayload::ShutdownAck),
        }
    }

    async fn handle_list_ports(&self) -> Result<ResponsePayload, String> {
        let ports = self.port_manager.list().await;
        let response_ports = ports
            .into_iter()
            .map(|p| PortInfoResponse {
                name: p.name,
                state: p.state,
                total_lines: p.total_lines,
            })
            .collect();
        Ok(ResponsePayload::PortList(response_ports))
    }

    async fn handle_open_port(
        &self,
        name: &str,
        baudrate: Option<u32>,
        data_bits: Option<u8>,
        parity: Option<&str>,
        stop_bits: Option<u8>,
        flow_control: Option<&str>,
    ) -> Result<ResponsePayload, String> {
        let base = self.config.ports.get(name).cloned().unwrap_or_default();
        let config = PortConfig {
            baudrate: baudrate.unwrap_or(base.baudrate),
            data_bits: data_bits.unwrap_or(base.data_bits),
            parity: parity.map_or_else(|| base.parity.clone(), String::from),
            stop_bits: stop_bits.unwrap_or(base.stop_bits),
            flow_control: flow_control.map_or_else(|| base.flow_control.clone(), String::from),
            ..base
        };

        self.port_manager
            .open_serial(name.to_string(), config.clone(), self.data_dir.clone())
            .await
            .map_err(|e| e.to_string())?;

        if let Ok(db) = self.state_db.lock() {
            db.port_opened(name, &config).ok();
        }

        let summary = format!(
            "{}baud, {}{}{}",
            config.baudrate,
            config.data_bits,
            config.parity.chars().next().unwrap_or('N'),
            config.stop_bits
        );

        Ok(ResponsePayload::PortOpened {
            name: name.to_string(),
            config_summary: summary,
        })
    }

    async fn handle_close_port(&self, name: &str) -> Result<ResponsePayload, String> {
        self.port_manager
            .close(name)
            .await
            .map_err(|e| e.to_string())?;

        if let Ok(db) = self.state_db.lock() {
            db.port_closed(name).ok();
        }

        Ok(ResponsePayload::PortClosed {
            name: name.to_string(),
        })
    }

    async fn handle_read_lines(
        &self,
        port: &str,
        start_id: Option<i64>,
        count: Option<u32>,
        tail: Option<u32>,
    ) -> Result<ResponsePayload, String> {
        let storage = self
            .port_manager
            .get_storage(port)
            .await
            .map_err(|e| e.to_string())?;

        let cnt = count.unwrap_or(100).min(1000);
        let lines = tokio::task::spawn_blocking(
            move || -> Result<Vec<crate::storage::StoredLine>, StorageError> {
                let s = storage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(t) = tail {
                    let stats = s.get_stats()?;
                    let start = if stats.total_lines > u64::from(t) {
                        i64::try_from(stats.total_lines - u64::from(t) + 1).unwrap_or(1)
                    } else {
                        1
                    };
                    s.read_lines(start, t)
                } else {
                    let start = start_id.unwrap_or(1);
                    s.read_lines(start, cnt)
                }
            },
        )
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;

        Ok(ResponsePayload::Lines(lines))
    }

    async fn handle_write_data(
        &self,
        port: &str,
        data_str: &str,
        is_hex: bool,
    ) -> Result<ResponsePayload, String> {
        let data: Vec<u8> = if is_hex || data_str.starts_with("0x") || data_str.starts_with("0X") {
            let hex_clean = data_str
                .strip_prefix("0x")
                .or_else(|| data_str.strip_prefix("0X"))
                .unwrap_or(data_str);
            hex_decode(hex_clean).map_err(|e| format!("invalid hex data: {e}"))?
        } else {
            data_str.as_bytes().to_vec()
        };

        let bytes_written = data.len();
        self.port_manager
            .write(port, data)
            .await
            .map_err(|e| e.to_string())?;

        // Record in send history
        if let Ok(storage) = self.port_manager.get_storage(port).await {
            let s = storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.append_send_history(data_str).ok();
        }

        Ok(ResponsePayload::WriteSuccess { bytes_written })
    }

    async fn handle_send_break(
        &self,
        port_name: &str,
        duration_ms: Option<u64>,
    ) -> Result<ResponsePayload, String> {
        let serial_port = self
            .port_manager
            .get_serial_port(port_name)
            .await
            .map_err(|e| e.to_string())?;

        let duration = std::time::Duration::from_millis(duration_ms.unwrap_or(250));
        let guard = serial_port.lock().await;
        guard
            .set_break(true)
            .map_err(|e| format!("failed to set serial break: {e}"))?;
        tokio::time::sleep(duration).await;
        guard
            .set_break(false)
            .map_err(|e| format!("failed to clear serial break: {e}"))?;
        drop(guard);

        self.log_to_buffer(port_name, "━━━ SERIAL BREAK SENT (250ms) ━━━")
            .await;

        Ok(ResponsePayload::BreakSuccess)
    }

    async fn handle_send_file(
        &self,
        port_name: &str,
        file_path: &str,
        protocol: crate::modem::FileTransferProtocol,
    ) -> Result<ResponsePayload, String> {
        let path = std::path::Path::new(file_path);
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file.bin")
            .to_string();

        let data = tokio::fs::read(file_path)
            .await
            .map_err(|e| format!("failed to read file '{file_path}': {e}"))?;

        let serial_port = self
            .port_manager
            .get_serial_port(port_name)
            .await
            .map_err(|e| e.to_string())?;

        let mut guard = serial_port.lock().await;
        let bytes_sent = match protocol {
            crate::modem::FileTransferProtocol::Xmodem => {
                crate::modem::xmodem::send(&mut *guard, &data, false, |_, _| {}).await?
            }
            crate::modem::FileTransferProtocol::XmodemCrc => {
                crate::modem::xmodem::send(&mut *guard, &data, false, |_, _| {}).await?
            }
            crate::modem::FileTransferProtocol::Xmodem1k => {
                crate::modem::xmodem::send(&mut *guard, &data, true, |_, _| {}).await?
            }
            crate::modem::FileTransferProtocol::Ymodem => {
                crate::modem::ymodem::send_file(&mut *guard, &filename, &data, |_, _| {}).await?
            }
            crate::modem::FileTransferProtocol::Zmodem => {
                crate::modem::zmodem::send_file(&mut *guard, &filename, &data, |_, _| {}).await?
            }
        };
        drop(guard);

        self.log_to_buffer(
            port_name,
            &format!("━━━ TRANSFER SEND {protocol:?} '{filename}' ({bytes_sent} bytes) → OK ━━━"),
        )
        .await;

        Ok(ResponsePayload::TransferSuccess {
            bytes_transferred: bytes_sent as u64,
            file_name: filename,
            protocol,
        })
    }

    async fn handle_receive_file(
        &self,
        port_name: &str,
        output_dir: &str,
        protocol: crate::modem::FileTransferProtocol,
    ) -> Result<ResponsePayload, String> {
        let serial_port = self
            .port_manager
            .get_serial_port(port_name)
            .await
            .map_err(|e| e.to_string())?;

        let mut guard = serial_port.lock().await;
        let (filename, data) = match protocol {
            crate::modem::FileTransferProtocol::Xmodem => {
                let bytes = crate::modem::xmodem::receive(&mut *guard, false, |_| {}).await?;
                ("xmodem_recv.bin".to_string(), bytes)
            }
            crate::modem::FileTransferProtocol::XmodemCrc => {
                let bytes = crate::modem::xmodem::receive(&mut *guard, true, |_| {}).await?;
                ("xmodem_recv.bin".to_string(), bytes)
            }
            crate::modem::FileTransferProtocol::Xmodem1k => {
                let bytes = crate::modem::xmodem::receive(&mut *guard, true, |_| {}).await?;
                ("xmodem1k_recv.bin".to_string(), bytes)
            }
            crate::modem::FileTransferProtocol::Ymodem => {
                crate::modem::ymodem::receive_file(&mut *guard, |_, _| {}).await?
            }
            crate::modem::FileTransferProtocol::Zmodem => {
                crate::modem::zmodem::receive_file(&mut *guard, |_, _| {}).await?
            }
        };
        drop(guard);

        let out_dir = std::path::Path::new(output_dir);
        if !out_dir.exists() {
            tokio::fs::create_dir_all(out_dir)
                .await
                .map_err(|e| format!("failed to create output dir '{output_dir}': {e}"))?;
        }

        let out_path = out_dir.join(&filename);
        tokio::fs::write(&out_path, &data).await.map_err(|e| {
            format!(
                "failed to write received file to '{}': {e}",
                out_path.display()
            )
        })?;

        self.log_to_buffer(
            port_name,
            &format!(
                "━━━ TRANSFER RECV {protocol:?} '{filename}' ({} bytes) → OK ━━━",
                data.len()
            ),
        )
        .await;

        Ok(ResponsePayload::TransferSuccess {
            bytes_transferred: data.len() as u64,
            file_name: filename,
            protocol,
        })
    }

    async fn handle_set_signal(
        &self,
        port_name: &str,
        dtr: Option<bool>,
        rts: Option<bool>,
    ) -> Result<ResponsePayload, String> {
        if dtr.is_none() && rts.is_none() {
            return Err("at least one of dtr or rts must be specified".to_string());
        }

        let serial_port = self
            .port_manager
            .get_serial_port(port_name)
            .await
            .map_err(|e| e.to_string())?;

        let guard = serial_port.lock().await;
        if let Some(dtr_val) = dtr {
            guard
                .set_dtr(dtr_val)
                .map_err(|e| format!("failed to set DTR: {e}"))?;
        }
        if let Some(rts_val) = rts {
            guard
                .set_rts(rts_val)
                .map_err(|e| format!("failed to set RTS: {e}"))?;
        }
        drop(guard);

        Ok(ResponsePayload::SignalSuccess)
    }

    async fn handle_execute_macro(
        &self,
        port_name: &str,
        macro_name: &str,
    ) -> Result<ResponsePayload, String> {
        let serial_port = self
            .port_manager
            .get_serial_port(port_name)
            .await
            .map_err(|e| e.to_string())?;

        let steps: Vec<MacroStep> = if let Some(user_macro) = self.config.macros.get(macro_name) {
            user_macro.steps.clone()
        } else {
            match macro_name {
                "reset" => vec![
                    MacroStep::Dtr { value: false },
                    MacroStep::Delay { ms: 100 },
                    MacroStep::Dtr { value: true },
                ],
                "enter_bootloader" => vec![
                    MacroStep::Rts { value: true },
                    MacroStep::Dtr { value: false },
                    MacroStep::Delay { ms: 50 },
                    MacroStep::Dtr { value: true },
                    MacroStep::Delay { ms: 50 },
                    MacroStep::Rts { value: false },
                ],
                "break" => vec![MacroStep::Write {
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
                    return Err(format!(
                        "unknown macro '{other}'. Available: {}",
                        available.join(", ")
                    ));
                }
            }
        };

        let mut executed = Vec::new();
        for step in steps {
            match step {
                MacroStep::Dtr { value } => {
                    serial_port
                        .lock()
                        .await
                        .set_dtr(value)
                        .map_err(|e| format!("DTR error: {e}"))?;
                    executed.push(format!("DTR={}", if value { "HIGH" } else { "LOW" }));
                }
                MacroStep::Rts { value } => {
                    serial_port
                        .lock()
                        .await
                        .set_rts(value)
                        .map_err(|e| format!("RTS error: {e}"))?;
                    executed.push(format!("RTS={}", if value { "HIGH" } else { "LOW" }));
                }
                MacroStep::Delay { ms } => {
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    executed.push(format!("delay {ms}ms"));
                }
                MacroStep::Write { value } => {
                    let data = if value.starts_with("0x") || value.starts_with("0X") {
                        hex_decode(&value[2..]).unwrap_or_default()
                    } else {
                        value.as_bytes().to_vec()
                    };
                    serial_port
                        .lock()
                        .await
                        .write_all(&data)
                        .await
                        .map_err(|e| format!("write error: {e}"))?;
                    executed.push(format!("write {} bytes", data.len()));
                }
            }
        }

        // Log separator in buffer
        let msg = format!(
            "━━━ MACRO '{macro_name}' executed ({}) ━━━",
            executed.join(" → ")
        );
        self.log_to_buffer(port_name, &msg).await;

        Ok(ResponsePayload::MacroSuccess {
            executed_steps: executed,
        })
    }

    async fn handle_search(
        &self,
        port: &str,
        query: &str,
        is_regex: bool,
        start_ns: Option<i64>,
        end_ns: Option<i64>,
        limit: Option<u32>,
    ) -> Result<ResponsePayload, String> {
        let max = limit.unwrap_or(100).min(1000);
        let time_range = match (start_ns, end_ns) {
            (Some(s), Some(e)) => Some(TimeRange {
                start_ns: s,
                end_ns: e,
            }),
            (Some(s), None) => Some(TimeRange {
                start_ns: s,
                end_ns: i64::MAX,
            }),
            (None, Some(e)) => Some(TimeRange {
                start_ns: 0,
                end_ns: e,
            }),
            (None, None) => None,
        };

        let storage = self
            .port_manager
            .get_storage(port)
            .await
            .map_err(|e| e.to_string())?;

        let q = query.to_string();

        let results = tokio::task::spawn_blocking(
            move || -> Result<Vec<crate::storage::StoredLine>, StorageError> {
                let s = storage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if is_regex {
                    let re = regex::Regex::new(&q).map_err(StorageError::InvalidRegex)?;
                    let candidates = if let Some(tr) = time_range {
                        s.search_time_range(tr.start_ns, tr.end_ns, max * 10)?
                    } else {
                        s.read_lines(1, max * 10)?
                    };
                    Ok(candidates
                        .into_iter()
                        .filter(|l| re.is_match(&l.payload))
                        .take(max as usize)
                        .collect())
                } else {
                    s.search_substring(&q, time_range, max)
                }
            },
        )
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;

        Ok(ResponsePayload::SearchResults(results))
    }

    async fn handle_export(
        &self,
        port: &str,
        output_path: &str,
        file_format: Option<&str>,
        start_line: Option<i64>,
        end_line: Option<i64>,
    ) -> Result<ResponsePayload, String> {
        let path = PathBuf::from(output_path);

        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("invalid output path: path traversal not allowed".to_string());
        }

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                return Err(format!(
                    "parent directory does not exist: {}",
                    parent.display()
                ));
            }
        }

        let start = start_line.unwrap_or(1);
        let end = end_line.unwrap_or(i64::MAX);
        let format = file_format.unwrap_or("txt").to_string();

        let storage = self
            .port_manager
            .get_storage(port)
            .await
            .map_err(|e| e.to_string())?;

        let export_path = path.clone();

        let count = tokio::task::spawn_blocking(move || -> Result<u64, String> {
            use std::io::Write;
            let lines = {
                let s = storage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                s.export_range(start, end).map_err(|e| e.to_string())?
            };

            let file = std::fs::File::create(&export_path).map_err(|e| e.to_string())?;
            let mut writer = std::io::BufWriter::new(file);
            let mut count = 0;

            for line in &lines {
                match format.as_str() {
                    "csv" => {
                        let escaped = line.payload.replace('"', "\"\"");
                        writeln!(writer, "{},{},\"{}\"", line.id, line.timestamp_ns, escaped)
                            .map_err(|e| e.to_string())?;
                    }
                    "jsonl" => {
                        let obj = serde_json::json!({
                            "id": line.id,
                            "timestamp_ns": line.timestamp_ns,
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
                        writeln!(writer, "{}", line.payload).map_err(|e| e.to_string())?;
                    }
                }
                count += 1;
            }
            writer.flush().map_err(|e| e.to_string())?;
            Ok(count)
        })
        .await
        .map_err(|e| e.to_string())??;

        let abs_path = std::fs::canonicalize(&path).unwrap_or(path);

        Ok(ResponsePayload::ExportSuccess {
            lines_exported: count,
            path: abs_path.display().to_string(),
        })
    }

    async fn handle_clear(&self, port: &str, archive: bool) -> Result<ResponsePayload, String> {
        let storage = self
            .port_manager
            .get_storage(port)
            .await
            .map_err(|e| e.to_string())?;

        let port_name = port.to_string();
        let (lines_cleared, archive_path) =
            tokio::task::spawn_blocking(move || -> Result<(u64, Option<String>), String> {
                let s = storage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let stats = s.get_stats().map_err(|e| e.to_string())?;

                let arch_path = if archive {
                    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
                    let sanitized = port_name.replace(['/', '\\'], "_");
                    Some(PathBuf::from(format!("./{sanitized}_{ts}.db")))
                } else {
                    None
                };

                s.clear(arch_path.as_deref()).map_err(|e| e.to_string())?;
                drop(s);

                Ok((
                    stats.total_lines,
                    arch_path.map(|p| p.display().to_string()),
                ))
            })
            .await
            .map_err(|e| e.to_string())??;

        Ok(ResponsePayload::ClearSuccess {
            lines_cleared,
            archive_path,
        })
    }

    async fn handle_get_stats(&self, port: &str) -> Result<ResponsePayload, String> {
        let storage = self
            .port_manager
            .get_storage(port)
            .await
            .map_err(|e| e.to_string())?;

        let stats = tokio::task::spawn_blocking(move || {
            let s = storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.get_stats().map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())??;

        Ok(ResponsePayload::Stats(stats))
    }

    #[cfg(feature = "esp")]
    async fn handle_esp_flash(
        &self,
        port: &str,
        firmware_path: &str,
        baud: Option<u32>,
    ) -> Result<ResponsePayload, String> {
        if !crate::esp::is_available() {
            return Err(
                "espflash not found in PATH. Install with: cargo install espflash".to_string(),
            );
        }

        let reopen = self.release_port_for_esp(port).await;
        let result = crate::esp::flash(port, firmware_path, baud, false).await;

        if let Some((config, data_dir)) = reopen {
            self.reopen_port_after_esp(port, config, data_dir).await;
        }

        let status = if result.is_ok() { "OK" } else { "FAILED" };
        self.log_to_buffer(port, &format!("━━━ FLASH {firmware_path} → {status} ━━━"))
            .await;

        result.map(ResponsePayload::EspSuccess)
    }

    #[cfg(feature = "esp")]
    async fn handle_esp_info(&self, port: &str) -> Result<ResponsePayload, String> {
        if !crate::esp::is_available() {
            return Err(
                "espflash not found in PATH. Install with: cargo install espflash".to_string(),
            );
        }

        let reopen = self.release_port_for_esp(port).await;
        let result = crate::esp::board_info(port).await;

        if let Some((config, data_dir)) = reopen {
            self.reopen_port_after_esp(port, config, data_dir).await;
        }

        result.map(ResponsePayload::EspSuccess)
    }

    #[cfg(feature = "esp")]
    async fn handle_esp_erase(&self, port: &str) -> Result<ResponsePayload, String> {
        if !crate::esp::is_available() {
            return Err(
                "espflash not found in PATH. Install with: cargo install espflash".to_string(),
            );
        }

        let reopen = self.release_port_for_esp(port).await;
        let result = crate::esp::erase_flash(port).await;

        if let Some((config, data_dir)) = reopen {
            self.reopen_port_after_esp(port, config, data_dir).await;
        }

        result.map(ResponsePayload::EspSuccess)
    }

    #[cfg(feature = "esp")]
    async fn release_port_for_esp(&self, port_name: &str) -> Option<(PortConfig, PathBuf)> {
        if self.port_manager.get_state(port_name).await.is_err() {
            return None;
        }
        let data_dir = self.data_dir.clone();
        self.port_manager
            .close_with_config(port_name)
            .await
            .ok()
            .map(|config| {
                tracing::info!(port = %port_name, "released port for espflash");
                (config, data_dir)
            })
    }

    #[cfg(feature = "esp")]
    async fn reopen_port_after_esp(&self, port_name: &str, config: PortConfig, data_dir: PathBuf) {
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

    /// Insert an informational message or event marker into a port's buffer.
    pub async fn log_to_buffer(&self, port_name: &str, message: &str) {
        if let Ok(storage) = self.port_manager.get_storage(port_name).await {
            let msg = message.to_string();
            tokio::task::spawn_blocking(move || {
                let s = storage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
                s.insert_lines(&[(now, &msg)]).ok();
            })
            .await
            .ok();
        }
    }
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if clean.len() % 2 != 0 {
        return Err("hex string must have an even number of digits".to_string());
    }
    (0..clean.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&clean[i..i + 2], 16)
                .map_err(|e| format!("invalid hex at index {i}: {e}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::SqliteStorage;
    use crate::testutil::mock_serial::mock_serial;

    fn make_test_engine() -> (CommandEngine, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pm = PortManagerHandle::new();
        let state_db = Arc::new(Mutex::new(StateDb::open_memory().unwrap()));
        let config = Arc::new(Config::default());
        let engine = CommandEngine::new(pm, state_db, config, dir.path().to_path_buf());
        (engine, dir)
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
        let (mock, handle) = mock_serial(1024);
        let storage = Arc::new(Mutex::new(SqliteStorage::open_memory().unwrap()));

        engine
            .port_manager
            .open(
                "mock1".into(),
                Box::new(mock),
                PortConfig::default(),
                storage,
            )
            .await
            .unwrap();

        let list = engine.execute(RequestPayload::ListPorts).await.unwrap();
        if let ResponsePayload::PortList(ports) = list {
            assert_eq!(ports.len(), 1);
            assert_eq!(ports[0].name, "mock1");
        } else {
            panic!("expected PortList");
        }

        handle.feed_lines(&["line 1", "line 2", "line 3"]).await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let read_resp = engine
            .execute(RequestPayload::ReadLines {
                port: "mock1".into(),
                start_id: Some(1),
                count: Some(10),
                tail: None,
            })
            .await
            .unwrap();

        if let ResponsePayload::Lines(lines) = read_resp {
            assert_eq!(lines.len(), 3);
            assert_eq!(lines[0].payload, "line 1");
        } else {
            panic!("expected Lines");
        }

        let search_resp = engine
            .execute(RequestPayload::Search {
                port: "mock1".into(),
                query: "line 2".into(),
                is_regex: false,
                start_ns: None,
                end_ns: None,
                limit: Some(10),
            })
            .await
            .unwrap();

        if let ResponsePayload::SearchResults(results) = search_resp {
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].payload, "line 2");
        } else {
            panic!("expected SearchResults");
        }

        let stats_resp = engine
            .execute(RequestPayload::GetStats {
                port: "mock1".into(),
            })
            .await
            .unwrap();

        if let ResponsePayload::Stats(stats) = stats_resp {
            assert_eq!(stats.total_lines, 3);
        } else {
            panic!("expected Stats");
        }

        let close_resp = engine
            .execute(RequestPayload::ClosePort {
                name: "mock1".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            close_resp,
            ResponsePayload::PortClosed {
                name: "mock1".into()
            }
        );
    }
}
