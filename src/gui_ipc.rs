// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Typed messages between GUI processes and their parent.
//!
//! Two channels use this vocabulary. The GUI multiplexer socket carries
//! [`GuiCommand`] so a second `devserial monitor` invocation opens a window in
//! the running instance, and the monitor subprocess reports user actions to its
//! parent as [`MonitorEvent`]. Both are newline-delimited JSON; the ad-hoc
//! `__macro:` style text protocol they replace could not report parse errors
//! and silently defaulted malformed settings.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::protocol::PortSettings;

/// Request payload to open a port in the GUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenPortRequest {
    pub port_name: String,
    pub db_path: PathBuf,
    pub port_info: String,
    /// Line settings for the receiving instance to open the port with.
    ///
    /// `None` means the sender owns the port and the receiving window should
    /// only display the capture database. That is the case for the monitor
    /// subprocess started by the MCP server.
    #[serde(default)]
    pub settings: Option<PortSettings>,
}

/// Commands accepted by the running GUI instance over its local socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GuiCommand {
    /// Liveness check, answered with the list of open ports.
    Ping,
    /// Open a window for a port.
    OpenPort(OpenPortRequest),
    /// Bring an existing window to the front.
    FocusPort { port_name: String },
    /// Close a window.
    ClosePort { port_name: String },
}

/// Responses sent by the GUI instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GuiResponse {
    /// Alive, with the ports currently open in the instance.
    Pong { open_ports: Vec<String> },
    /// Command accepted.
    Ok,
    /// Command refused.
    Error(String),
}

/// A user action in a monitor window that its parent process must carry out.
///
/// The monitor subprocess has no serial handle of its own; the parent owns the
/// port and executes these through the command engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MonitorEvent {
    /// Run a macro by name.
    Macro { name: String },
    /// Set control lines.
    Signal {
        dtr: Option<bool>,
        rts: Option<bool>,
    },
    /// Write raw bytes, hex encoded.
    Write { hex: String },
    /// Change the line settings of the port.
    Reconfigure { settings: PortSettings },
    /// Send an RS-232 BREAK pulse.
    Break { duration_ms: Option<u64> },
    /// Send a file with a modem protocol.
    SendFile {
        path: String,
        protocol: crate::modem::FileTransferProtocol,
    },
    /// Receive a file with a modem protocol.
    ReceiveFile {
        dir: String,
        protocol: crate::modem::FileTransferProtocol,
    },
}

impl MonitorEvent {
    /// Encode as a single line, ready to write to a pipe.
    ///
    /// # Errors
    /// Returns an error if serialization fails.
    pub fn encode(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Decode a line produced by [`Self::encode`].
    ///
    /// # Errors
    /// Returns an error if the line is not a valid event.
    pub fn decode(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }
}

/// Try to send an `OpenPort` request to an already-running GUI instance.
///
/// Returns `Ok(true)` if the request was acknowledged, `Ok(false)` if no GUI
/// instance is listening.
///
/// # Errors
/// Returns error if communication fails after connection.
pub fn try_open_in_existing_gui(req: &OpenPortRequest) -> Result<bool, String> {
    let socket = crate::paths::default_gui_socket_path();

    #[cfg(unix)]
    {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;

        if !socket.exists() {
            return Ok(false);
        }

        let Ok(mut stream) = UnixStream::connect(&socket) else {
            // The file exists but nothing listens: a leftover from a crash.
            let _ = std::fs::remove_file(&socket);
            return Ok(false);
        };

        let timeout = std::time::Duration::from_millis(500);
        stream.set_read_timeout(Some(timeout)).map_err(err_text)?;
        stream.set_write_timeout(Some(timeout)).map_err(err_text)?;

        let message =
            serde_json::to_string(&GuiCommand::OpenPort(req.clone())).map_err(err_text)?;
        writeln!(stream, "{message}").map_err(err_text)?;
        stream.flush().map_err(err_text)?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line).map_err(err_text)? > 0 {
            match serde_json::from_str::<GuiResponse>(&line) {
                Ok(GuiResponse::Ok | GuiResponse::Pong { .. }) => return Ok(true),
                Ok(GuiResponse::Error(e)) => return Err(e),
                Err(e) => return Err(format!("malformed GUI response: {e}")),
            }
        }
        Ok(false)
    }

    #[cfg(not(unix))]
    {
        // Windows GUI instances do not multiplex yet; each invocation opens its
        // own window, which is the documented behaviour there. Both parameters
        // are consumed only by the Unix branch.
        let _ = (socket, req);
        Ok(false)
    }
}

// Only the Unix branch above maps errors to strings; off Unix the function
// would be dead code and -D warnings turns that into a failure.
#[cfg(unix)]
fn err_text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Server handle that cleans up the socket on drop.
pub struct GuiSocketServer {
    socket: PathBuf,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for GuiSocketServer {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Shared view of the ports a GUI instance currently shows.
pub type OpenPortsView = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// Start the GUI socket server thread.
///
/// # Errors
/// Returns error if binding the socket fails.
#[cfg(unix)]
pub fn start_gui_socket_server(
    tx: std::sync::mpsc::Sender<GuiCommand>,
    ctx_holder: std::sync::Arc<std::sync::Mutex<Option<eframe::egui::Context>>>,
    open_ports: OpenPortsView,
) -> std::io::Result<GuiSocketServer> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    let socket = crate::paths::default_gui_socket_path();
    if let Some(parent) = socket.parent() {
        crate::paths::create_private_dir(parent)?;
    }
    if socket.exists() {
        let _ = std::fs::remove_file(&socket);
    }

    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;

    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_thread = std::sync::Arc::clone(&shutdown);

    std::thread::spawn(move || {
        while !shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    // Accepted sockets inherit the listener's non-blocking flag
                    // on some platforms, which would make the read below fail
                    // spuriously and drop the command.
                    if stream.set_nonblocking(false).is_err() {
                        continue;
                    }
                    let Ok(read_side) = stream.try_clone() else {
                        continue;
                    };
                    let mut stream = stream;
                    let mut reader = BufReader::new(read_side);
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        continue;
                    }

                    let response = match serde_json::from_str::<GuiCommand>(&line) {
                        Ok(GuiCommand::Ping) => GuiResponse::Pong {
                            open_ports: open_ports
                                .lock()
                                .map(|ports| ports.clone())
                                .unwrap_or_default(),
                        },
                        Ok(command) => {
                            if tx.send(command).is_ok() {
                                if let Some(ctx) = ctx_holder.lock().ok().and_then(|g| g.clone()) {
                                    ctx.request_repaint();
                                }
                                GuiResponse::Ok
                            } else {
                                GuiResponse::Error("GUI is shutting down".to_string())
                            }
                        }
                        Err(e) => GuiResponse::Error(format!("malformed command: {e}")),
                    };

                    if let Ok(text) = serde_json::to_string(&response) {
                        let _ = writeln!(stream, "{text}");
                        let _ = stream.flush();
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
    });

    Ok(GuiSocketServer { socket, shutdown })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_events_round_trip() {
        let events = [
            MonitorEvent::Macro {
                name: "reset".into(),
            },
            MonitorEvent::Signal {
                dtr: Some(true),
                rts: None,
            },
            MonitorEvent::Write { hex: "0d0a".into() },
            MonitorEvent::Break {
                duration_ms: Some(120),
            },
            MonitorEvent::Reconfigure {
                settings: PortSettings {
                    baudrate: Some(9600),
                    ..PortSettings::default()
                },
            },
        ];

        for event in events {
            let line = event.encode().unwrap();
            assert!(!line.contains('\n'), "events must fit on a single line");
            assert_eq!(MonitorEvent::decode(&line).unwrap(), event);
        }
    }

    #[test]
    fn malformed_event_is_an_error_not_a_default() {
        assert!(MonitorEvent::decode("__macro:reset").is_err());
        assert!(MonitorEvent::decode("{\"Reconfigure\":{}}").is_err());
    }
}
