// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Command line handlers.
//!
//! Every handler builds a [`RequestPayload`], sends it through one shared
//! session and renders the response. The previous version repeated the runtime
//! setup, the daemon handshake and an `eprintln!` plus `process::exit` block in
//! each of sixteen functions.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use crate::cli::output;
use crate::cli::{CliError, Command};
use crate::ipc::IpcClient;
use crate::paths;
use crate::protocol::{
    LinesPage, ReadWindow, RequestPayload, ResponsePayload, SearchMode, SearchOutcome,
};

/// Polling interval while following a port.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(50);
/// Lines requested per poll while following.
const FOLLOW_BATCH: u32 = 500;

/// A CLI session: one runtime, one client.
pub struct Session {
    client: IpcClient,
    runtime: tokio::runtime::Runtime,
}

impl Session {
    /// Create a session for the given endpoint.
    ///
    /// # Errors
    /// Returns an error if the Tokio runtime cannot be created.
    pub fn new(socket: Option<PathBuf>, config: Option<PathBuf>) -> Result<Self, CliError> {
        let endpoint = socket.unwrap_or_else(paths::default_socket_path);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            client: IpcClient::new(endpoint).with_config_path(config),
            runtime,
        })
    }

    /// The endpoint this session talks to.
    #[must_use]
    pub fn endpoint(&self) -> &std::path::Path {
        self.client.endpoint()
    }

    /// Send a request, starting the daemon first if it is not running.
    ///
    /// # Errors
    /// Returns an error if the daemon cannot be reached or reports a failure.
    pub fn request(&self, payload: RequestPayload) -> Result<ResponsePayload, CliError> {
        self.runtime.block_on(async {
            self.client.ensure_daemon().await?;
            Ok(self.client.send(payload).await?)
        })
    }

    /// Send a request without starting a daemon.
    ///
    /// # Errors
    /// Returns an error if the daemon is not reachable.
    pub fn request_existing(&self, payload: RequestPayload) -> Result<ResponsePayload, CliError> {
        self.runtime
            .block_on(async { Ok(self.client.send(payload).await?) })
    }

    fn is_alive(&self) -> bool {
        self.runtime.block_on(self.client.is_alive())
    }
}

/// Response of an unexpected shape, which would be an engine bug.
fn unexpected(payload: &ResponsePayload) -> CliError {
    CliError::msg(format!("unexpected response from daemon: {payload:?}"))
}

/// Route a remote command to its handler.
///
/// # Errors
/// Returns the handler's error.
#[allow(clippy::too_many_lines)]
pub fn run_remote(session: &Session, command: Command) -> Result<(), CliError> {
    match command {
        Command::List { json } => list(session, json),

        Command::Open { port, line } => {
            let response = session.request(RequestPayload::OpenPort {
                name: port,
                settings: line.to_settings()?,
            })?;
            match response {
                ResponsePayload::PortOpened {
                    name,
                    config_summary,
                } => {
                    println!("Opened {name} ({config_summary})");
                    Ok(())
                }
                ResponsePayload::PortReconfigured {
                    name,
                    config_summary,
                } => {
                    println!("Reconfigured {name} ({config_summary})");
                    Ok(())
                }
                other => Err(unexpected(&other)),
            }
        }

        Command::Close { port } => {
            let response = session.request(RequestPayload::ClosePort { name: port })?;
            let ResponsePayload::PortClosed { name } = response else {
                return Err(unexpected(&response));
            };
            println!("Closed {name}");
            Ok(())
        }

        Command::Read {
            port,
            tail,
            from,
            after,
            since,
            limit,
            wait_ms,
            follow,
            timestamps,
            json,
        } => {
            let window = ReadWindow {
                start_id: from,
                after_id: after,
                tail,
                since_ns: parse_time(since.as_deref())?,
                limit: Some(limit),
                wait_ms: Some(wait_ms),
            };
            if follow {
                follow_port(session, &port, window, timestamps, json)
            } else {
                let page = read_page(session, &port, window)?;
                for line in &page.lines {
                    output::print_line(line, timestamps, json);
                }
                Ok(())
            }
        }

        Command::Write {
            port,
            data,
            hex,
            newline,
        } => write(session, &port, data, hex, newline),

        Command::Break { port, duration_ms } => {
            let response = session.request(RequestPayload::SendBreak {
                port: port.clone(),
                duration_ms: Some(duration_ms),
            })?;
            let ResponsePayload::BreakSuccess { duration_ms } = response else {
                return Err(unexpected(&response));
            };
            println!("Sent serial BREAK pulse ({duration_ms}ms) on {port}");
            Ok(())
        }

        Command::Send {
            port,
            file,
            protocol,
        } => {
            println!("Sending '{file}' to {port} using {protocol:?}...");
            let response = session.request(RequestPayload::SendFile {
                port,
                file_path: file,
                protocol,
            })?;
            let ResponsePayload::TransferSuccess {
                bytes_transferred,
                file_name,
                protocol,
                ..
            } = response
            else {
                return Err(unexpected(&response));
            };
            println!("Sent {bytes_transferred} bytes ({file_name}) via {protocol:?}");
            Ok(())
        }

        Command::Recv {
            port,
            output: out_dir,
            protocol,
        } => {
            println!(
                "Waiting to receive a file from {port} into '{out_dir}' using {protocol:?}..."
            );
            let response = session.request(RequestPayload::ReceiveFile {
                port,
                output_dir: out_dir,
                protocol,
            })?;
            let ResponsePayload::TransferSuccess {
                bytes_transferred,
                file_name,
                path,
                ..
            } = response
            else {
                return Err(unexpected(&response));
            };
            let location = path.unwrap_or(file_name);
            println!("Received {bytes_transferred} bytes into '{location}'");
            Ok(())
        }

        Command::Signal { port, dtr, rts } => {
            let response = session.request(RequestPayload::SetSignal {
                port: port.clone(),
                dtr,
                rts,
            })?;
            let ResponsePayload::SignalSuccess { applied } = response else {
                return Err(unexpected(&response));
            };
            println!("Set {} on {port}", applied.join(", "));
            Ok(())
        }

        Command::Macro { port, name } => {
            let response = session.request(RequestPayload::ExecuteMacro {
                port: port.clone(),
                macro_name: name.clone(),
            })?;
            let ResponsePayload::MacroSuccess { executed_steps } = response else {
                return Err(unexpected(&response));
            };
            println!(
                "Executed macro '{name}' on {port} ({} steps: {})",
                executed_steps.len(),
                executed_steps.join(" → ")
            );
            Ok(())
        }

        Command::Search {
            port,
            query,
            mode,
            start,
            end,
            limit,
            json,
        } => search(
            session,
            &port,
            &query,
            mode,
            parse_time(start.as_deref())?,
            parse_time(end.as_deref())?,
            limit,
            json,
        ),

        Command::Export {
            port,
            output: out_path,
            format,
            start,
            end,
        } => {
            let response = session.request(RequestPayload::Export {
                port,
                output_path: out_path,
                file_format: Some(format),
                start_line: start,
                end_line: end,
            })?;
            let ResponsePayload::ExportSuccess {
                lines_exported,
                path,
                file_format,
            } = response
            else {
                return Err(unexpected(&response));
            };
            println!("Exported {lines_exported} lines to {path} (format: {file_format})");
            Ok(())
        }

        Command::Clear { port, archive } => {
            let response = session.request(RequestPayload::Clear {
                port: port.clone(),
                archive_current: Some(archive),
            })?;
            let ResponsePayload::ClearSuccess {
                lines_cleared,
                archive_path,
            } = response
            else {
                return Err(unexpected(&response));
            };
            match archive_path {
                Some(path) => {
                    println!("Cleared buffer on {port} ({lines_cleared} lines archived to {path})");
                }
                None => println!("Cleared buffer on {port} ({lines_cleared} lines removed)"),
            }
            Ok(())
        }

        Command::Stats { port, json } => {
            let response = session.request(RequestPayload::GetStatus { port })?;
            let ResponsePayload::Status { name, state, stats } = response else {
                return Err(unexpected(&response));
            };
            output::print_status(&name, &state, &stats, json)
        }

        #[cfg(feature = "esp")]
        Command::Flash {
            port,
            firmware,
            baud,
            monitor,
        } => flash(session, &port, &firmware, baud, monitor),

        // Handled before reaching this router.
        Command::Mcp | Command::Daemon { .. } | Command::About => {
            Err(CliError::msg("command is not a remote operation"))
        }

        #[cfg(feature = "monitor")]
        Command::Gui | Command::Monitor { .. } | Command::MonitorSubprocess { .. } => {
            Err(CliError::msg("command is not a remote operation"))
        }

        #[cfg(feature = "tui")]
        Command::Tui { .. } => Err(CliError::msg("command is not a remote operation")),
    }
}

/// Handle `daemon --status` and `daemon --stop`.
///
/// # Errors
/// Returns an error if the daemon cannot be reached while stopping it.
pub fn daemon_control(session: &Session, status: bool, stop: bool) -> Result<(), CliError> {
    if status {
        let state = if session.is_alive() {
            "RUNNING"
        } else {
            "STOPPED"
        };
        println!(
            "Daemon is {state} (endpoint: {})",
            session.endpoint().display()
        );
        return Ok(());
    }

    if stop {
        match session.request_existing(RequestPayload::Shutdown) {
            Ok(_) => println!("Daemon shutdown requested."),
            Err(CliError::Ipc(crate::ipc::IpcError::Connect { .. })) => {
                println!("Daemon is not running.");
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn list(session: &Session, json: bool) -> Result<(), CliError> {
    // Hardware enumeration must work even without a daemon.
    let hardware = crate::engine::hardware_ports();
    let managed = match session.request_existing(RequestPayload::ListPorts) {
        Ok(ResponsePayload::PortList(ports)) => Some(ports),
        Ok(other) => return Err(unexpected(&other)),
        Err(_) => None,
    };
    output::print_port_list(managed.as_deref(), &hardware, json)
}

fn read_page(session: &Session, port: &str, window: ReadWindow) -> Result<LinesPage, CliError> {
    let response = session.request(RequestPayload::ReadLines {
        port: port.to_string(),
        window,
    })?;
    match response {
        ResponsePayload::Lines(page) => Ok(page),
        other => Err(unexpected(&other)),
    }
}

/// Stream new lines until interrupted.
fn follow_port(
    session: &Session,
    port: &str,
    window: ReadWindow,
    timestamps: bool,
    json: bool,
) -> Result<(), CliError> {
    // Seed from the requested window, then continue by id.
    let initial = ReadWindow {
        tail: window.tail.or(Some(20)),
        limit: window.limit,
        ..ReadWindow::default()
    };
    let mut page = read_page(session, port, initial)?;
    for line in &page.lines {
        output::print_line(line, timestamps, json);
    }
    let mut last_id = page.next_after_id;

    session.runtime.block_on(async {
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    return Ok(());
                }
                () = tokio::time::sleep(FOLLOW_INTERVAL) => {
                    let response = session
                        .client
                        .send(RequestPayload::ReadLines {
                            port: port.to_string(),
                            window: ReadWindow {
                                after_id: Some(last_id),
                                limit: Some(FOLLOW_BATCH),
                                ..ReadWindow::default()
                            },
                        })
                        .await?;
                    let ResponsePayload::Lines(next) = response else {
                        return Err(unexpected(&response));
                    };
                    for line in &next.lines {
                        output::print_line(line, timestamps, json);
                    }
                    if !next.lines.is_empty() {
                        last_id = next.next_after_id;
                    }
                    page = next;
                }
            }
        }
    })?;
    let _ = page;
    Ok(())
}

fn write(
    session: &Session,
    port: &str,
    data: Option<String>,
    hex: bool,
    newline: bool,
) -> Result<(), CliError> {
    let mut payload = match data {
        Some(value) if value == "-" => read_stdin()?,
        Some(value) => value,
        None => {
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                return Err(CliError::msg(
                    "no data to write: pass it as an argument or pipe it via stdin",
                ));
            }
            read_stdin()?
        }
    };

    if newline {
        if hex {
            return Err(CliError::msg(
                "--newline cannot be combined with --hex; include 0D0A in the hex data instead",
            ));
        }
        payload.push_str("\r\n");
    }

    let response = session.request(RequestPayload::WriteData {
        port: port.to_string(),
        data: payload,
        is_hex: hex,
    })?;
    let ResponsePayload::WriteSuccess { bytes_written } = response else {
        return Err(unexpected(&response));
    };
    println!("Wrote {bytes_written} bytes to {port}");
    Ok(())
}

fn read_stdin() -> Result<String, CliError> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

#[allow(clippy::too_many_arguments)]
fn search(
    session: &Session,
    port: &str,
    query: &str,
    mode: SearchMode,
    start_ns: Option<i64>,
    end_ns: Option<i64>,
    limit: u32,
    json: bool,
) -> Result<(), CliError> {
    let response = session.request(RequestPayload::Search {
        port: port.to_string(),
        query: query.to_string(),
        mode,
        start_ns,
        end_ns,
        limit: Some(limit),
    })?;
    let ResponsePayload::SearchResults(outcome) = response else {
        return Err(unexpected(&response));
    };
    print_search(&outcome, json)
}

fn print_search(outcome: &SearchOutcome, json: bool) -> Result<(), CliError> {
    if json {
        println!("{}", serde_json::to_string_pretty(outcome)?);
        return Ok(());
    }
    println!("Found {} matching lines:", outcome.results.len());
    for line in &outcome.results {
        println!("  [{}] {}", line.id, line.payload);
    }
    if outcome.truncated {
        println!("(scan limit reached; later lines were not examined)");
    }
    Ok(())
}

#[cfg(feature = "esp")]
fn flash(
    session: &Session,
    port: &str,
    firmware: &str,
    baud: Option<u32>,
    monitor: bool,
) -> Result<(), CliError> {
    println!("Flashing firmware {firmware} to {port}...");
    let response = session.request(RequestPayload::EspFlash {
        port: port.to_string(),
        firmware_path: firmware.to_string(),
        baud,
    })?;
    let ResponsePayload::EspSuccess(output) = response else {
        return Err(unexpected(&response));
    };
    print!("{output}");

    if monitor {
        println!("\n━━━ Following serial output (Ctrl+C to exit) ━━━\n");
        return follow_port(session, port, ReadWindow::default(), false, false);
    }
    Ok(())
}

fn parse_time(value: Option<&str>) -> Result<Option<i64>, CliError> {
    value
        .map(|text| {
            chrono::DateTime::parse_from_rfc3339(text)
                .map(|dt| dt.timestamp_nanos_opt().unwrap_or(0))
                .map_err(|e| CliError::msg(format!("invalid timestamp '{text}': {e}")))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_must_be_rfc3339() {
        assert!(parse_time(Some("2026-01-15T10:00:00Z")).unwrap().is_some());
        assert!(parse_time(None).unwrap().is_none());
        assert!(parse_time(Some("yesterday")).is_err());
    }

    #[test]
    fn newline_and_hex_are_mutually_exclusive() {
        let session = Session::new(
            Some(std::path::PathBuf::from("/nonexistent/devserial.sock")),
            None,
        )
        .unwrap();
        let err = write(&session, "/dev/x", Some("41".into()), true, true).unwrap_err();
        assert!(err.to_string().contains("cannot be combined"));
    }

    #[test]
    fn session_reports_its_endpoint() {
        let session = Session::new(Some(std::path::PathBuf::from("/tmp/x.sock")), None).unwrap();
        assert_eq!(session.endpoint(), std::path::Path::new("/tmp/x.sock"));
    }
}
