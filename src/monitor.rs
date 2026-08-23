// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Native GUI serial monitor window using egui/eframe.
//!
//! On macOS, GUI windows must run on the main thread. Since our MCP server
//! occupies the main thread with tokio, the monitor runs as a **child process**
//! of the same binary invoked with `--monitor <port> --db <path>`.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui;

/// Maximum lines kept in the display buffer.
const MAX_DISPLAY_LINES: usize = 100_000;

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

/// Entry point for the monitor subprocess.
///
/// # Errors
/// Returns error if the database cannot be opened or the window fails to initialize.
///
/// # Panics
/// Panics if the mutex for the egui context cannot be locked (should not happen in practice).
pub fn run_monitor(port_name: &str, db_path: &Path, port_info: &str) -> Result<(), String> {
    run_monitor_inner(port_name, db_path, port_info, None)
}

/// Run monitor with a direct write handle (for standalone mode).
///
/// # Errors
/// Returns error if the database cannot be opened or the window fails.
///
/// # Panics
/// Panics if the mutex for the egui context cannot be locked.
pub fn run_monitor_with_port(
    port_name: &str,
    db_path: &Path,
    port_info: &str,
    write_port: Box<dyn std::io::Write + Send>,
) -> Result<(), String> {
    run_monitor_inner(
        port_name,
        db_path,
        port_info,
        Some(Arc::new(std::sync::Mutex::new(write_port))),
    )
}

fn run_monitor_inner(
    port_name: &str,
    db_path: &Path,
    port_info: &str,
    direct_port: Option<Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>>,
) -> Result<(), String> {
    // Stdin pipe thread: reads notification bytes from server, EOF = parent died
    // The egui context will be set after eframe starts (via Arc)
    let ctx_holder: Arc<std::sync::Mutex<Option<egui::Context>>> =
        Arc::new(std::sync::Mutex::new(None));
    let ctx_for_thread = Arc::clone(&ctx_holder);

    let storage = crate::storage::SqliteStorage::open(db_path)
        .map_err(|e| format!("failed to open DB: {e}"))?;

    let history = storage.load_send_history(500).unwrap_or_default();

    let app = MonitorApp {
        port_name: port_name.to_string(),
        port_info: port_info.to_string(),
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
        last_id: i64::MAX, // will be reset on first poll
        history,
        history_idx: None,
        history_draft: String::new(),
        connected: true,
        direct_port,
        show_transfer_dialog: false,
        transfer_path: String::new(),
        transfer_proto: crate::modem::FileTransferProtocol::Zmodem,
        transfer_status: None,
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!(
                "devserial {} — {port_name} — {port_info}",
                env!("CARGO_PKG_VERSION")
            ))
            .with_icon(load_icon())
            .with_inner_size([900.0, 600.0])
            .with_min_inner_size([500.0, 350.0]),
        ..Default::default()
    };

    eframe::run_native(
        &format!("Serial Monitor — {port_name}"),
        options,
        Box::new(|cc| {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "jetbrains_mono".to_owned(),
                std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
                    "../resources/fonts/JetBrainsMono-Regular.ttf"
                ))),
            );
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, "jetbrains_mono".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "jetbrains_mono".to_owned());
            cc.egui_ctx.set_fonts(fonts);

            // Dark theme with subtle styling
            let mut style = (*cc.egui_ctx.global_style()).clone();
            style.spacing.item_spacing = egui::vec2(8.0, 4.0);
            style.spacing.button_padding = egui::vec2(8.0, 4.0);
            cc.egui_ctx.set_global_style(style);

            // Start stdin notification thread (reads bytes = repaint, EOF = exit)
            {
                let mut guard = ctx_for_thread.lock().unwrap();
                *guard = Some(cc.egui_ctx.clone());
            }
            std::thread::spawn(move || {
                use std::io::Read;
                let mut buf = [0u8; 64];
                let stdin = std::io::stdin();
                loop {
                    let n = {
                        let mut guard = stdin.lock();
                        guard.read(&mut buf).unwrap_or(0)
                    };
                    if n == 0 {
                        // EOF or error = parent died
                        std::process::exit(0);
                    }
                    // Notification: new data available
                    if let Some(ctx) = ctx_holder.lock().ok().and_then(|g| g.clone()) {
                        ctx.request_repaint();
                    }
                }
            });

            Ok(Box::new(app))
        }),
    )
    .map_err(|e| format!("eframe error: {e}"))
}

// --- Internal types ---

#[derive(Clone)]
struct DisplayLine {
    timestamp: String,
    payload: String,
    raw_bytes: Option<Vec<u8>>,
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

// --- App state ---

#[allow(clippy::struct_excessive_bools)]
struct MonitorApp {
    port_name: String,
    port_info: String,
    storage: crate::storage::SqliteStorage,
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
    /// Direct write handle for standalone mode (None = use stdout pipe for MCP mode)
    direct_port: Option<Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>>,
    show_transfer_dialog: bool,
    transfer_path: String,
    transfer_proto: crate::modem::FileTransferProtocol,
    transfer_status: Option<String>,
}

// --- eframe::App ---

impl eframe::App for MonitorApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.paused {
            // First poll: get the latest lines
            if self.last_id == i64::MAX {
                if let Ok(stats) = self.storage.get_stats() {
                    self.last_id = i64::try_from(stats.total_lines).unwrap_or(0);
                }
            }

            if let Ok(new_lines) = self.storage.read_lines(self.last_id + 1, 500) {
                for line in &new_lines {
                    let ts = chrono::DateTime::from_timestamp_nanos(line.timestamp_ns)
                        .format("%H:%M:%S%.3f")
                        .to_string();
                    self.lines.push_back(DisplayLine {
                        timestamp: ts,
                        payload: line.payload.clone(),
                        raw_bytes: Some(line.payload.as_bytes().to_vec()),
                        is_sent: false,
                    });
                    self.last_id = line.id;
                }
                if !new_lines.is_empty() {
                    self.rebuild_filter();
                }
            }
        }

        while self.lines.len() > MAX_DISPLAY_LINES {
            self.lines.pop_front();
            // Shift filtered indices
            self.filtered_indices.retain_mut(|idx| {
                if *idx == 0 {
                    false
                } else {
                    *idx -= 1;
                    true
                }
            });
        }

        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Toolbar (top)
        egui::Panel::top("toolbar").show_inside(ui, |ui| {
            self.render_toolbar(ui);
        });

        // Input bar (bottom)
        egui::Panel::bottom("input_panel").show_inside(ui, |ui| {
            self.render_input(ui);
        });

        // Status bar (bottom)
        egui::Panel::bottom("status_bar")
            .max_size(22.0)
            .show_inside(ui, |ui| {
                self.render_status(ui);
            });

        // Main buffer (center)
        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.render_buffer(ui);
        });

        if self.show_transfer_dialog {
            self.render_transfer_dialog(ui.ctx());
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {}
}

// --- Rendering ---

impl MonitorApp {
    #[allow(clippy::too_many_lines)]
    fn render_transfer_dialog(&mut self, ctx: &egui::Context) {
        let mut is_open = self.show_transfer_dialog;
        let mut close_requested = false;

        egui::Window::new("File Transfer (X/Y/Z-Modem)")
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
                    egui::ComboBox::from_id_salt("transfer_proto_combo")
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
                    ui.add(
                        egui::TextEdit::singleline(&mut self.transfer_path)
                            .desired_width(280.0)
                            .hint_text("/path/to/firmware.bin or ./downloads"),
                    );
                });

                if let Some(status) = &self.transfer_status {
                    ui.add_space(4.0);
                    ui.colored_label(egui::Color32::from_rgb(100, 200, 255), status);
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .button("Send File")
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
                            Self::send_command(&format!(
                                "__transfer:send:{:?}:{path}",
                                self.transfer_proto
                            ));
                        }
                    }

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
                        Self::send_command(&format!(
                            "__transfer:recv:{:?}:{dir}",
                            self.transfer_proto
                        ));
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            close_requested = true;
                        }
                    });
                });
            });

        self.show_transfer_dialog = is_open && !close_requested;
    }
    fn render_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            // Connection indicator
            let (color, label) = if self.connected {
                (egui::Color32::from_rgb(80, 200, 80), "Connected")
            } else {
                (egui::Color32::from_rgb(200, 60, 60), "Disconnected")
            };
            ui.colored_label(color, format!("● {label}"));
            ui.separator();

            // Macro buttons
            if ui
                .button("Reset")
                .on_hover_text("DTR toggle reset")
                .clicked()
            {
                Self::send_command("__macro:reset");
            }
            if ui
                .button("Bootloader")
                .on_hover_text("Enter bootloader mode")
                .clicked()
            {
                Self::send_command("__macro:enter_bootloader");
            }
            if ui
                .button("Break")
                .on_hover_text("Send RS-232 serial BREAK pulse (250ms)")
                .clicked()
            {
                Self::send_command("__signal:break");
            }
            ui.separator();

            // DTR / RTS toggles
            if ui.toggle_value(&mut self.dtr_state, "DTR").changed() {
                Self::send_command(if self.dtr_state {
                    "__signal:dtr:1"
                } else {
                    "__signal:dtr:0"
                });
            }
            if ui.toggle_value(&mut self.rts_state, "RTS").changed() {
                Self::send_command(if self.rts_state {
                    "__signal:rts:1"
                } else {
                    "__signal:rts:0"
                });
            }
            ui.separator();

            // View toggles
            ui.checkbox(&mut self.show_timestamps, "Time");
            ui.checkbox(&mut self.hex_view, "Hex");
            ui.separator();

            // Clear button
            if ui.button("Clear").on_hover_text("Clear display").clicked() {
                self.lines.clear();
                self.filtered_indices.clear();
            }

            // Pause button
            let pause_label = if self.paused { "Resume" } else { "Pause" };
            if ui.button(pause_label).clicked() {
                self.paused = !self.paused;
            }
            ui.separator();

            // Transfer dialog button
            if ui
                .button("Transfer ▾")
                .on_hover_text("File transfer (ZMODEM / YMODEM / XMODEM)")
                .clicked()
            {
                self.show_transfer_dialog = !self.show_transfer_dialog;
            }
            ui.separator();

            // Filter
            ui.label("Filter:");
            let filter_response = ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .desired_width(150.0)
                    .font(egui::TextStyle::Monospace)
                    .hint_text("grep..."),
            );
            if filter_response.changed() {
                self.filter_active = !self.filter.is_empty();
                self.rebuild_filter();
            }
        });
    }

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
                    if line.is_sent {
                        ui.colored_label(
                            egui::Color32::from_rgb(90, 190, 255),
                            format!(">> {}", line.payload),
                        );
                    } else if self.hex_view {
                        let hex = line
                            .raw_bytes
                            .as_ref()
                            .map_or_else(String::new, |b| hex_format(b));
                        ui.label(egui::RichText::new(hex).monospace());
                    } else {
                        ui.label(colorize_label(&line.payload));
                    }
                });
            }
        });

        // Auto-follow logic
        #[allow(clippy::cast_precision_loss)]
        let content_height = total_rows as f32 * row_height;
        let viewport_height = response.inner_rect.height();
        let max_offset = (content_height - viewport_height).max(0.0);
        let at_bottom = response.state.offset.y >= max_offset - row_height;

        if at_bottom && !self.auto_follow {
            // User scrolled/dragged to bottom → re-enable
            self.auto_follow = true;
        } else if !at_bottom && self.auto_follow && ui.input(|i| i.smooth_scroll_delta.y != 0.0) {
            // User scrolled away from bottom → disable
            self.auto_follow = false;
        }
    }

    fn render_input(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.input)
                    .desired_width(ui.available_width() - 130.0)
                    .font(egui::TextStyle::Monospace)
                    .hint_text("Send data... (Up/Down for history)"),
            );

            egui::ComboBox::from_id_salt("line_ending")
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
            ui.checkbox(&mut self.auto_follow, "Auto-follow");
            ui.separator();
            ui.label(format!("Lines: {}", self.lines.len()));
            if self.filter_active {
                ui.label(format!("(showing {})", self.filtered_indices.len()));
            }
            ui.separator();
            ui.label(&self.port_name);
            ui.label(&self.port_info);
        });
    }

    // --- Logic ---

    fn rebuild_filter(&mut self) {
        if self.filter.is_empty() {
            self.filter_active = false;
            self.filtered_indices.clear();
            return;
        }
        let query = self.filter.to_lowercase();
        self.filtered_indices = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.payload.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect();
    }

    fn send_command(cmd: &str) {
        use std::io::Write;
        // Write to stdout — server reads this pipe
        let mut out = std::io::stdout().lock();
        writeln!(out, "{cmd}").ok();
        out.flush().ok();
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

        let data = if input.starts_with("0x") || input.starts_with("0X") {
            hex_decode_lossy(&input[2..])
        } else {
            let mut bytes = input.as_bytes().to_vec();
            bytes.extend_from_slice(self.line_ending.suffix());
            bytes
        };

        // Send via direct port (standalone) or stdout pipe (MCP mode)
        if let Some(ref port) = self.direct_port {
            if let Ok(mut p) = port.lock() {
                p.write_all(&data).ok();
                p.flush().ok();
            }
        } else {
            use std::io::Write;
            let hex = data.iter().fold(String::new(), |mut acc, b| {
                use std::fmt::Write;
                let _ = write!(acc, "{b:02x}");
                acc
            });
            let mut out = std::io::stdout().lock();
            writeln!(out, "__data:{hex}").ok();
            out.flush().ok();
        }

        let ts = chrono::Utc::now().format("%H:%M:%S%.3f").to_string();
        self.lines.push_back(DisplayLine {
            timestamp: ts,
            payload: input,
            raw_bytes: None,
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
    } else {
        egui::RichText::new(text).monospace()
    }
}

fn hex_format(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .chunks(16)
        .map(|chunk| chunk.join(" "))
        .collect::<Vec<_>>()
        .join("  ")
}

fn hex_decode_lossy(s: &str) -> Vec<u8> {
    let s = s.replace(' ', "");
    (0..s.len())
        .step_by(2)
        .filter_map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
        .collect()
}

fn load_icon() -> egui::IconData {
    let png_bytes = include_bytes!("../resources/icon.png");
    let img = image::load_from_memory(png_bytes)
        .expect("embedded icon.png is valid")
        .to_rgba8();
    let (w, h) = img.dimensions();
    egui::IconData {
        rgba: img.into_raw(),
        width: w,
        height: h,
    }
}
