// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Native GUI serial monitor window using egui/eframe with multi-window support.
//!
//! On macOS, all GUI windows run in a single application instance, sharing
//! exactly one Dock icon and appearing in the native `Window (Fenster)` menu.
//! Subsequent `devserial monitor` invocations multiplex into the running GUI
//! instance via a local domain socket.

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui;

use crate::config::PortConfig;
use crate::export::{self, ExportFormat};
use crate::gui_ipc::{GuiCommand, MonitorEvent, OpenPortRequest};
use crate::platform::{self, EditState, MenuRequest};
use crate::serial_params::{BAUD_PRESETS, DEFAULT_BAUD, DataBits, FlowControl, Parity, StopBits};

/// Maximum lines kept in the display buffer.
const MAX_DISPLAY_LINES: usize = 100_000;

/// Upper bound on lines pulled into memory for one export.
const MAX_EXPORT_LINES: u32 = 500_000;

/// Callback type for direct hardware reconfiguration in standalone mode.
pub type ReconfigureFn = Arc<dyn Fn(&PortConfig) -> Result<(), String> + Send + Sync>;

/// Callback type for direct connection toggling (connect/disconnect) in standalone mode.
pub type ToggleConnectFn = Arc<dyn Fn(bool, &PortConfig) -> Result<(), String> + Send + Sync>;

// --- Public API (spawn/handle) ---

/// Handle to a running monitor child process.
pub struct MonitorHandle {
    child: std::process::Child,
    shutdown: Arc<AtomicBool>,
}

impl MonitorHandle {
    /// Signal the monitor to close.
    pub fn close(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.child.kill().ok();
        self.child.wait().ok();
    }

    /// Check if the monitor process is still running.
    #[must_use]
    pub fn is_open(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    /// Take the stdout pipe for reading send-data from the monitor.
    #[allow(clippy::missing_const_for_fn)] // Option::take is not const
    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }

    /// Take the stdin pipe for sending notifications to the monitor.
    #[allow(clippy::missing_const_for_fn)]
    pub fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.child.stdin.take()
    }
}

/// Spawn a monitor as a child process.
///
/// # Errors
/// Returns error if the child process cannot be spawned.
pub fn spawn_monitor(
    port_name: &str,
    db_path: &Path,
    port_info: &str,
) -> Result<MonitorHandle, std::io::Error> {
    let exe = std::env::current_exe()?;
    let child = std::process::Command::new(exe)
        .arg("monitor-subprocess")
        .arg("--port")
        .arg(port_name)
        .arg("--db")
        .arg(db_path)
        .arg("--info")
        .arg(port_info)
        .stdin(std::process::Stdio::piped()) // parent-death detection
        .stdout(std::process::Stdio::piped()) // send-data channel (monitor → server)
        .stderr(std::process::Stdio::inherit())
        .spawn()?;

    Ok(MonitorHandle {
        child,
        shutdown: Arc::new(AtomicBool::new(false)),
    })
}

/// Entry point for the monitor subprocess started by the MCP server.
///
/// The parent process owns the serial port, so this window only displays the
/// capture database and reports user actions back over stdout.
///
/// # Errors
/// Returns an error if the database cannot be opened or the window fails.
pub fn run_monitor(port_name: &str, db_path: &Path, port_info: &str) -> Result<(), String> {
    let request = OpenPortRequest {
        port_name: port_name.to_string(),
        db_path: db_path.to_path_buf(),
        port_info: port_info.to_string(),
        settings: None,
    };

    if matches!(crate::gui_ipc::try_open_in_existing_gui(&request), Ok(true)) {
        // A window already exists in the running instance. Stay alive until the
        // parent closes our stdin, because the parent uses that pipe to detect
        // that the window is gone.
        wait_for_parent();
        return Ok(());
    }

    run_monitor_inner(port_name, db_path, port_info, None, None, None)
}

/// Block until the parent process closes our stdin.
fn wait_for_parent() {
    use std::io::Read;
    let mut buf = [0u8; 64];
    let stdin = std::io::stdin();
    loop {
        let read = stdin.lock().read(&mut buf).unwrap_or(0);
        if read == 0 {
            return;
        }
    }
}

/// Run the monitor for a port this process owns.
///
/// # Errors
/// Returns an error if the database cannot be opened or the window fails.
pub fn run_monitor_with_port(
    port_name: &str,
    db_path: &Path,
    port_info: &str,
    write_port: Box<dyn std::io::Write + Send>,
    keepalive: Arc<crate::standalone::SessionKeepalive>,
) -> Result<(), String> {
    run_monitor_inner(
        port_name,
        db_path,
        port_info,
        Some(Arc::new(std::sync::Mutex::new(write_port))),
        None,
        Some(keepalive),
    )
}

/// Publish the current edit state to the native menu.
pub fn update_macos_edit_state(state: EditState) {
    platform::update_edit_state(state);
}

/// Clipboard text from the platform, if it offers native access.
fn get_clipboard_text() -> Option<String> {
    platform::clipboard_text()
}

/// Context menu shared by every text field in the GUI.
fn text_edit_context_menu(ui: &mut egui::Ui, text: &mut String) {
    let has_text = !text.is_empty();
    let clipboard = get_clipboard_text();
    let has_clipboard = clipboard.as_ref().is_some_and(|s| !s.is_empty());

    if ui
        .add_enabled(has_text, egui::Button::new("✂ Cut").shortcut_text("Cmd+X"))
        .clicked()
    {
        ui.ctx().copy_text(text.clone());
        text.clear();
        ui.close();
    }

    if ui
        .add_enabled(
            has_text,
            egui::Button::new("📋 Copy").shortcut_text("Cmd+C"),
        )
        .clicked()
    {
        ui.ctx().copy_text(text.clone());
        ui.close();
    }

    if ui
        .add_enabled(
            has_clipboard,
            egui::Button::new("📥 Paste").shortcut_text("Cmd+V"),
        )
        .clicked()
    {
        if let Some(clip) = clipboard {
            text.push_str(&clip);
        }
        ui.close();
    }

    ui.separator();

    if ui
        .add_enabled(
            has_text,
            egui::Button::new("Select All").shortcut_text("Cmd+A"),
        )
        .clicked()
    {
        ui.close();
    }

    if ui
        .add_enabled(has_text, egui::Button::new("⌫ Clear"))
        .clicked()
    {
        text.clear();
        ui.close();
    }
}

/// Standard window options for a devserial GUI window.
fn window_options(title: String, size: [f32; 2], min_size: [f32; 2]) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("devserial")
            .with_title(title)
            .with_icon(load_icon())
            .with_inner_size(size)
            .with_min_inner_size(min_size),
        ..Default::default()
    }
}

/// Install fonts and spacing.
///
/// Shared by both entry points, which previously carried two copies of this
/// hundred-line block.
fn configure_style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "jetbrains_mono".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../resources/fonts/JetBrainsMono-Regular.ttf"
        ))),
    );
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "jetbrains_mono".to_owned());

    // Prefer the native UI face for proportional text where one exists.
    #[cfg(target_os = "macos")]
    {
        const CANDIDATES: [&str; 4] = [
            "/System/Library/Fonts/SFNS.ttf",
            "/System/Library/Fonts/SFPro.ttf",
            "/System/Library/Fonts/SF-Pro.ttf",
            "/Library/Fonts/SF-Pro.ttf",
        ];
        for path in CANDIDATES {
            if let Ok(data) = std::fs::read(path) {
                fonts.font_data.insert(
                    "system_ui".to_owned(),
                    Arc::new(egui::FontData::from_owned(data)),
                );
                fonts
                    .families
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .insert(0, "system_ui".to_owned());
                break;
            }
        }
    }

    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .push("jetbrains_mono".to_owned());

    ctx.set_fonts(fonts);

    let mut style = (*ctx.global_style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 4.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    ctx.set_global_style(style);
}

/// Start the GUI socket server if this platform supports multiplexing.
fn start_socket_server(
    tx: std::sync::mpsc::Sender<GuiCommand>,
    ctx_holder: &Arc<std::sync::Mutex<Option<egui::Context>>>,
    open_ports: &crate::gui_ipc::OpenPortsView,
) -> Option<crate::gui_ipc::GuiSocketServer> {
    #[cfg(unix)]
    {
        match crate::gui_ipc::start_gui_socket_server(
            tx,
            Arc::clone(ctx_holder),
            Arc::clone(open_ports),
        ) {
            Ok(server) => Some(server),
            Err(e) => {
                tracing::warn!(error = %e, "GUI multiplexing is unavailable");
                None
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (tx, ctx_holder, open_ports);
        None
    }
}

/// Run the GUI without opening a port up front.
///
/// # Errors
/// Returns an error if the window cannot be initialized.
pub fn run_monitor_gui() -> Result<(), String> {
    platform::init_app();

    let ctx_holder: Arc<std::sync::Mutex<Option<egui::Context>>> =
        Arc::new(std::sync::Mutex::new(None));
    let open_ports: crate::gui_ipc::OpenPortsView = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (tx, rx) = std::sync::mpsc::channel();
    let socket_server = start_socket_server(tx, &ctx_holder, &open_ports);

    let mut config_dialog = PortConfigDialogState::new();
    config_dialog.is_open = true;

    let app = MultiMonitorApp {
        monitors: Vec::new(),
        commands: rx,
        open_ports,
        _socket_server: socket_server,
        icon_texture: None,
        config_dialog,
    };

    let options = window_options(
        format!("devserial v{}", env!("CARGO_PKG_VERSION")),
        [920.0, 620.0],
        [520.0, 380.0],
    );

    eframe::run_native(
        "devserial",
        options,
        Box::new(move |cc| {
            platform::init_app();
            configure_style(&cc.egui_ctx);
            if let Ok(mut guard) = ctx_holder.lock() {
                *guard = Some(cc.egui_ctx.clone());
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| format!("eframe error: {e}"))
}

fn run_monitor_inner(
    port_name: &str,
    db_path: &Path,
    port_info: &str,
    direct_port: Option<Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>>,
    direct_reconfigure: Option<ReconfigureFn>,
    keepalive: Option<Arc<crate::standalone::SessionKeepalive>>,
) -> Result<(), String> {
    platform::init_app();

    let ctx_holder: Arc<std::sync::Mutex<Option<egui::Context>>> =
        Arc::new(std::sync::Mutex::new(None));
    let open_ports: crate::gui_ipc::OpenPortsView =
        Arc::new(std::sync::Mutex::new(vec![port_name.to_string()]));

    let storage = crate::storage::SqliteStorage::open(db_path)
        .map_err(|e| format!("failed to open capture database: {e}"))?;
    let history = storage.load_send_history(500).unwrap_or_default();
    let owns_port = direct_port.is_some();

    let initial = PortMonitorState::new_with_reconfigure(
        port_name.to_string(),
        port_info.to_string(),
        storage,
        history,
        direct_port,
        direct_reconfigure,
        None,
        None,
        keepalive,
    );

    let (tx, rx) = std::sync::mpsc::channel();
    let socket_server = start_socket_server(tx, &ctx_holder, &open_ports);

    let app = MultiMonitorApp {
        monitors: vec![initial],
        commands: rx,
        open_ports,
        _socket_server: socket_server,
        icon_texture: None,
        config_dialog: PortConfigDialogState::new(),
    };

    let options = window_options(
        format!(
            "{port_name} ({port_info}) — devserial v{}",
            env!("CARGO_PKG_VERSION")
        ),
        [900.0, 600.0],
        [500.0, 350.0],
    );

    let ctx_for_thread = Arc::clone(&ctx_holder);
    eframe::run_native(
        "devserial",
        options,
        Box::new(move |cc| {
            platform::init_app();
            configure_style(&cc.egui_ctx);
            if let Ok(mut guard) = ctx_for_thread.lock() {
                *guard = Some(cc.egui_ctx.clone());
            }

            // Windows driven by a parent process use stdin as their lifeline:
            // a byte means new data, end of file means the parent is gone.
            if !owns_port {
                std::thread::spawn(move || {
                    use std::io::Read;
                    let mut buf = [0u8; 64];
                    let stdin = std::io::stdin();
                    loop {
                        if stdin.lock().read(&mut buf).unwrap_or(0) == 0 {
                            std::process::exit(0);
                        }
                        if let Some(ctx) = ctx_holder.lock().ok().and_then(|g| g.clone()) {
                            ctx.request_repaint();
                        }
                    }
                });
            }

            Ok(Box::new(app))
        }),
    )
    .map_err(|e| format!("eframe error: {e}"))
}

// --- Internal types ---

#[derive(Clone)]
struct DisplayLine {
    timestamp: String,
    /// Raw ingestion timestamp, kept so an export of the visible buffer carries
    /// real times instead of blanks.
    timestamp_ns: i64,
    payload: String,
    is_sent: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LineEnding {
    None,
    Lf,
    CrLf,
    Cr,
}

impl LineEnding {
    const fn suffix(self) -> &'static [u8] {
        match self {
            Self::None => b"",
            Self::Lf => b"\n",
            Self::CrLf => b"\r\n",
            Self::Cr => b"\r",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Lf => "LF",
            Self::CrLf => "CRLF",
            Self::Cr => "CR",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ExportScope {
    AllDb,
    VisibleBuffer,
    CustomRange,
}

// --- macOS-style Dialog Action Button Bar ---

/// Standard macOS-style dialog action button bar.
///
/// Places Cancel on the left and the primary action button on the far right
/// with macOS blue accent styling and proper keyboard handling (Enter / Escape).
pub struct MacosDialogButtons<'a> {
    pub cancel_label: &'a str,
    pub primary_label: &'a str,
    pub primary_enabled: bool,
    pub primary_tooltip: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogAction {
    None,
    Cancel,
    Primary,
}

impl<'a> MacosDialogButtons<'a> {
    #[must_use]
    pub const fn new(primary_label: &'a str) -> Self {
        Self {
            cancel_label: "Cancel",
            primary_label,
            primary_enabled: true,
            primary_tooltip: None,
        }
    }

    #[must_use]
    pub const fn with_cancel(mut self, cancel_label: &'a str) -> Self {
        self.cancel_label = cancel_label;
        self
    }

    #[must_use]
    pub const fn with_enabled(mut self, enabled: bool) -> Self {
        self.primary_enabled = enabled;
        self
    }

    #[must_use]
    pub const fn with_tooltip(mut self, tooltip: &'a str) -> Self {
        self.primary_tooltip = Some(tooltip);
        self
    }

    pub fn show(&self, ui: &mut egui::Ui) -> DialogAction {
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        let mut action = DialogAction::None;

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut primary_btn =
                egui::Button::new(egui::RichText::new(self.primary_label).strong().color(
                    if self.primary_enabled {
                        egui::Color32::from_rgb(255, 255, 255)
                    } else {
                        egui::Color32::from_rgb(140, 140, 140)
                    },
                ))
                .min_size(egui::vec2(84.0, 24.0));

            if self.primary_enabled {
                primary_btn = primary_btn.fill(egui::Color32::from_rgb(0, 122, 255));
            }

            let mut primary_resp = ui.add_enabled(self.primary_enabled, primary_btn);
            if let Some(tt) = self.primary_tooltip {
                primary_resp = primary_resp.on_hover_text(tt);
            }
            if primary_resp.clicked()
                || (self.primary_enabled && ui.input(|i| i.key_pressed(egui::Key::Enter)))
            {
                action = DialogAction::Primary;
            }

            ui.add_space(8.0);

            if !self.cancel_label.is_empty() {
                let cancel_btn =
                    egui::Button::new(self.cancel_label).min_size(egui::vec2(72.0, 24.0));
                let cancel_resp = ui.add(cancel_btn);
                if cancel_resp.clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    action = DialogAction::Cancel;
                }
            }
        });

        action
    }
}

/// Whether a port name refers to a device an open window already holds.
///
/// Compares devices rather than strings, so the second device node of the same
/// hardware is recognised too.
fn is_active(active_ports: &[String], port: &str) -> bool {
    active_ports
        .iter()
        .any(|active| crate::port_manager::same_device(active, port))
}

// --- Shared line-setting controls ---

/// Baud selector with presets and a free-text field.
///
/// Used by the connection dialog and by the per-window settings dialog, which
/// previously carried two copies of this list.
fn baud_controls(ui: &mut egui::Ui, id: &str, baud: &mut u32, custom: &mut String) {
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt(format!("baud_combo_{id}"))
            .selected_text(format!("{baud} baud"))
            .width(130.0)
            .show_ui(ui, |ui| {
                for preset in BAUD_PRESETS {
                    if ui
                        .selectable_value(baud, preset, format!("{preset}"))
                        .clicked()
                    {
                        *custom = preset.to_string();
                    }
                }
            });

        ui.label("Custom:");
        let response = ui.add(
            egui::TextEdit::singleline(custom)
                .desired_width(90.0)
                .font(egui::TextStyle::Monospace),
        );
        response.context_menu(|ui| {
            text_edit_context_menu(ui, custom);
        });
        if response.changed() {
            if let Ok(value) = custom.trim().parse::<u32>() {
                if value > 0 {
                    *baud = value;
                }
            }
        }
    });
}

/// Data bits, parity, stop bits and flow control for one configuration.
fn framing_controls(ui: &mut egui::Ui, id: &str, config: &mut PortConfig) {
    egui::Grid::new(format!("framing_grid_{id}"))
        .num_columns(4)
        .spacing([14.0, 6.0])
        .show(ui, |ui| {
            ui.label("Data Bits:");
            egui::ComboBox::from_id_salt(format!("framing_db_{id}"))
                .selected_text(config.data_bits.to_string())
                .width(65.0)
                .show_ui(ui, |ui| {
                    for value in DataBits::all() {
                        ui.selectable_value(&mut config.data_bits, value, value.to_string());
                    }
                });

            ui.label("Parity:");
            egui::ComboBox::from_id_salt(format!("framing_parity_{id}"))
                .selected_text(config.parity.label())
                .width(95.0)
                .show_ui(ui, |ui| {
                    for value in Parity::all() {
                        ui.selectable_value(&mut config.parity, value, value.label());
                    }
                });
            ui.end_row();

            ui.label("Stop Bits:");
            egui::ComboBox::from_id_salt(format!("framing_sb_{id}"))
                .selected_text(config.stop_bits.to_string())
                .width(65.0)
                .show_ui(ui, |ui| {
                    for value in StopBits::all() {
                        ui.selectable_value(&mut config.stop_bits, value, value.to_string());
                    }
                });

            ui.label("Flow Control:");
            egui::ComboBox::from_id_salt(format!("framing_fc_{id}"))
                .selected_text(config.flow_control.label())
                .width(95.0)
                .show_ui(ui, |ui| {
                    for value in FlowControl::all() {
                        ui.selectable_value(&mut config.flow_control, value, value.label());
                    }
                });
            ui.end_row();
        });
}

// --- Port Configuration & Connection Dialog State ---

#[derive(Clone)]
pub struct PortConfigDialogState {
    pub available_ports: Vec<String>,
    pub selected_port: String,
    pub custom_port: String,
    pub use_custom_port: bool,
    /// Line settings the dialog will open the port with.
    pub config: PortConfig,
    pub custom_baud: String,
    pub is_open: bool,
    pub error_message: Option<String>,
}

impl PortConfigDialogState {
    #[must_use]
    pub fn new() -> Self {
        let mut state = Self {
            available_ports: Vec::new(),
            selected_port: String::new(),
            custom_port: String::new(),
            use_custom_port: false,
            config: PortConfig::default(),
            custom_baud: DEFAULT_BAUD.to_string(),
            is_open: false,
            error_message: None,
        };
        state.rescan();
        state
    }

    /// Refresh the list of offered ports.
    pub fn rescan(&mut self) {
        self.available_ports = crate::port_manager::available_ports();
        if (self.selected_port.is_empty() || !self.available_ports.contains(&self.selected_port))
            && !self.available_ports.is_empty()
        {
            self.selected_port = self.available_ports[0].clone();
        }
    }

    #[must_use]
    pub fn effective_port(&self) -> String {
        if self.use_custom_port || self.selected_port.is_empty() {
            self.custom_port.trim().to_string()
        } else {
            self.selected_port.clone()
        }
    }

    /// Baud rate the dialog will use, preferring a non-empty custom entry.
    #[must_use]
    pub fn effective_baud(&self) -> u32 {
        self.custom_baud
            .trim()
            .parse()
            .ok()
            .filter(|value| *value > 0)
            .unwrap_or(self.config.baudrate)
    }

    /// Full configuration the dialog will open the port with.
    #[must_use]
    pub fn effective_config(&self) -> PortConfig {
        PortConfig {
            baudrate: self.effective_baud(),
            ..self.config.clone()
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn render_form(&mut self, ui: &mut egui::Ui, active_ports: &[String]) -> Option<bool> {
        ui.add_space(4.0);

        // Port selection
        ui.label(egui::RichText::new("Serial Port / Device:").strong());
        ui.horizontal(|ui| {
            if self.available_ports.is_empty() || self.use_custom_port {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.custom_port)
                        .desired_width(260.0)
                        .hint_text("/dev/cu.usbmodem... or COM3"),
                );
                resp.context_menu(|ui| {
                    text_edit_context_menu(ui, &mut self.custom_port);
                });
            } else {
                let selected_is_open = is_active(active_ports, &self.selected_port);
                let display_title = if self.selected_port.is_empty() {
                    "Select Port...".to_string()
                } else if selected_is_open {
                    format!("{} (Already Open)", self.selected_port)
                } else {
                    self.selected_port.clone()
                };

                egui::ComboBox::from_id_salt("port_selection_combo")
                    .selected_text(display_title)
                    .width(260.0)
                    .show_ui(ui, |ui| {
                        for p in &self.available_ports {
                            let is_open = is_active(active_ports, p);
                            let label = if is_open {
                                format!("{p} (Already Open)")
                            } else {
                                p.clone()
                            };
                            if is_open {
                                ui.add_enabled_ui(false, |ui| {
                                    ui.selectable_value(&mut self.selected_port, p.clone(), label);
                                });
                            } else {
                                ui.selectable_value(&mut self.selected_port, p.clone(), label);
                            }
                        }
                    });
            }

            if ui
                .button("🔄 Rescan")
                .on_hover_text("Rescan available serial ports")
                .clicked()
            {
                self.rescan();
            }

            ui.checkbox(&mut self.use_custom_port, "Custom path");
        });

        let eff_port = self.effective_port();
        let is_already_open = is_active(active_ports, &eff_port);

        if is_already_open {
            ui.add_space(4.0);
            ui.colored_label(
                egui::Color32::from_rgb(255, 180, 60),
                format!("ℹ '{eff_port}' is already active in an open window."),
            );
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        // Baud rate
        ui.label(egui::RichText::new("Baud Rate:").strong());
        baud_controls(
            ui,
            "connect_dialog",
            &mut self.config.baudrate,
            &mut self.custom_baud,
        );

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        ui.label(egui::RichText::new("Framing & Flow Control:").strong());
        framing_controls(ui, "connect_dialog", &mut self.config);

        if let Some(ref err) = self.error_message {
            ui.add_space(8.0);
            ui.colored_label(egui::Color32::from_rgb(255, 90, 90), format!("⚠ {err}"));
        }

        let has_port = !eff_port.is_empty() && !is_already_open;
        match MacosDialogButtons::new("🔌 Connect / Open Port")
            .with_cancel("Cancel")
            .with_enabled(has_port)
            .with_tooltip("Open connection to serial port and start live monitoring")
            .show(ui)
        {
            DialogAction::Primary => Some(true),
            DialogAction::Cancel => Some(false),
            DialogAction::None => None,
        }
    }
}

impl Default for PortConfigDialogState {
    fn default() -> Self {
        Self::new()
    }
}

// --- Multi-Monitor App State ---

#[allow(clippy::struct_excessive_bools)]
pub struct PortMonitorState {
    pub port_name: String,
    pub port_info: String,
    pub storage: crate::storage::SqliteStorage,
    lines: VecDeque<DisplayLine>,
    filtered_indices: Vec<usize>,
    auto_follow: bool,
    paused: bool,
    input: String,
    filter: String,
    filter_active: bool,
    line_ending: LineEnding,
    show_timestamps: bool,
    hex_view: bool,
    dtr_state: bool,
    rts_state: bool,
    last_id: i64,
    history: Vec<String>,
    history_idx: Option<usize>,
    history_draft: String,
    connected: bool,
    pub direct_port: Option<Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>>,
    pub direct_reconfigure: Option<ReconfigureFn>,
    show_transfer_dialog: bool,
    transfer_path: String,
    transfer_proto: crate::modem::FileTransferProtocol,
    transfer_status: Option<String>,
    show_about_dialog: bool,
    pub show_settings_dialog: bool,
    /// Live line settings; the single place this window keeps them.
    pub config: PortConfig,
    pub settings_custom_baud: String,
    pub settings_status: Option<(String, bool)>,
    show_export_dialog: bool,
    export_path: String,
    export_format: ExportFormat,
    export_scope: ExportScope,
    export_start_line: String,
    export_end_line: String,
    export_status: Option<(String, bool)>,
    pub is_open: bool,
    pub open_port_dialog_requested: bool,
    pub is_selecting_in_buffer: bool,
    pub toggle_connect: Option<ToggleConnectFn>,
    /// Keeps the session runtime and reader alive for as long as this window
    /// exists. Dropping it shuts both down in order.
    _keepalive: Option<Arc<crate::standalone::SessionKeepalive>>,
}

impl PortMonitorState {
    #[must_use]
    pub fn new(
        port_name: String,
        port_info: String,
        storage: crate::storage::SqliteStorage,
        history: Vec<String>,
        direct_port: Option<Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>>,
    ) -> Self {
        Self::new_with_reconfigure(
            port_name,
            port_info,
            storage,
            history,
            direct_port,
            None,
            None,
            None,
            None,
        )
    }

    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_reconfigure(
        port_name: String,
        port_info: String,
        storage: crate::storage::SqliteStorage,
        history: Vec<String>,
        direct_port: Option<Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>>,
        direct_reconfigure: Option<ReconfigureFn>,
        initial_config: Option<PortConfig>,
        toggle_connect: Option<ToggleConnectFn>,
        keepalive: Option<Arc<crate::standalone::SessionKeepalive>>,
    ) -> Self {
        let config = initial_config.unwrap_or_default();

        Self {
            port_name,
            port_info,
            storage,
            lines: VecDeque::new(),
            filtered_indices: Vec::new(),
            auto_follow: true,
            paused: false,
            input: String::new(),
            filter: String::new(),
            filter_active: false,
            line_ending: LineEnding::CrLf,
            show_timestamps: true,
            hex_view: false,
            dtr_state: false,
            rts_state: false,
            last_id: i64::MAX,
            history,
            history_idx: None,
            history_draft: String::new(),
            connected: true,
            direct_port,
            direct_reconfigure,
            show_transfer_dialog: false,
            transfer_path: String::new(),
            transfer_proto: crate::modem::FileTransferProtocol::Zmodem,
            transfer_status: None,
            show_about_dialog: false,
            show_settings_dialog: false,
            settings_custom_baud: config.baudrate.to_string(),
            config,
            settings_status: None,
            show_export_dialog: false,
            export_path: String::new(),
            export_format: ExportFormat::Txt,
            export_scope: ExportScope::AllDb,
            export_start_line: "1".to_string(),
            export_end_line: String::new(),
            export_status: None,
            is_open: true,
            open_port_dialog_requested: false,
            is_selecting_in_buffer: false,
            toggle_connect,
            _keepalive: keepalive,
        }
    }

    /// Live line settings of this window.
    #[must_use]
    pub fn current_config(&self) -> PortConfig {
        self.config.clone()
    }

    /// Connect or disconnect the port this window owns.
    ///
    /// Windows that only display a capture database, such as the monitor
    /// subprocess of the MCP server, do not own the port and say so instead of
    /// sending a command nobody handles.
    pub fn toggle_connection(&mut self) {
        let Some(toggle) = self.toggle_connect.clone() else {
            self.settings_status = Some((
                "This window does not own the port; connect and disconnect are unavailable here."
                    .to_string(),
                true,
            ));
            return;
        };

        let config = self.current_config();
        let connect = !self.connected;
        if let Err(e) = toggle(connect, &config) {
            let action = if connect { "Reconnect" } else { "Disconnect" };
            self.settings_status = Some((format!("{action} failed: {e}"), true));
            return;
        }

        self.connected = connect;
        let notice = if connect {
            format!(
                "━━━ PORT CONNECTED: {} ({}) ━━━",
                self.port_name, self.port_info
            )
        } else {
            format!("━━━ PORT DISCONNECTED: {} ━━━", self.port_name)
        };
        self.note(&notice);

        if self.filter_active {
            self.rebuild_filter();
        }
    }

    /// Append a marker line to both the display buffer and the database.
    fn note(&mut self, text: &str) {
        let timestamp_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        self.push_display_line(DisplayLine {
            timestamp: chrono::Utc::now().format("%H:%M:%S%.3f").to_string(),
            timestamp_ns,
            payload: text.to_string(),
            is_sent: false,
        });
        if let Err(e) = self.storage.insert_lines(&[(timestamp_ns, text)]) {
            tracing::debug!(error = %e, "could not persist marker line");
        }
    }

    /// Push one line into the display buffer, keeping it within its cap.
    fn push_display_line(&mut self, line: DisplayLine) {
        self.lines.push_back(line);
        while self.lines.len() > MAX_DISPLAY_LINES {
            self.lines.pop_front();
            self.filtered_indices.retain_mut(|index| {
                if *index == 0 {
                    false
                } else {
                    *index -= 1;
                    true
                }
            });
        }
    }

    fn update_lines(&mut self) {
        if !self.paused {
            if self.last_id == i64::MAX {
                if let Ok(stats) = self.storage.get_stats() {
                    self.last_id = i64::try_from(stats.total_lines).unwrap_or(0);
                }
            }

            if let Ok(new_lines) = self.storage.read_lines(self.last_id + 1, 500) {
                for line in &new_lines {
                    let timestamp = chrono::DateTime::from_timestamp_nanos(line.timestamp_ns)
                        .format("%H:%M:%S%.3f")
                        .to_string();
                    self.last_id = line.id;
                    self.push_display_line(DisplayLine {
                        timestamp,
                        timestamp_ns: line.timestamp_ns,
                        payload: line.payload.clone(),
                        is_sent: false,
                    });
                }
                if !new_lines.is_empty() {
                    self.rebuild_filter();
                }
            }
        }
    }

    fn render_ui(&mut self, ui: &mut egui::Ui, icon_texture: Option<&egui::TextureHandle>) {
        if platform::take_menu_request(MenuRequest::Export) {
            self.show_export_dialog = true;
            if self.export_path.is_empty() {
                let sanitized_port = crate::paths::sanitize_port_name(&self.port_name);
                let now = chrono::Local::now().format("%Y%m%d_%H%M%S");
                self.export_path = format!("devserial_export_{sanitized_port}_{now}.txt");
            }
        }

        if platform::take_menu_request(MenuRequest::ToggleConnect) {
            self.toggle_connection();
        }

        // Global shortcut: Cmd+K to toggle Disconnect / Connect
        if ui.input(|i| i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::K)) {
            self.toggle_connection();
        }

        // Global shortcut: Cmd+N to open New Window
        if ui.input(|i| i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::N)) {
            self.open_port_dialog_requested = true;
        }

        // Global shortcut: Cmd+E / Ctrl+E to open/toggle Export dialog
        if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::E)) {
            self.show_export_dialog = !self.show_export_dialog;
            if self.show_export_dialog && self.export_path.is_empty() {
                let sanitized_port = crate::paths::sanitize_port_name(&self.port_name);
                let now = chrono::Local::now().format("%Y%m%d_%H%M%S");
                self.export_path = format!("devserial_export_{sanitized_port}_{now}.txt");
            }
        }

        // Toolbar (top)
        egui::Panel::top(format!("toolbar_{}", self.port_name)).show_inside(ui, |ui| {
            self.render_toolbar(ui);
        });

        // Input bar (bottom)
        egui::Panel::bottom(format!("input_panel_{}", self.port_name)).show_inside(ui, |ui| {
            self.render_input(ui);
        });

        // Status bar (bottom)
        egui::Panel::bottom(format!("status_bar_{}", self.port_name))
            .max_size(22.0)
            .show_inside(ui, |ui| {
                self.render_status(ui);
            });

        // Main buffer (center)
        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.render_buffer(ui);
        });

        if self.show_settings_dialog {
            self.render_settings_dialog(ui.ctx());
        }

        if self.show_transfer_dialog {
            self.render_transfer_dialog(ui.ctx());
        }

        if self.show_export_dialog {
            self.render_export_dialog(ui.ctx());
        }

        if self.show_about_dialog {
            self.render_about_dialog(ui.ctx(), icon_texture);
        }

        // Compute context-aware edit state for macOS menu bar (Cut / Copy / Paste / Select All)
        let focused_id = ui.ctx().memory(egui::Memory::focused);
        let (is_text_focused, has_text_selection, text_field_not_empty) = if let Some(fid) =
            focused_id
        {
            if let Some(state) = egui::text_edit::TextEditState::load(ui.ctx(), fid) {
                let has_sel = state
                    .cursor
                    .char_range()
                    .is_some_and(|r| r.primary != r.secondary);
                let not_empty = if fid == egui::Id::new(format!("input_text_{}", self.port_name)) {
                    !self.input.is_empty()
                } else if fid == egui::Id::new(format!("filter_text_{}", self.port_name)) {
                    !self.filter.is_empty()
                } else {
                    true
                };
                (true, has_sel, not_empty)
            } else {
                (false, false, false)
            }
        } else {
            (false, false, false)
        };

        let buffer_not_empty = !self.lines.is_empty();
        let has_buffer_selection = self.is_selecting_in_buffer;

        update_macos_edit_state(EditState {
            text_focused: is_text_focused,

            text_selection: has_text_selection,

            text_not_empty: text_field_not_empty,

            buffer_not_empty,

            buffer_selection: has_buffer_selection,

            can_undo: text_field_not_empty,

            can_redo: false,
        });
    }
}

struct MultiMonitorApp {
    monitors: Vec<PortMonitorState>,
    /// Commands from other devserial invocations.
    commands: std::sync::mpsc::Receiver<GuiCommand>,
    /// Ports this instance shows, published for `Ping` answers.
    open_ports: crate::gui_ipc::OpenPortsView,
    _socket_server: Option<crate::gui_ipc::GuiSocketServer>,
    icon_texture: Option<egui::TextureHandle>,
    config_dialog: PortConfigDialogState,
}

impl MultiMonitorApp {
    /// Act on one command from another invocation.
    fn handle_command(&mut self, command: GuiCommand) {
        match command {
            GuiCommand::Ping => {}
            GuiCommand::OpenPort(request) => self.open_requested_port(request),
            GuiCommand::FocusPort { port_name } => {
                if let Some(monitor) = self
                    .monitors
                    .iter_mut()
                    .find(|m| crate::port_manager::same_device(&m.port_name, &port_name))
                {
                    monitor.is_open = true;
                }
            }
            GuiCommand::ClosePort { port_name } => {
                self.monitors
                    .retain(|m| !crate::port_manager::same_device(&m.port_name, &port_name));
            }
        }
    }

    /// Open a window for a port requested from outside.
    fn open_requested_port(&mut self, request: OpenPortRequest) {
        if request.port_name.is_empty() {
            self.config_dialog.is_open = true;
            self.config_dialog.rescan();
            return;
        }

        if let Some(monitor) = self
            .monitors
            .iter_mut()
            .find(|m| crate::port_manager::same_device(&m.port_name, &request.port_name))
        {
            monitor.is_open = true;
            return;
        }

        // With settings the sender expects this instance to own the port, so a
        // full session is started. Without them the sender keeps the port and
        // this window only shows the capture database.
        match request.settings {
            Some(settings) => {
                let config = PortConfig {
                    baudrate: settings.baudrate.unwrap_or(DEFAULT_BAUD),
                    data_bits: settings.data_bits.unwrap_or_default(),
                    parity: settings.parity.unwrap_or_default(),
                    stop_bits: settings.stop_bits.unwrap_or_default(),
                    flow_control: settings.flow_control.unwrap_or_default(),
                    ..PortConfig::default()
                };
                match crate::standalone::open_standalone_session(&request.port_name, &config) {
                    Ok(state) => self.monitors.push(state),
                    Err(e) => {
                        self.config_dialog.error_message = Some(e);
                        self.config_dialog.is_open = true;
                    }
                }
            }
            None => match crate::storage::SqliteStorage::open(&request.db_path) {
                Ok(storage) => {
                    let history = storage.load_send_history(500).unwrap_or_default();
                    self.monitors.push(PortMonitorState::new(
                        request.port_name,
                        request.port_info,
                        storage,
                        history,
                        None,
                    ));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not open the capture database");
                }
            },
        }
    }

    /// Publish the list of ports this instance shows.
    fn publish_open_ports(&self) {
        if let Ok(mut ports) = self.open_ports.lock() {
            ports.clear();
            ports.extend(self.monitors.iter().map(|m| m.port_name.clone()));
        }
    }

    /// Open a port from the connection dialog.
    fn connect_from_dialog(&mut self) {
        let port = self.config_dialog.effective_port();
        // The same hardware may be named by either of its device nodes, so an
        // existing window is matched by device and simply raised.
        if let Some(index) = self
            .monitors
            .iter()
            .position(|m| crate::port_manager::same_device(&m.port_name, &port))
        {
            self.monitors[index].is_open = true;
            if !self.monitors[index].connected {
                self.monitors[index].toggle_connection();
            }
            self.config_dialog.is_open = false;
            self.config_dialog.error_message = None;
            return;
        }

        match crate::standalone::open_standalone_session(
            &port,
            &self.config_dialog.effective_config(),
        ) {
            Ok(state) => {
                self.monitors.push(state);
                self.config_dialog.is_open = false;
                self.config_dialog.error_message = None;
            }
            Err(e) => self.config_dialog.error_message = Some(e),
        }
    }
}

impl eframe::App for MultiMonitorApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if platform::take_menu_request(MenuRequest::OpenPort) {
            self.config_dialog.is_open = true;
            self.config_dialog.rescan();
        }

        if platform::take_menu_request(MenuRequest::PortSettings) {
            if let Some(mon) = self.monitors.iter_mut().find(|m| m.is_open) {
                mon.show_settings_dialog = !mon.show_settings_dialog;
            }
        }

        // Global shortcut: Cmd+O / Ctrl+O to open Connection Manager
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::O)) {
            self.config_dialog.is_open = true;
            self.config_dialog.rescan();
        }

        // Global shortcut: Cmd+Shift+P to open Port Settings
        if ctx.input(|i| i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::P)) {
            if let Some(mon) = self.monitors.iter_mut().find(|m| m.is_open) {
                mon.show_settings_dialog = !mon.show_settings_dialog;
            }
        }

        // Drain commands from other invocations.
        while let Ok(command) = self.commands.try_recv() {
            self.handle_command(command);
        }
        self.publish_open_ports();

        // Check if any active monitor requested opening a new port from its toolbar
        for mon in &mut self.monitors {
            if mon.open_port_dialog_requested {
                mon.open_port_dialog_requested = false;
                self.config_dialog.is_open = true;
                self.config_dialog.rescan();
            }
        }

        // Update lines for all open monitors
        for mon in &mut self.monitors {
            if mon.is_open {
                mon.update_lines();
            }
        }

        // If not empty, and all monitors have been closed, exit application cleanly
        if !self.monitors.is_empty()
            && self.monitors.iter().all(|m| !m.is_open)
            && !self.config_dialog.is_open
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        ctx.request_repaint_after(std::time::Duration::from_millis(150));
    }

    #[allow(clippy::too_many_lines)]
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.icon_texture.is_none() {
            if let Ok(img) = image::load_from_memory(crate::assets::ICON_PNG) {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                #[allow(clippy::cast_possible_truncation)]
                let color_image = egui::ColorImage::from_rgba_unmultiplied(
                    [w as usize, h as usize],
                    &rgba.into_raw(),
                );
                self.icon_texture = Some(ui.ctx().load_texture(
                    "devserial_icon",
                    color_image,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }

        let primary_idx = self.monitors.iter().position(|m| m.is_open);

        // When no serial ports are open yet, display the Connection View directly in the main window!
        let Some(p_idx) = primary_idx else {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Title(format!(
                    "devserial v{} — Connect Serial Device",
                    env!("CARGO_PKG_VERSION")
                )));

            let mut connect_req = false;

            let active_ports: Vec<String> = self
                .monitors
                .iter()
                .filter(|m| m.is_open)
                .map(|m| m.port_name.clone())
                .collect();

            egui::CentralPanel::default().show_inside(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(24.0);
                    if let Some(ref tex) = self.icon_texture {
                        ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(64.0, 64.0)));
                        ui.add_space(6.0);
                    }
                    ui.heading(egui::RichText::new("devserial").strong().size(22.0));
                    ui.label(
                        egui::RichText::new("Interactive serial hardware monitor & MCP bridge")
                            .color(egui::Color32::from_rgb(170, 170, 180)),
                    );
                    ui.add_space(16.0);

                    egui::Frame::group(ui.style())
                        .inner_margin(16.0)
                        .show(ui, |ui| {
                            ui.set_max_width(520.0);
                            if let Some(action) = self.config_dialog.render_form(ui, &active_ports)
                            {
                                if action {
                                    connect_req = true;
                                }
                            }
                        });
                });
            });

            if connect_req {
                self.connect_from_dialog();
            }

            let focused_id = ui.ctx().memory(egui::Memory::focused);
            let (is_text_focused, has_text_selection, text_field_not_empty) =
                if let Some(fid) = focused_id {
                    if let Some(state) = egui::text_edit::TextEditState::load(ui.ctx(), fid) {
                        let has_sel = state
                            .cursor
                            .char_range()
                            .is_some_and(|r| r.primary != r.secondary);
                        (
                            true,
                            has_sel,
                            !self.config_dialog.custom_port.is_empty()
                                || !self.config_dialog.custom_baud.is_empty(),
                        )
                    } else {
                        (false, false, false)
                    }
                } else {
                    (false, false, false)
                };

            update_macos_edit_state(EditState {
                text_focused: is_text_focused,

                text_selection: has_text_selection,

                text_not_empty: text_field_not_empty,

                buffer_not_empty: false,

                buffer_selection: false,

                can_undo: text_field_not_empty,

                can_redo: false,
            });

            return;
        };

        // Render root viewport for the primary open monitor
        let primary_port_name = self.monitors[p_idx].port_name.clone();
        let primary_port_info = self.monitors[p_idx].port_info.clone();
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Title(format!(
                "{primary_port_name} ({primary_port_info}) — devserial v{}",
                env!("CARGO_PKG_VERSION")
            )));

        let open_count = self.monitors.iter().filter(|m| m.is_open).count();
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            if open_count > 1 {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
            self.monitors[p_idx].is_open = false;
        }

        let icon_tex = self.icon_texture.clone();
        self.monitors[p_idx].render_ui(ui, icon_tex.as_ref());

        // Show modal Connect Port dialog if triggered
        if self.config_dialog.is_open {
            let mut is_open = self.config_dialog.is_open;
            let mut connect_req = false;
            let mut close_req = false;

            let active_ports: Vec<String> = self
                .monitors
                .iter()
                .filter(|m| m.is_open)
                .map(|m| m.port_name.clone())
                .collect();

            egui::Window::new("Connect Serial Port")
                .open(&mut is_open)
                .resizable(false)
                .collapsible(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .default_width(460.0)
                .show(ui.ctx(), |ui| {
                    if let Some(action) = self.config_dialog.render_form(ui, &active_ports) {
                        if action {
                            connect_req = true;
                        } else {
                            close_req = true;
                        }
                    }
                });

            self.config_dialog.is_open = is_open && !close_req;

            if connect_req {
                self.connect_from_dialog();
            }
        }

        // Render secondary viewports for additional open monitors
        for (i, mon) in self.monitors.iter_mut().enumerate() {
            if i == p_idx || !mon.is_open {
                continue;
            }
            let port_name = mon.port_name.clone();
            let port_info = mon.port_info.clone();
            let viewport_id = egui::ViewportId::from_hash_of(&port_name);
            let icon_tex_ref = icon_tex.clone();

            ui.ctx().show_viewport_immediate(
                viewport_id,
                egui::ViewportBuilder::default()
                    .with_title(format!(
                        "{port_name} ({port_info}) — devserial v{}",
                        env!("CARGO_PKG_VERSION")
                    ))
                    .with_icon(load_icon())
                    .with_inner_size([900.0, 600.0])
                    .with_min_inner_size([500.0, 350.0]),
                |ctx, _class| {
                    if ctx.input(|i| i.viewport().close_requested()) {
                        mon.is_open = false;
                    }
                    #[allow(deprecated)]
                    egui::CentralPanel::default().show(ctx, |ui| {
                        mon.render_ui(ui, icon_tex_ref.as_ref());
                    });
                },
            );
        }
    }
}

// --- Rendering Details for PortMonitorState ---

impl PortMonitorState {
    #[allow(clippy::too_many_lines)]
    fn render_settings_dialog(&mut self, ctx: &egui::Context) {
        let mut is_open = self.show_settings_dialog;
        let mut close_requested = false;
        let mut apply_requested = false;

        egui::Window::new(format!("⚙ Port Settings — {}", self.port_name))
            .open(&mut is_open)
            .resizable(false)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .default_width(420.0)
            .show(ctx, |ui| {
                ui.heading(
                    egui::RichText::new("Runtime Port Configuration")
                        .strong()
                        .size(16.0),
                );
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(
                        "Change baud rate, framing, and flow control on the fly without closing the session.",
                    )
                    .size(11.5)
                    .color(egui::Color32::from_rgb(180, 180, 190)),
                );
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);

                ui.label(egui::RichText::new("Baud Rate:").strong());
                baud_controls(
                    ui,
                    &self.port_name,
                    &mut self.config.baudrate,
                    &mut self.settings_custom_baud,
                );

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);

                ui.label(egui::RichText::new("Framing & Flow Control:").strong());
                framing_controls(ui, &self.port_name, &mut self.config);

                if let Some((ref status_msg, is_err)) = self.settings_status {
                    ui.add_space(8.0);
                    let col = if is_err {
                        egui::Color32::from_rgb(255, 90, 90)
                    } else {
                        egui::Color32::from_rgb(80, 220, 120)
                    };
                    ui.colored_label(col, status_msg);
                }

                match MacosDialogButtons::new("✓ Apply Changes")
                    .with_cancel("Cancel")
                    .with_tooltip("Apply new connection parameters immediately")
                    .show(ui)
                {
                    DialogAction::Primary => apply_requested = true,
                    DialogAction::Cancel => close_requested = true,
                    DialogAction::None => {}
                }
            });

        if apply_requested {
            self.apply_reconfiguration();
        } else {
            self.show_settings_dialog = is_open && !close_requested;
        }
    }

    fn apply_reconfiguration(&mut self) {
        if let Ok(value) = self.settings_custom_baud.trim().parse::<u32>() {
            if value > 0 {
                self.config.baudrate = value;
            }
        }
        let config = self.current_config();

        let result = self.direct_reconfigure.as_ref().map_or_else(
            || {
                self.send_event(&MonitorEvent::Reconfigure {
                    settings: crate::protocol::PortSettings {
                        baudrate: Some(config.baudrate),
                        data_bits: Some(config.data_bits),
                        parity: Some(config.parity),
                        stop_bits: Some(config.stop_bits),
                        flow_control: Some(config.flow_control),
                    },
                })
            },
            |reconfigure| reconfigure(&config),
        );

        match result {
            Ok(()) => {
                let summary = config.framing_summary();
                self.port_info.clone_from(&summary);
                self.note(&format!("━━━ PORT RECONFIGURED: {summary} ━━━"));
                if self.filter_active {
                    self.rebuild_filter();
                }
                self.settings_status = Some((format!("Port reconfigured to {summary}"), false));
                self.show_settings_dialog = false;
            }
            Err(e) => {
                self.settings_status = Some((format!("Reconfiguration failed: {e}"), true));
                self.show_settings_dialog = true;
            }
        }
    }

    #[allow(clippy::too_many_lines)] // one contiguous egui layout description
    fn render_transfer_dialog(&mut self, ctx: &egui::Context) {
        let mut is_open = self.show_transfer_dialog;
        let mut close_requested = false;

        egui::Window::new(format!("File Transfer — {}", self.port_name))
            .open(&mut is_open)
            .resizable(false)
            .collapsible(false)
            .default_width(380.0)
            .show(ctx, |ui| {
                ui.label(
                    "Transfer files directly over the serial port using standard modem protocols.",
                );
                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    ui.label("Protocol:");
                    egui::ComboBox::from_id_salt(format!("proto_combo_{}", self.port_name))
                        .selected_text(format!("{:?}", self.transfer_proto))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.transfer_proto,
                                crate::modem::FileTransferProtocol::Zmodem,
                                "ZMODEM (Streaming, 32-bit CRC)",
                            );
                            ui.selectable_value(
                                &mut self.transfer_proto,
                                crate::modem::FileTransferProtocol::Ymodem,
                                "YMODEM (Batch, 1K CRC)",
                            );
                            ui.selectable_value(
                                &mut self.transfer_proto,
                                crate::modem::FileTransferProtocol::Xmodem1k,
                                "XMODEM-1K (1024-byte blocks)",
                            );
                            ui.selectable_value(
                                &mut self.transfer_proto,
                                crate::modem::FileTransferProtocol::XmodemCrc,
                                "XMODEM-CRC (128-byte blocks)",
                            );
                            ui.selectable_value(
                                &mut self.transfer_proto,
                                crate::modem::FileTransferProtocol::Xmodem,
                                "XMODEM (Standard Checksum)",
                            );
                        });
                });

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Path:");
                    let path_resp = ui.add(
                        egui::TextEdit::singleline(&mut self.transfer_path)
                            .desired_width(280.0)
                            .hint_text("/path/to/firmware.bin or ./downloads"),
                    );
                    path_resp.context_menu(|ui| {
                        text_edit_context_menu(ui, &mut self.transfer_path);
                    });
                });

                if let Some(status) = &self.transfer_status {
                    ui.add_space(4.0);
                    ui.colored_label(egui::Color32::from_rgb(100, 200, 255), status);
                }

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    if ui
                        .button("Receive File")
                        .on_hover_text("Receive file into directory")
                        .clicked()
                    {
                        let dir = if self.transfer_path.trim().is_empty() {
                            ".".to_string()
                        } else {
                            self.transfer_path.trim().to_string()
                        };
                        self.transfer_status = Some(format!(
                            "Waiting to receive via {:?} into '{dir}'...",
                            self.transfer_proto
                        ));
                        if let Err(e) = self.send_event(&MonitorEvent::ReceiveFile {
                            dir,
                            protocol: self.transfer_proto,
                        }) {
                            self.transfer_status = Some(format!("Transfer failed: {e}"));
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let send_btn = egui::Button::new(
                            egui::RichText::new("Send File")
                                .strong()
                                .color(egui::Color32::from_rgb(255, 255, 255)),
                        )
                        .min_size(egui::vec2(80.0, 24.0))
                        .fill(egui::Color32::from_rgb(0, 122, 255));

                        if ui
                            .add(send_btn)
                            .on_hover_text("Transmit file via selected protocol")
                            .clicked()
                        {
                            let path = self.transfer_path.trim().to_string();
                            if path.is_empty() {
                                self.transfer_status =
                                    Some("Error: please specify file path".to_string());
                            } else {
                                self.transfer_status = Some(format!(
                                    "Initiating {:?} send for '{}'...",
                                    self.transfer_proto, path
                                ));
                                if let Err(e) = self.send_event(&MonitorEvent::SendFile {
                                    path,
                                    protocol: self.transfer_proto,
                                }) {
                                    self.transfer_status = Some(format!("Transfer failed: {e}"));
                                }
                            }
                        }

                        ui.add_space(8.0);

                        if ui
                            .add(egui::Button::new("Cancel").min_size(egui::vec2(72.0, 24.0)))
                            .clicked()
                            || ui.input(|i| i.key_pressed(egui::Key::Escape))
                        {
                            close_requested = true;
                        }
                    });
                });
            });

        self.show_transfer_dialog = is_open && !close_requested;
    }

    fn render_about_dialog(
        &mut self,
        ctx: &egui::Context,
        icon_texture: Option<&egui::TextureHandle>,
    ) {
        let mut is_open = self.show_about_dialog;
        let mut close_requested = false;

        egui::Window::new("About devserial")
            .open(&mut is_open)
            .resizable(false)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .default_width(380.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    if let Some(texture) = icon_texture {
                        ui.add(egui::Image::new(texture).fit_to_exact_size(egui::vec2(72.0, 72.0)));
                        ui.add_space(8.0);
                    }

                    ui.heading(egui::RichText::new("devserial").strong().size(20.0));
                    ui.label(
                        egui::RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION")))
                            .color(egui::Color32::from_rgb(120, 180, 255))
                            .monospace(),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Modern MCP server & interactive serial terminal for LLMs, microcontrollers, and embedded developers.",
                        )
                        .size(12.0)
                        .color(egui::Color32::from_rgb(200, 200, 210)),
                    );
                });

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);

                egui::Grid::new("about_info_grid")
                    .num_columns(2)
                    .spacing([16.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("Author:").strong());
                        ui.label("Fabian Schmieder");
                        ui.end_row();

                        ui.label(egui::RichText::new("License:").strong());
                        ui.label("GNU General Public License v3.0");
                        ui.end_row();

                        ui.label(egui::RichText::new("Repository:").strong());
                        ui.hyperlink_to(
                            "github.com/metaneutrons/devserial",
                            "https://github.com/metaneutrons/devserial",
                        );
                        ui.end_row();
                    });

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    if ui.button("🌐 GitHub Repo").clicked() {
                        ctx.open_url(egui::OpenUrl::new_tab(
                            "https://github.com/metaneutrons/devserial",
                        ));
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let done_btn = egui::Button::new(
                            egui::RichText::new("Done")
                                .strong()
                                .color(egui::Color32::from_rgb(255, 255, 255)),
                        )
                        .min_size(egui::vec2(72.0, 24.0))
                        .fill(egui::Color32::from_rgb(0, 122, 255));

                        if ui.add(done_btn).clicked()
                            || ui.input(|i| {
                                i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Escape)
                            })
                        {
                            close_requested = true;
                        }
                    });
                });
            });

        self.show_about_dialog = is_open && !close_requested;
    }

    #[allow(clippy::too_many_lines)]
    fn render_export_dialog(&mut self, ctx: &egui::Context) {
        let mut is_open = self.show_export_dialog;
        let mut close_requested = false;
        let mut do_export = false;
        let mut do_copy = false;

        egui::Window::new(format!("Export Buffer — {}", self.port_name))
            .open(&mut is_open)
            .resizable(false)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .default_width(440.0)
            .show(ctx, |ui| {
                ui.heading(
                    egui::RichText::new("Export Serial Buffer")
                        .strong()
                        .size(16.0),
                );
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(
                        "Save captured serial data to a file or copy directly to clipboard.",
                    )
                    .size(11.5)
                    .color(egui::Color32::from_rgb(180, 180, 190)),
                );
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);

                // Scope selection
                ui.label(egui::RichText::new("Export Scope:").strong());
                ui.horizontal(|ui| {
                    ui.radio_value(
                        &mut self.export_scope,
                        ExportScope::AllDb,
                        "All SQLite Data",
                    );
                    ui.radio_value(
                        &mut self.export_scope,
                        ExportScope::VisibleBuffer,
                        "Display Buffer",
                    );
                    ui.radio_value(
                        &mut self.export_scope,
                        ExportScope::CustomRange,
                        "Line Range",
                    );
                });

                if self.export_scope == ExportScope::CustomRange {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label("From ID:");
                        let s_resp = ui.add(
                            egui::TextEdit::singleline(&mut self.export_start_line)
                                .desired_width(70.0),
                        );
                        s_resp.context_menu(|ui| {
                            text_edit_context_menu(ui, &mut self.export_start_line);
                        });
                        ui.label("To ID:");
                        let e_resp = ui.add(
                            egui::TextEdit::singleline(&mut self.export_end_line)
                                .desired_width(70.0)
                                .hint_text("max"),
                        );
                        e_resp.context_menu(|ui| {
                            text_edit_context_menu(ui, &mut self.export_end_line);
                        });
                    });
                }

                ui.add_space(8.0);

                // Format selection
                ui.label(egui::RichText::new("Format:").strong());
                ui.horizontal(|ui| {
                    if ui
                        .radio_value(&mut self.export_format, ExportFormat::Txt, "TXT (Plain)")
                        .clicked()
                    {
                        self.export_path =
                            export::with_format_extension(&self.export_path, ExportFormat::Txt);
                    }
                    if ui
                        .radio_value(&mut self.export_format, ExportFormat::Csv, "CSV (Table)")
                        .clicked()
                    {
                        self.export_path =
                            export::with_format_extension(&self.export_path, ExportFormat::Csv);
                    }
                    if ui
                        .radio_value(
                            &mut self.export_format,
                            ExportFormat::Jsonl,
                            "JSONL (Structured)",
                        )
                        .clicked()
                    {
                        self.export_path =
                            export::with_format_extension(&self.export_path, ExportFormat::Jsonl);
                    }
                });

                ui.add_space(8.0);

                // Destination path
                ui.label(egui::RichText::new("Destination File Path:").strong());
                ui.horizontal(|ui| {
                    let ep_resp = ui.add(
                        egui::TextEdit::singleline(&mut self.export_path)
                            .desired_width(300.0)
                            .font(egui::TextStyle::Monospace)
                            .hint_text("devserial_export.txt"),
                    );
                    ep_resp.context_menu(|ui| {
                        text_edit_context_menu(ui, &mut self.export_path);
                    });
                    if ui.button("Default").clicked() {
                        let sanitized_port = crate::paths::sanitize_port_name(&self.port_name);
                        let ext = match self.export_format {
                            ExportFormat::Txt => "txt",
                            ExportFormat::Csv => "csv",
                            ExportFormat::Jsonl => "jsonl",
                        };
                        let now = chrono::Local::now().format("%Y%m%d_%H%M%S");
                        self.export_path = format!("devserial_export_{sanitized_port}_{now}.{ext}");
                    }
                });

                if let Some((status_text, is_err)) = &self.export_status {
                    ui.add_space(6.0);
                    let color = if *is_err {
                        egui::Color32::from_rgb(255, 90, 90)
                    } else {
                        egui::Color32::from_rgb(80, 220, 120)
                    };
                    ui.colored_label(color, status_text);
                }

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    if ui
                        .button("📋 Copy to Clipboard")
                        .on_hover_text("Copy formatted export data directly to clipboard")
                        .clicked()
                    {
                        do_copy = true;
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let primary_btn = egui::Button::new(
                            egui::RichText::new("💾 Export to File")
                                .strong()
                                .color(egui::Color32::from_rgb(255, 255, 255)),
                        )
                        .min_size(egui::vec2(84.0, 24.0))
                        .fill(egui::Color32::from_rgb(0, 122, 255));

                        if ui
                            .add(primary_btn)
                            .on_hover_text("Write data to specified file path")
                            .clicked()
                            || ui.input(|i| i.key_pressed(egui::Key::Enter))
                        {
                            do_export = true;
                        }

                        ui.add_space(8.0);

                        if ui
                            .add(egui::Button::new("Cancel").min_size(egui::vec2(72.0, 24.0)))
                            .clicked()
                            || ui.input(|i| i.key_pressed(egui::Key::Escape))
                        {
                            close_requested = true;
                        }
                    });
                });
            });

        if do_export {
            self.execute_export_to_file();
        } else if do_copy {
            self.execute_export_to_clipboard(ctx);
        }

        self.show_export_dialog = is_open && !close_requested;
    }

    /// Collect the lines the current export scope covers.
    fn get_export_lines(&self) -> Result<Vec<crate::storage::StoredLine>, String> {
        match self.export_scope {
            ExportScope::AllDb => {
                let total = self
                    .storage
                    .line_count()
                    .map_err(|e| format!("database error: {e}"))?;
                let count = u32::try_from(total.min(u64::from(MAX_EXPORT_LINES))).unwrap_or(1);
                self.storage
                    .read_lines(1, count.max(1))
                    .map_err(|e| format!("database read error: {e}"))
            }
            ExportScope::VisibleBuffer => Ok(self
                .lines
                .iter()
                .enumerate()
                .map(|(index, line)| crate::storage::StoredLine {
                    id: i64::try_from(index + 1).unwrap_or(i64::MAX),
                    timestamp_ns: line.timestamp_ns,
                    payload: line.payload.clone(),
                })
                .collect()),
            ExportScope::CustomRange => {
                let start: i64 = self
                    .export_start_line
                    .trim()
                    .parse()
                    .map_err(|_| "invalid start ID".to_string())?;
                let end: i64 = if self.export_end_line.trim().is_empty() {
                    i64::MAX
                } else {
                    self.export_end_line
                        .trim()
                        .parse()
                        .map_err(|_| "invalid end ID".to_string())?
                };
                if end < start {
                    return Err("end ID is before start ID".to_string());
                }
                let span = end.saturating_sub(start).saturating_add(1);
                let count = u32::try_from(span).unwrap_or(MAX_EXPORT_LINES);
                self.storage
                    .read_lines(start, count.min(MAX_EXPORT_LINES))
                    .map_err(|e| format!("database read error: {e}"))
            }
        }
    }

    /// Render the selected lines in the shared export format.
    fn format_export_data(&self, lines: &[crate::storage::StoredLine]) -> String {
        export::to_string(lines, self.export_format).unwrap_or_default()
    }

    fn execute_export_to_file(&mut self) {
        if self.export_path.trim().is_empty() {
            self.export_status = Some(("Please specify a destination file path".to_string(), true));
            return;
        }

        match self.get_export_lines() {
            Ok(lines) => {
                let count = lines.len();
                let data = self.format_export_data(&lines);
                let path = std::path::Path::new(&self.export_path);
                match std::fs::write(path, data.as_bytes()) {
                    Ok(()) => {
                        let size_kb = data.len() / 1024;
                        self.export_status = Some((
                            format!(
                                "✓ Exported {count} lines ({size_kb} KB) to {}",
                                path.display()
                            ),
                            false,
                        ));
                    }
                    Err(e) => {
                        self.export_status = Some((format!("Export failed: {e}"), true));
                    }
                }
            }
            Err(e) => {
                self.export_status = Some((format!("Failed to query lines: {e}"), true));
            }
        }
    }

    fn execute_export_to_clipboard(&mut self, ctx: &egui::Context) {
        match self.get_export_lines() {
            Ok(lines) => {
                let count = lines.len();
                let data = self.format_export_data(&lines);
                ctx.copy_text(data);
                self.export_status = Some((format!("✓ Copied {count} lines to clipboard"), false));
            }
            Err(e) => {
                self.export_status = Some((format!("Failed to query lines: {e}"), true));
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn render_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            // Group 1: Connection status indicator & Disconnect/Connect toggle
            let (status_color, status_label) = if self.connected {
                (egui::Color32::from_rgb(80, 200, 80), "Connected")
            } else {
                (egui::Color32::from_rgb(200, 60, 60), "Disconnected")
            };
            ui.colored_label(status_color, format!("● {status_label}"));

            if self.connected {
                if ui
                    .button("⏏ Disconnect")
                    .on_hover_text("Release serial port hardware handle (Cmd+K)")
                    .clicked()
                {
                    self.toggle_connection();
                }
            } else if ui
                .button("🔌 Connect")
                .on_hover_text("Reconnect to serial port (Cmd+K)")
                .clicked()
            {
                self.toggle_connection();
            }
            ui.separator();

            // Group 2: New Window & Port Settings
            if ui
                .button("➕ New Window...")
                .on_hover_text("Open another serial port in a new window (Cmd+N)")
                .clicked()
            {
                self.open_port_dialog_requested = true;
            }

            if ui
                .button("⚙ Settings ▾")
                .on_hover_text("Configure baud rate and serial parameters (Cmd+Shift+P)")
                .clicked()
            {
                self.show_settings_dialog = !self.show_settings_dialog;
            }
            ui.separator();

            // Group 3: Hardware Macros
            if ui
                .button("Reset")
                .on_hover_text("DTR toggle reset")
                .clicked()
            {
                self.run_macro("reset");
            }
            if ui
                .button("Bootloader")
                .on_hover_text("Enter bootloader mode")
                .clicked()
            {
                self.run_macro("enter_bootloader");
            }
            if ui
                .button("Break")
                .on_hover_text("Send an RS-232 BREAK pulse")
                .clicked()
            {
                self.request(&MonitorEvent::Break { duration_ms: None });
            }
            ui.separator();

            // Group 4: DTR / RTS live toggles
            if ui.toggle_value(&mut self.dtr_state, "DTR").changed() {
                self.request(&MonitorEvent::Signal {
                    dtr: Some(self.dtr_state),
                    rts: None,
                });
            }
            if ui.toggle_value(&mut self.rts_state, "RTS").changed() {
                self.request(&MonitorEvent::Signal {
                    dtr: None,
                    rts: Some(self.rts_state),
                });
            }
            ui.separator();

            // Group 5: View toggles & Clear/Pause
            ui.checkbox(&mut self.show_timestamps, "Time");
            ui.checkbox(&mut self.hex_view, "Hex");
            ui.separator();

            if ui.button("Clear").on_hover_text("Clear display").clicked() {
                self.lines.clear();
                self.filtered_indices.clear();
            }

            let pause_label = if self.paused { "Resume" } else { "Pause" };
            if ui.button(pause_label).clicked() {
                self.paused = !self.paused;
            }
            ui.separator();

            // Group 6: Transfer & Export Dialogs
            if ui
                .button("Transfer ▾")
                .on_hover_text("File transfer (ZMODEM / YMODEM / XMODEM)")
                .clicked()
            {
                self.show_transfer_dialog = !self.show_transfer_dialog;
            }

            if ui
                .button("Export ▾")
                .on_hover_text("Export buffer to file (TXT / CSV / JSONL) or clipboard")
                .clicked()
            {
                self.show_export_dialog = !self.show_export_dialog;
                if self.export_path.is_empty() {
                    let sanitized_port = crate::paths::sanitize_port_name(&self.port_name);
                    let now = chrono::Local::now().format("%Y%m%d_%H%M%S");
                    self.export_path = format!("devserial_export_{sanitized_port}_{now}.txt");
                }
            }
            ui.separator();

            // Group 7: Instant Filter
            ui.label("Filter:");
            let filter_id = egui::Id::new(format!("filter_text_{}", self.port_name));
            let filter_response = ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .id(filter_id)
                    .desired_width(130.0)
                    .font(egui::TextStyle::Monospace)
                    .hint_text("grep..."),
            );
            filter_response.context_menu(|ui| {
                text_edit_context_menu(ui, &mut self.filter);
            });
            if filter_response.changed() {
                self.filter_active = !self.filter.is_empty();
                self.rebuild_filter();
            }

            ui.separator();
            if ui
                .button("ℹ About")
                .on_hover_text("About devserial (Author, Version, Repository)")
                .clicked()
            {
                self.show_about_dialog = true;
            }
        });
    }

    #[allow(clippy::too_many_lines)]
    fn render_buffer(&mut self, ui: &mut egui::Ui) {
        let text_style = egui::TextStyle::Monospace;
        let row_height = ui.text_style_height(&text_style) + 2.0;

        let indices: Vec<usize> = if self.filter_active {
            self.filtered_indices.clone()
        } else {
            (0..self.lines.len()).collect()
        };
        let total_rows = indices.len();

        let scroll = egui::ScrollArea::vertical()
            .auto_shrink(false)
            .stick_to_bottom(self.auto_follow);

        let lines_not_empty = !self.lines.is_empty();
        let mut clear_requested = false;
        let mut copy_all_requested = false;

        let response = scroll.show_rows(ui, row_height, total_rows, |ui, row_range| {
            for row_idx in row_range {
                let Some(&line_idx) = indices.get(row_idx) else {
                    continue;
                };
                let Some(line) = self.lines.get(line_idx) else {
                    continue;
                };
                ui.horizontal(|ui| {
                    if self.show_timestamps {
                        ui.colored_label(egui::Color32::from_rgb(110, 110, 110), &line.timestamp);
                        ui.add_space(6.0);
                    }
                    let resp = if line.is_sent {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!(">> {}", line.payload))
                                    .color(egui::Color32::from_rgb(90, 190, 255))
                                    .monospace(),
                            )
                            .selectable(true),
                        )
                    } else if self.hex_view {
                        // Rendered from the payload itself. Keeping a second
                        // byte copy per line doubled the memory of the display
                        // buffer for the same content.
                        let hex = crate::hex::format_dump(line.payload.as_bytes());
                        ui.add(
                            egui::Label::new(egui::RichText::new(hex).monospace()).selectable(true),
                        )
                    } else {
                        ui.add(egui::Label::new(colorize_label(&line.payload)).selectable(true))
                    };

                    resp.context_menu(|ui| {
                        if ui
                            .add_enabled(
                                !line.payload.is_empty(),
                                egui::Button::new("📋 Copy Line").shortcut_text("Cmd+C"),
                            )
                            .clicked()
                        {
                            ui.ctx().copy_text(line.payload.clone());
                            ui.close();
                        }

                        if ui
                            .add_enabled(
                                lines_not_empty,
                                egui::Button::new("📄 Copy Entire Buffer").shortcut_text("Cmd+A"),
                            )
                            .clicked()
                        {
                            copy_all_requested = true;
                            ui.close();
                        }

                        ui.separator();

                        if ui
                            .add_enabled(lines_not_empty, egui::Button::new("⌫ Clear Display"))
                            .clicked()
                        {
                            clear_requested = true;
                            ui.close();
                        }
                    });
                });
            }
        });

        if clear_requested {
            self.lines.clear();
            self.filtered_indices.clear();
        }

        if copy_all_requested && !self.lines.is_empty() {
            let all_text = self
                .lines
                .iter()
                .map(|l| l.payload.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            ui.ctx().copy_text(all_text);
        }

        // Auto-follow logic
        #[allow(clippy::cast_precision_loss)]
        let content_height = total_rows as f32 * row_height;
        let viewport_height = response.inner_rect.height();
        let max_offset = (content_height - viewport_height).max(0.0);
        let at_bottom = response.state.offset.y >= max_offset - row_height;

        // If user is actively clicking or dragging to select text within the buffer, pause auto-follow
        let user_selecting_in_buffer = ui.input(|i| {
            i.pointer.interact_pos().is_some_and(|pos| {
                (i.pointer.button_down(egui::PointerButton::Primary)
                    || i.pointer.is_decidedly_dragging())
                    && response.inner_rect.contains(pos)
            })
        });

        self.is_selecting_in_buffer = user_selecting_in_buffer;

        // Keyboard shortcuts when focusing buffer (and not inside an editable TextEdit)
        let focused_id = ui.ctx().memory(egui::Memory::focused);
        let is_in_text_edit = focused_id
            .is_some_and(|fid| egui::text_edit::TextEditState::load(ui.ctx(), fid).is_some());

        if !is_in_text_edit {
            // Buffer Select All (Cmd+A): copy entire buffer to clipboard
            if ui
                .input(|i| i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::A))
                && !self.lines.is_empty()
            {
                let all_text = self
                    .lines
                    .iter()
                    .map(|l| l.payload.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.ctx().copy_text(all_text);
            }

            // Buffer Copy (Cmd+C): if no specific mouse selection, copy latest line
            if ui
                .input(|i| i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::C))
                && !user_selecting_in_buffer
                && !self.lines.is_empty()
            {
                if let Some(last) = self.lines.back() {
                    ui.ctx().copy_text(last.payload.clone());
                }
            }
        }

        if user_selecting_in_buffer && self.auto_follow {
            // Pause auto-follow immediately when selecting text
            self.auto_follow = false;
        } else if at_bottom && !self.auto_follow && !user_selecting_in_buffer {
            // User scrolled back to bottom and released mouse → re-enable
            self.auto_follow = true;
        } else if !at_bottom && self.auto_follow && ui.input(|i| i.smooth_scroll_delta.y != 0.0) {
            // User scrolled away from bottom → disable
            self.auto_follow = false;
        }
    }

    fn render_input(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let input_id = egui::Id::new(format!("input_text_{}", self.port_name));
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.input)
                    .id(input_id)
                    .desired_width(ui.available_width() - 130.0)
                    .font(egui::TextStyle::Monospace)
                    .hint_text("Send data... (Up/Down for history)"),
            );

            response.context_menu(|ui| {
                text_edit_context_menu(ui, &mut self.input);
            });

            egui::ComboBox::from_id_salt(format!("line_ending_{}", self.port_name))
                .selected_text(self.line_ending.label())
                .width(55.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.line_ending, LineEnding::CrLf, "CRLF");
                    ui.selectable_value(&mut self.line_ending, LineEnding::Lf, "LF");
                    ui.selectable_value(&mut self.line_ending, LineEnding::Cr, "CR");
                    ui.selectable_value(&mut self.line_ending, LineEnding::None, "None");
                });

            if response.has_focus() {
                if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                    self.history_up();
                } else if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                    self.history_down();
                }
            }

            let send = ui.button("Send").clicked()
                || (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));

            if send && !self.input.is_empty() {
                self.send_input();
                response.request_focus();
            }
        });
    }

    fn render_status(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(&self.port_name)
                    .strong()
                    .color(egui::Color32::from_rgb(140, 200, 255)),
            );
            ui.label(
                egui::RichText::new(format!("({})", self.port_info))
                    .color(egui::Color32::from_rgb(160, 160, 160)),
            );

            ui.separator();
            ui.label(format!("Lines: {}", self.lines.len()));

            if self.filter_active {
                ui.separator();
                ui.colored_label(
                    egui::Color32::from_rgb(255, 200, 100),
                    format!("Filtered: {} matches", self.filtered_indices.len()),
                );
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.checkbox(&mut self.auto_follow, "Auto-follow");
            });
        });
    }

    /// Ask the owning process to run a macro.
    fn run_macro(&mut self, name: &str) {
        self.request(&MonitorEvent::Macro {
            name: name.to_string(),
        });
    }

    /// Send an event and surface a failure in the status line.
    fn request(&mut self, event: &MonitorEvent) {
        if let Err(e) = self.send_event(event) {
            self.settings_status = Some((e, true));
        }
    }

    /// Report a user action to the process that owns the port.
    ///
    /// Windows with a direct port handle act on their own; windows driven by a
    /// parent process forward the typed event over stdout.
    fn send_event(&self, event: &MonitorEvent) -> Result<(), String> {
        if self.direct_port.is_some() {
            return Err("this window owns the port and handles actions directly".to_string());
        }
        let line = event.encode().map_err(|e| e.to_string())?;
        let mut out = std::io::stdout().lock();
        writeln!(out, "{line}").map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())
    }

    fn send_input(&mut self) {
        let input = self.input.trim().to_string();
        if input.is_empty() {
            return;
        }

        if self.history.last().is_none_or(|last| *last != input) {
            self.history.push(input.clone());
            self.storage.append_send_history(&input).ok();
        }
        self.history_idx = None;
        self.history_draft.clear();

        let data = if crate::hex::has_prefix(&input) {
            match crate::hex::decode(&input) {
                Ok(bytes) => bytes,
                Err(e) => {
                    self.settings_status = Some((format!("Invalid hex input: {e}"), true));
                    return;
                }
            }
        } else {
            let mut bytes = input.as_bytes().to_vec();
            bytes.extend_from_slice(self.line_ending.suffix());
            bytes
        };

        // Windows that own the port write directly; the others report the
        // action to the process that does.
        if let Some(ref port) = self.direct_port {
            if let Ok(mut port) = port.lock() {
                let _ = port.write_all(&data);
                let _ = port.flush();
            }
        } else if let Err(e) = self.send_event(&MonitorEvent::Write {
            hex: crate::hex::encode(&data),
        }) {
            self.settings_status = Some((e, true));
            return;
        }

        let timestamp_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        self.push_display_line(DisplayLine {
            timestamp: chrono::Utc::now().format("%H:%M:%S%.3f").to_string(),
            timestamp_ns,
            payload: input,
            is_sent: true,
        });
        if self.filter_active {
            self.rebuild_filter();
        }

        self.input.clear();
    }

    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_idx {
            None => {
                self.history_draft = self.input.clone();
                self.history_idx = Some(self.history.len() - 1);
                self.input.clone_from(&self.history[self.history.len() - 1]);
            }
            Some(0) => {}
            Some(idx) => {
                self.history_idx = Some(idx - 1);
                self.input.clone_from(&self.history[idx - 1]);
            }
        }
    }

    fn history_down(&mut self) {
        match self.history_idx {
            None => {}
            Some(idx) => {
                if idx + 1 >= self.history.len() {
                    self.history_idx = None;
                    self.input.clone_from(&self.history_draft);
                } else {
                    self.history_idx = Some(idx + 1);
                    self.input.clone_from(&self.history[idx + 1]);
                }
            }
        }
    }

    fn rebuild_filter(&mut self) {
        if !self.filter_active || self.filter.is_empty() {
            self.filtered_indices.clear();
            return;
        }

        let is_regex =
            self.filter.starts_with('/') && self.filter.ends_with('/') && self.filter.len() > 2;
        let re = if is_regex {
            regex::Regex::new(&self.filter[1..self.filter.len() - 1]).ok()
        } else {
            None
        };

        let filter_lower = self.filter.to_lowercase();

        self.filtered_indices = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                re.as_ref().map_or_else(
                    || line.payload.to_lowercase().contains(&filter_lower),
                    |r| r.is_match(&line.payload),
                )
            })
            .map(|(idx, _)| idx)
            .collect();
    }
}

// --- Helpers ---

fn colorize_label(text: &str) -> egui::RichText {
    if text.contains("[ERROR]") || text.contains("[PANIC]") || text.contains("error") {
        egui::RichText::new(text)
            .monospace()
            .color(egui::Color32::from_rgb(255, 85, 85))
    } else if text.contains("[WARN]") || text.contains("warning") {
        egui::RichText::new(text)
            .monospace()
            .color(egui::Color32::from_rgb(255, 200, 60))
    } else if text.contains("[DEBUG]") {
        egui::RichText::new(text)
            .monospace()
            .color(egui::Color32::from_rgb(130, 130, 130))
    } else if text.contains("[INFO]") {
        egui::RichText::new(text)
            .monospace()
            .color(egui::Color32::from_rgb(200, 200, 200))
    } else if text.contains("RECONFIGURED") {
        egui::RichText::new(text)
            .monospace()
            .color(egui::Color32::from_rgb(100, 220, 255))
    } else {
        egui::RichText::new(text).monospace()
    }
}

/// Decode the embedded application icon.
///
/// A broken embedded asset would be a build error, so an unusable icon degrades
/// to no icon instead of taking the window down.
fn load_icon() -> egui::IconData {
    match image::load_from_memory(crate::assets::ICON_PNG) {
        Ok(image) => {
            let image = image.to_rgba8();
            let (width, height) = image.dimensions();
            egui::IconData {
                rgba: image.into_raw(),
                width,
                height,
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "embedded icon could not be decoded");
            egui::IconData {
                rgba: Vec::new(),
                width: 0,
                height: 0,
            }
        }
    }
}
