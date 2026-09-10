// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Attached mode: a window or a terminal on a port the daemon holds.
//!
//! A serial port can be opened once. While a monitor held its own port, that
//! port could not also be a daemon port, so the CLI, the MCP server and
//! anything else reaching the daemon could not see it. Both monitors therefore
//! ask the daemon to open the line, then read the capture it writes and send
//! every action back to it.
//!
//! What that buys, beyond one kind of port: the capture survives the window.
//! Closing a monitor used to stop the reader that filled the buffer; now the
//! daemon keeps reading, and reopening the monitor continues the same log.
//!
//! The surfaces do not know any of this. They take a writer, three closures
//! and a watch on the link state, and this module produces all of them from an
//! IPC endpoint instead of from an owned file descriptor.

#[cfg(any(feature = "monitor", feature = "tui"))]
use std::path::Path;
#[cfg(any(feature = "monitor", feature = "tui"))]
use std::sync::{Arc, Mutex};

#[cfg(any(feature = "monitor", feature = "tui"))]
use crate::cli::CliError;
#[cfg(any(feature = "monitor", feature = "tui"))]
use crate::config::PortConfig;
#[cfg(any(feature = "monitor", feature = "tui"))]
use crate::ipc::IpcClient;
#[cfg(any(feature = "monitor", feature = "tui"))]
use crate::protocol::{RequestPayload, ResponsePayload};
#[cfg(any(feature = "monitor", feature = "tui"))]
use crate::reader::ConnectionState;
#[cfg(any(feature = "monitor", feature = "tui"))]
use crate::storage::SqliteStorage;

/// How often the link state is read back from the daemon.
///
/// The reader lives in the daemon now, so a surface cannot watch it directly.
/// Half a second is below what a person notices on an indicator and far above
/// what the request costs on a local socket.
#[cfg(any(feature = "monitor", feature = "tui"))]
const LINK_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Resolve the data directory for an attached session.
///
/// Reads the same configuration the daemon reads, so both agree on where the
/// capture database for a port lives.
#[cfg(any(feature = "monitor", feature = "tui"))]
fn resolve_data_dir(config_path: Option<&Path>) -> std::path::PathBuf {
    match crate::config::load_config(config_path) {
        Ok(config) => config.global.data_dir,
        Err(e) => {
            tracing::warn!(error = %e, "could not read the configuration, using the default data directory");
            crate::paths::default_data_dir()
        }
    }
}

/// Data directory for windows opened from inside a running GUI.
#[cfg(feature = "monitor")]
pub(crate) fn gui_data_dir() -> std::path::PathBuf {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| resolve_data_dir(None)).clone()
}

/// Keeps the runtime and the link watch of an attached session alive.
#[cfg(any(feature = "monitor", feature = "tui"))]
pub struct SessionKeepalive {
    runtime: Mutex<Option<tokio::runtime::Runtime>>,
    link: tokio::sync::watch::Receiver<ConnectionState>,
    poller: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[cfg(any(feature = "monitor", feature = "tui"))]
impl SessionKeepalive {
    fn new(
        runtime: tokio::runtime::Runtime,
        link: tokio::sync::watch::Receiver<ConnectionState>,
        poller: tokio::task::JoinHandle<()>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime: Mutex::new(Some(runtime)),
            link,
            poller: Mutex::new(Some(poller)),
        })
    }

    /// What the daemon last reported about the hardware.
    ///
    /// The surfaces show whether the device is there, which is not the same
    /// question as whether the user pressed Disconnect. A device that is
    /// unplugged leaves that flag untouched.
    pub fn connection_state(&self) -> Option<ConnectionState> {
        Some(self.link.borrow().clone())
    }

    /// Watch on the link state, for a surface that shows it.
    pub fn link(&self) -> Option<tokio::sync::watch::Receiver<ConnectionState>> {
        Some(self.link.clone())
    }

    /// Runtime handle for blocking calls from the interface thread.
    ///
    /// The monitor window also uses it to run espflash while the port is
    /// released, so it is visible outside this module.
    pub fn handle(&self) -> Option<tokio::runtime::Handle> {
        let guard = self
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = guard.as_ref().map(|rt| rt.handle().clone());
        drop(guard);
        handle
    }
}

#[cfg(any(feature = "monitor", feature = "tui"))]
impl Drop for SessionKeepalive {
    fn drop(&mut self) {
        // The port stays open on the daemon on purpose. Only this process's
        // view of it ends here.
        let poller = self
            .poller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(poller) = poller {
            poller.abort();
        }
        let taken = {
            let mut guard = self
                .runtime
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.take()
        };
        if let Some(runtime) = taken {
            // Shutting down in the background keeps the interface responsive.
            runtime.shutdown_background();
        }
    }
}

/// Blocking writer that forwards input to the port the daemon holds.
#[cfg(any(feature = "monitor", feature = "tui"))]
#[derive(Clone)]
pub struct DaemonWriter {
    port: String,
    client: Arc<IpcClient>,
    runtime: tokio::runtime::Handle,
}

#[cfg(any(feature = "monitor", feature = "tui"))]
impl std::io::Write for DaemonWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // The wire carries a string and a flag. Text goes as text so the
        // daemon's log of it stays readable; anything that is not UTF-8 goes as
        // hex rather than being replaced or refused.
        let (data, is_hex) = std::str::from_utf8(buf).map_or_else(
            |_| (crate::hex::encode(buf), true),
            |text| (text.to_string(), false),
        );
        let payload = RequestPayload::WriteData {
            port: self.port.clone(),
            data,
            is_hex,
        };
        let client = Arc::clone(&self.client);
        let written = buf.len();
        self.runtime.block_on(async move {
            match client.send(payload).await {
                Ok(ResponsePayload::WriteSuccess { .. }) => Ok(written),
                Ok(_) => Err(std::io::Error::other("unexpected answer from the daemon")),
                Err(e) => Err(std::io::Error::other(e.to_string())),
            }
        })
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // The daemon writes through before it answers, so there is nothing
        // buffered on this side to push.
        Ok(())
    }
}

/// Everything one attached session owns.
#[cfg(any(feature = "monitor", feature = "tui"))]
struct SessionParts {
    /// The window hands this to its monitor state; the terminal reads through
    /// `shared_storage` instead and never needs it.
    #[cfg(feature = "monitor")]
    storage: SqliteStorage,
    shared_storage: Arc<Mutex<SqliteStorage>>,
    client: Arc<IpcClient>,
    keepalive: Arc<SessionKeepalive>,
    history: Vec<String>,
}

/// Changes the line settings of an open port.
///
/// Shared by both surfaces, so it lives here rather than with the window; the
/// terminal reconfigured its own port handle and had no such type.
#[cfg(any(feature = "monitor", feature = "tui"))]
pub type ReconfigureFn = Arc<dyn Fn(&PortConfig) -> Result<(), String> + Send + Sync>;

/// Acts on the port the daemon holds.
#[cfg(any(feature = "monitor", feature = "tui"))]
pub type DirectActionFn = Arc<dyn Fn(&PortAction) -> Result<(), String> + Send + Sync>;

/// Releases the port or takes it back, with the settings to reopen it under.
///
/// Defined here rather than with the window: the closure is built from the
/// session, and the terminal offers the same choice. Leaving it in `monitor.rs`
/// meant `--features tui` alone no longer compiled.
#[cfg(any(feature = "monitor", feature = "tui"))]
pub type ToggleConnectFn = Arc<dyn Fn(bool, &PortConfig) -> Result<(), String> + Send + Sync>;

/// Sends or receives a file over a modem protocol.
///
/// The daemon reads and writes the file itself, so a surface passes a path
/// rather than bytes. The terminal used to read the file, run the protocol on
/// its own port handle and write the result; with the port on the daemon there
/// is no handle to run a protocol on, and the daemon already has the code.
#[cfg(feature = "tui")]
pub type TransferFn = Arc<
    dyn Fn(bool, &str, crate::modem::FileTransferProtocol) -> Result<String, String> + Send + Sync,
>;

/// What a surface asks of the HTTP interface.
#[cfg(all(feature = "rest", any(feature = "monitor", feature = "tui")))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestRequest {
    /// Report the state without changing it.
    Status,
    /// Start listening, optionally somewhere other than configured.
    Enable {
        bind: Option<String>,
        port: Option<u16>,
    },
    /// Stop listening.
    Disable,
}

/// Shows and changes the HTTP interface through the daemon.
#[cfg(all(feature = "rest", any(feature = "monitor", feature = "tui")))]
pub type RestControlFn =
    Arc<dyn Fn(&RestRequest) -> Result<crate::protocol::RestState, String> + Send + Sync>;

/// Something a surface asks the hardware to do.
///
/// Its own vocabulary rather than the window's wire type: `gui_ipc` exists to
/// talk to a parent process and is compiled only with the window, while this
/// path serves the terminal as well.
#[cfg(any(feature = "monitor", feature = "tui"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortAction {
    /// Hold the line low for a while.
    Break { duration_ms: Option<u64> },
    /// Set one or both control lines.
    Signal {
        dtr: Option<bool>,
        rts: Option<bool>,
    },
    /// Run a configured macro by name.
    Macro { name: String },
}

#[cfg(any(feature = "monitor", feature = "tui"))]
impl PortAction {
    /// The request that carries out this action.
    fn payload(&self, port: &str) -> RequestPayload {
        match self {
            Self::Break { duration_ms } => RequestPayload::SendBreak {
                port: port.to_string(),
                duration_ms: *duration_ms,
            },
            Self::Signal { dtr, rts } => RequestPayload::SetSignal {
                port: port.to_string(),
                dtr: *dtr,
                rts: *rts,
            },
            Self::Macro { name } => RequestPayload::ExecuteMacro {
                port: port.to_string(),
                macro_name: name.clone(),
            },
        }
    }
}

/// Send one request and reduce the answer to success or a readable reason.
#[cfg(any(feature = "monitor", feature = "tui"))]
fn ask(
    runtime: &tokio::runtime::Handle,
    client: &Arc<IpcClient>,
    payload: RequestPayload,
) -> Result<ResponsePayload, String> {
    let client = Arc::clone(client);
    runtime.block_on(async move { client.send(payload).await.map_err(|e| e.to_string()) })
}

/// Build the closure that carries out hardware actions.
///
/// The macro table is not read here. The daemon resolves a macro name against
/// the configuration it loaded, which is what makes a macro behave the same
/// whether it was started from a window, a terminal or the command line.
#[cfg(any(feature = "monitor", feature = "tui"))]
fn daemon_action(
    port: &str,
    client: &Arc<IpcClient>,
    runtime: tokio::runtime::Handle,
) -> DirectActionFn {
    let port = port.to_string();
    let client = Arc::clone(client);
    Arc::new(move |action| ask(&runtime, &client, action.payload(&port)).map(|_| ()))
}

/// Release the port or take it back.
#[cfg(any(feature = "monitor", feature = "tui"))]
fn daemon_toggle(
    port: &str,
    client: &Arc<IpcClient>,
    runtime: tokio::runtime::Handle,
) -> ToggleConnectFn {
    let port = port.to_string();
    let client = Arc::clone(client);
    Arc::new(move |connect: bool, config: &PortConfig| {
        let payload = if connect {
            RequestPayload::OpenPort {
                name: port.clone(),
                settings: config_settings(config),
            }
        } else {
            RequestPayload::ClosePort { name: port.clone() }
        };
        ask(&runtime, &client, payload).map(|_| ())
    })
}

/// Show and change the HTTP interface through the daemon.
///
/// The state is the daemon's, read on every call rather than cached, because
/// another surface or the command line can change it while a window is open.
#[cfg(all(feature = "rest", any(feature = "monitor", feature = "tui")))]
fn daemon_rest(client: &Arc<IpcClient>, runtime: tokio::runtime::Handle) -> RestControlFn {
    let client = Arc::clone(client);
    Arc::new(move |request: &RestRequest| {
        let payload = match request {
            RestRequest::Status => RequestPayload::RestStatus,
            RestRequest::Enable { bind, port } => RequestPayload::RestEnable {
                bind: bind.clone(),
                port: *port,
                token: None,
            },
            RestRequest::Disable => RequestPayload::RestDisable,
        };
        match ask(&runtime, &client, payload)? {
            ResponsePayload::RestState(state) => Ok(state),
            _ => Err("unexpected answer from the daemon".to_string()),
        }
    })
}

/// Send or receive a file through the daemon.
#[cfg(feature = "tui")]
fn daemon_transfer(
    port: &str,
    client: &Arc<IpcClient>,
    runtime: tokio::runtime::Handle,
) -> TransferFn {
    let port = port.to_string();
    let client = Arc::clone(client);
    Arc::new(
        move |send: bool, path: &str, protocol: crate::modem::FileTransferProtocol| {
            let payload = if send {
                RequestPayload::SendFile {
                    port: port.clone(),
                    file_path: path.to_string(),
                    protocol,
                }
            } else {
                RequestPayload::ReceiveFile {
                    port: port.clone(),
                    output_dir: path.to_string(),
                    protocol,
                }
            };
            match ask(&runtime, &client, payload)? {
                ResponsePayload::TransferSuccess {
                    bytes_transferred,
                    file_name,
                    path,
                    ..
                } => Ok(path.map_or_else(
                    || format!("{bytes_transferred} bytes, {file_name}"),
                    |path| format!("{bytes_transferred} bytes, {path}"),
                )),
                _ => Err("unexpected answer from the daemon".to_string()),
            }
        },
    )
}

/// Change the line settings of the port the daemon holds.
#[cfg(any(feature = "monitor", feature = "tui"))]
fn daemon_reconfigure(
    port: &str,
    client: &Arc<IpcClient>,
    runtime: tokio::runtime::Handle,
) -> ReconfigureFn {
    let port = port.to_string();
    let client = Arc::clone(client);
    Arc::new(move |config: &PortConfig| {
        ask(
            &runtime,
            &client,
            RequestPayload::ReconfigurePort {
                name: port.clone(),
                settings: config_settings(config),
            },
        )
        .map(|_| ())
    })
}

/// Macro names the configuration offers, for a surface that lists them.
///
/// Only the terminal numbers them; the window shows Reset and Bootloader as
/// named buttons instead.
#[cfg(feature = "tui")]
fn macro_names(config_path: Option<&Path>) -> Vec<String> {
    crate::config::load_config(config_path)
        .unwrap_or_default()
        .available_macros()
}

/// Publish what the daemon reports about the link into a watch.
///
/// A surface that shows the link state watches a channel. With the reader in
/// the daemon there is nothing local to watch, so this task asks and publishes.
/// A failed request is not a disconnected device: the daemon may be busy or
/// gone, and reporting the last known state is closer to the truth than
/// claiming the cable was pulled.
#[cfg(any(feature = "monitor", feature = "tui"))]
fn spawn_link_poller(
    port: &str,
    client: &Arc<IpcClient>,
    runtime: &tokio::runtime::Handle,
    initial: ConnectionState,
) -> (
    tokio::sync::watch::Receiver<ConnectionState>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = tokio::sync::watch::channel(initial);
    let port = port.to_string();
    let client = Arc::clone(client);
    let poller = runtime.spawn(async move {
        let mut ticker = tokio::time::interval(LINK_POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let payload = RequestPayload::GetStatus { port: port.clone() };
            if let Ok(ResponsePayload::Status { state, .. }) = client.send(payload).await {
                tx.send_if_modified(|current| {
                    if *current == state {
                        false
                    } else {
                        *current = state;
                        true
                    }
                });
            }
            if tx.is_closed() {
                return;
            }
        }
    });
    (rx, poller)
}

/// Ask the daemon to hold the port, then assemble what a surface needs.
///
/// A port the daemon already holds is left as it is rather than reconfigured.
/// Opening a monitor is not a request to change the line settings of a session
/// somebody else is using; the settings dialog and `devserial open` are.
#[cfg(any(feature = "monitor", feature = "tui"))]
fn attach_session(
    port: &str,
    config: &PortConfig,
    data_dir: &Path,
    socket: Option<std::path::PathBuf>,
    config_path: Option<&Path>,
) -> Result<SessionParts, String> {
    crate::paths::create_private_dir(data_dir)
        .map_err(|e| format!("failed to create data directory: {e}"))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to build the async runtime: {e}"))?;

    let endpoint = socket.unwrap_or_else(crate::paths::default_socket_path);
    let client = Arc::new(
        IpcClient::new(endpoint).with_config_path(config_path.map(std::path::Path::to_path_buf)),
    );
    let handle = runtime.handle().clone();

    let state = {
        let client = Arc::clone(&client);
        let port = port.to_string();
        let settings = config_settings(config);
        handle.block_on(async move {
            client
                .ensure_daemon()
                .await
                .map_err(|e| format!("could not reach the daemon: {e}"))?;

            let status = client
                .send(RequestPayload::GetStatus { port: port.clone() })
                .await;
            if let Ok(ResponsePayload::Status { state, .. }) = status {
                return Ok(state);
            }

            client
                .send(RequestPayload::OpenPort {
                    name: port.clone(),
                    settings,
                })
                .await
                .map_err(|e| format!("failed to open port '{port}': {e}"))?;

            // PortOpened carries a settings summary, not a link state, so the
            // state comes from asking rather than from the open.
            match client
                .send(RequestPayload::GetStatus { port: port.clone() })
                .await
            {
                Ok(ResponsePayload::Status { state, .. }) => Ok(state),
                Ok(_) => Err("unexpected answer from the daemon".to_string()),
                Err(e) => Err(format!("could not read the state of '{port}': {e}")),
            }
        })?
    };

    let db_path = crate::paths::port_db_path(data_dir, port);
    let storage = SqliteStorage::open(&db_path)
        .map_err(|e| format!("failed to open capture database: {e}"))?;
    let history = storage.load_send_history(500).unwrap_or_default();
    let shared_storage = Arc::new(Mutex::new(
        SqliteStorage::open(&db_path)
            .map_err(|e| format!("failed to open capture database: {e}"))?,
    ));

    let (link, poller) = spawn_link_poller(port, &client, &handle, state);

    Ok(SessionParts {
        #[cfg(feature = "monitor")]
        storage,
        shared_storage,
        client,
        keepalive: SessionKeepalive::new(runtime, link, poller),
        history,
    })
}

/// Open a monitoring session for use inside a running GUI.
///
/// # Errors
/// Returns an error if the daemon cannot open the port or the database fails.
#[cfg(feature = "monitor")]
pub fn open_standalone_session(
    port: &str,
    config: &PortConfig,
) -> Result<crate::monitor::PortMonitorState, String> {
    let parts = attach_session(port, config, &gui_data_dir(), None, None)?;
    let runtime = parts
        .keepalive
        .handle()
        .ok_or_else(|| "session runtime is gone".to_string())?;

    let writer = DaemonWriter {
        port: port.to_string(),
        client: Arc::clone(&parts.client),
        runtime: runtime.clone(),
    };

    let mut monitor = crate::monitor::PortMonitorState::new_with_reconfigure(
        port.to_string(),
        config.framing_summary(),
        parts.storage,
        parts.history.clone(),
        Some(Arc::new(Mutex::new(
            Box::new(writer) as Box<dyn std::io::Write + Send>
        ))),
        Some(daemon_reconfigure(port, &parts.client, runtime.clone())),
        Some(daemon_action(port, &parts.client, runtime.clone())),
        Some(config.clone()),
        Some(daemon_toggle(port, &parts.client, runtime.clone())),
        Some(Arc::clone(&parts.keepalive)),
    );
    #[cfg(feature = "rest")]
    {
        monitor.rest = Some(daemon_rest(&parts.client, runtime));
    }
    Ok(monitor)
}

/// Open a serial port on the daemon and show it in a window.
///
/// # Errors
/// Returns an error if the port cannot be opened or the window fails.
#[cfg(feature = "monitor")]
pub fn run_monitor_standalone(
    port: &str,
    config: &PortConfig,
    socket: Option<std::path::PathBuf>,
    config_path: Option<&Path>,
) -> Result<(), CliError> {
    let data_dir = resolve_data_dir(config_path);

    // Hand over to an already running GUI before opening anything. The
    // receiving instance attaches to the port itself, so this process does not
    // have to stay alive to show a window it did not create.
    let request = crate::gui_ipc::OpenPortRequest {
        port_name: port.to_string(),
        db_path: crate::paths::port_db_path(&data_dir, port),
        port_info: config.framing_summary(),
        settings: Some(config_settings(config)),
    };
    match crate::gui_ipc::try_open_in_existing_gui(&request) {
        Ok(true) => {
            println!("Opened {port} in the running devserial window.");
            return Ok(());
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %e, "could not reach the running GUI instance"),
    }

    let parts =
        attach_session(port, config, &data_dir, socket, config_path).map_err(CliError::msg)?;
    let runtime = parts
        .keepalive
        .handle()
        .ok_or_else(|| CliError::msg("session runtime is gone"))?;
    let writer = DaemonWriter {
        port: port.to_string(),
        client: Arc::clone(&parts.client),
        runtime: runtime.clone(),
    };

    let db_path = crate::paths::port_db_path(&data_dir, port);
    let info = config.framing_summary();
    let result = crate::monitor::run_monitor_with_port(
        port,
        &db_path,
        &info,
        Box::new(writer),
        Arc::clone(&parts.keepalive),
        #[cfg(feature = "rest")]
        Some(daemon_rest(&parts.client, runtime.clone())),
    );

    match result {
        Ok(()) => Ok(()),
        Err(gui_error) => {
            #[cfg(feature = "tui")]
            {
                eprintln!("GUI unavailable ({gui_error}), falling back to the TUI");
                run_tui_attached(port, config, &parts, &runtime, config_path)
            }
            #[cfg(not(feature = "tui"))]
            Err(CliError::msg(gui_error))
        }
    }
}

/// Launch the GUI without opening a port up front.
///
/// # Errors
/// Returns an error if the GUI cannot start.
#[cfg(feature = "monitor")]
pub fn run_monitor_gui_app() -> Result<(), CliError> {
    crate::monitor::run_monitor_gui().map_err(CliError::msg)
}

/// Open a serial port on the daemon and show it in the terminal.
///
/// # Errors
/// Returns an error if the port cannot be opened or the TUI fails.
#[cfg(feature = "tui")]
pub fn run_tui_standalone(
    port: &str,
    config: &PortConfig,
    socket: Option<std::path::PathBuf>,
    config_path: Option<&Path>,
) -> Result<(), CliError> {
    let data_dir = resolve_data_dir(config_path);
    let parts =
        attach_session(port, config, &data_dir, socket, config_path).map_err(CliError::msg)?;
    let runtime = parts
        .keepalive
        .handle()
        .ok_or_else(|| CliError::msg("session runtime is gone"))?;

    run_tui_attached(port, config, &parts, &runtime, config_path)
}

/// Run the terminal interface against an attached session.
///
/// Shared by `devserial tui` and by the window's fallback, which previously
/// carried its own copy of this wiring.
#[cfg(feature = "tui")]
fn run_tui_attached(
    port: &str,
    config: &PortConfig,
    parts: &SessionParts,
    runtime: &tokio::runtime::Handle,
    config_path: Option<&Path>,
) -> Result<(), CliError> {
    let writer = DaemonWriter {
        port: port.to_string(),
        client: Arc::clone(&parts.client),
        runtime: runtime.clone(),
    };

    let context = crate::tui::TuiContext {
        link: parts.keepalive.link(),
        action: Some(daemon_action(port, &parts.client, runtime.clone())),
        macros: macro_names(config_path),
        toggle: Some(daemon_toggle(port, &parts.client, runtime.clone())),
        history: parts.history.clone(),
        write: Some(Box::new(writer)),
        reconfigure: Some(daemon_reconfigure(port, &parts.client, runtime.clone())),
        transfer: Some(daemon_transfer(port, &parts.client, runtime.clone())),
        #[cfg(feature = "rest")]
        rest: Some(daemon_rest(&parts.client, runtime.clone())),
    };

    crate::tui::run_tui(port, config, &parts.shared_storage, runtime, context)
}

/// Protocol settings for a port configuration.
#[cfg(any(feature = "monitor", feature = "tui"))]
const fn config_settings(config: &PortConfig) -> crate::protocol::PortSettings {
    crate::protocol::PortSettings {
        baudrate: Some(config.baudrate),
        data_bits: Some(config.data_bits),
        parity: Some(config.parity),
        stop_bits: Some(config.stop_bits),
        flow_control: Some(config.flow_control),
    }
}

#[cfg(all(test, any(feature = "monitor", feature = "tui")))]
mod tests {
    use super::*;

    /// A daemon on a temporary socket, holding one mock port.
    ///
    /// Needs `testutil` for the mock, which the `--all-features` job in CI
    /// enables and the release build never does.
    #[cfg(all(unix, feature = "testutil"))]
    struct FakeDaemon {
        _dir: tempfile::TempDir,
        socket: std::path::PathBuf,
        data_dir: std::path::PathBuf,
        ports: crate::port_manager::PortManagerHandle,
        runtime: tokio::runtime::Runtime,
        _shutdown: tokio::sync::broadcast::Sender<()>,
    }

    #[cfg(all(unix, feature = "testutil"))]
    impl FakeDaemon {
        const PORT: &'static str = "mock_attached_port";

        fn start() -> Self {
            let _ = std::fs::create_dir_all("./target/tmp");
            let dir = tempfile::Builder::new().tempdir_in("./target/tmp").unwrap();
            let socket = dir.path().join("attached.sock");
            let pid = dir.path().join("attached.pid");
            let data_dir = dir.path().to_path_buf();

            // Its own runtime, because the session under test builds one of its
            // own and blocks on it; a server sharing that runtime would be
            // driven only while the session happened to be waiting.
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();

            // The handle spawns its actor task on construction, so it needs the
            // runtime entered rather than merely available.
            let ports = {
                let _guard = runtime.enter();
                crate::port_manager::PortManagerHandle::new()
            };
            let mut config = crate::config::Config::default();
            config.global.data_dir.clone_from(&data_dir);
            config.global.archive_dir = dir.path().join("archive");
            let engine = crate::engine::CommandEngine::new(
                ports.clone(),
                Arc::new(Mutex::new(crate::state::StateDb::open_memory().unwrap())),
                Arc::new(config),
            );

            let (mock, _control) = crate::testutil::mock_serial::mock_serial(1024);
            let storage = Arc::new(Mutex::new(
                SqliteStorage::open(&crate::paths::port_db_path(&data_dir, Self::PORT)).unwrap(),
            ));
            runtime
                .block_on(ports.open(
                    Self::PORT.to_string(),
                    Box::new(mock),
                    PortConfig::default(),
                    storage,
                ))
                .unwrap();

            let (shutdown, shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);
            let server = crate::ipc::IpcServer::new(engine, socket.clone(), pid);
            runtime.spawn(async move {
                let _ = server.run(shutdown_rx).await;
            });
            while !socket.exists() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }

            Self {
                _dir: dir,
                socket,
                data_dir,
                ports,
                runtime,
                _shutdown: shutdown,
            }
        }

        fn managed_ports(&self) -> Vec<String> {
            self.runtime
                .block_on(self.ports.list())
                .into_iter()
                .map(|p| p.name)
                .collect()
        }
    }

    /// The session attaches to the port the daemon holds, its writes reach the
    /// engine, and ending it leaves the port open.
    ///
    /// The last of those is the behaviour this change exists to produce: a
    /// monitor that closed the port on exit took the capture down with it.
    ///
    /// What this cannot prove is the byte arriving at the device. A mock port
    /// is registered as a reader, and the engine answers a write to one with
    /// "no hardware handle", so the assertion below checks that the request
    /// reached the engine and was carried out there rather than checking the
    /// wire. Write-through is covered by the CLI against real hardware.
    #[cfg(all(unix, feature = "testutil"))]
    #[test]
    fn the_session_attaches_and_leaves_the_port_open() {
        use std::io::Write as _;

        let daemon = FakeDaemon::start();
        assert!(
            daemon
                .managed_ports()
                .contains(&FakeDaemon::PORT.to_string()),
            "the daemon should be holding the mock port before the session starts"
        );

        let parts = attach_session(
            FakeDaemon::PORT,
            &PortConfig::default(),
            &daemon.data_dir,
            Some(daemon.socket.clone()),
            None,
        )
        .expect("attaching to a port the daemon already holds");

        // Attaching neither opened a second port nor disturbed the first.
        assert_eq!(
            daemon.managed_ports(),
            vec![FakeDaemon::PORT.to_string()],
            "attaching changed what the daemon holds"
        );

        let runtime = parts.keepalive.handle().unwrap();
        let mut writer = DaemonWriter {
            port: FakeDaemon::PORT.to_string(),
            client: Arc::clone(&parts.client),
            runtime,
        };
        let outcome = writer.write_all(b"ping");
        let reason = outcome
            .expect_err("a mock port has no write side")
            .to_string();
        assert!(
            reason.contains("mock port"),
            "the write should have been refused by the engine, for the mock's \
             own reason; instead: {reason}"
        );

        drop(parts);
        assert!(
            daemon
                .managed_ports()
                .contains(&FakeDaemon::PORT.to_string()),
            "ending the session closed the port; the capture would stop with it"
        );
    }

    #[test]
    fn settings_round_trip_through_the_protocol() {
        let config = PortConfig {
            baudrate: 9600,
            parity: crate::serial_params::Parity::Even,
            ..PortConfig::default()
        };
        let settings = config_settings(&config);
        assert_eq!(settings.baudrate, Some(9600));
        assert_eq!(settings.parity, Some(crate::serial_params::Parity::Even));
    }

    #[test]
    fn data_dir_follows_the_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let config_file = dir.path().join("devserial.toml");
        let data_dir = dir.path().join("captures");
        // The value is serialised rather than interpolated. A Windows path
        // carries backslashes, and \U or \T is not a valid TOML escape, so
        // interpolation produced a file that failed to parse; resolve_data_dir
        // then fell back to the platform default and the assertion compared
        // two unrelated paths.
        let value = toml::Value::String(data_dir.to_string_lossy().into_owned());
        std::fs::write(&config_file, format!("[global]\ndata_dir = {value}\n")).unwrap();

        assert_eq!(resolve_data_dir(Some(&config_file)), data_dir);
    }

    #[test]
    fn every_action_maps_to_the_request_that_performs_it() {
        // A surface control that produced no request, or the wrong one, would
        // look like it worked and change nothing on the device.
        let port = "/dev/ttyUSB0";

        assert!(matches!(
            PortAction::Break {
                duration_ms: Some(7)
            }
            .payload(port),
            RequestPayload::SendBreak {
                duration_ms: Some(7),
                ..
            }
        ));
        assert!(matches!(
            PortAction::Signal {
                dtr: Some(true),
                rts: None
            }
            .payload(port),
            RequestPayload::SetSignal {
                dtr: Some(true),
                rts: None,
                ..
            }
        ));
        assert!(matches!(
            PortAction::Macro {
                name: "reset".to_string()
            }
            .payload(port),
            RequestPayload::ExecuteMacro { .. }
        ));

        for action in [
            PortAction::Break { duration_ms: None },
            PortAction::Signal {
                dtr: None,
                rts: Some(false),
            },
            PortAction::Macro {
                name: "enter_bootloader".to_string(),
            },
        ] {
            let named = match action.payload(port) {
                RequestPayload::SendBreak { port, .. }
                | RequestPayload::SetSignal { port, .. }
                | RequestPayload::ExecuteMacro { port, .. } => port,
                other => panic!("{action:?} produced {other:?}"),
            };
            assert_eq!(named, port, "the request names another port");
        }
    }
}
