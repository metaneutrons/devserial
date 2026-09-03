// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Terminal monitor built on ratatui and crossterm.
//!
//! The terminal is restored by a guard and by a panic hook, so a crash cannot
//! leave the user with an unusable shell.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};

use crate::cli::CliError;
use crate::cli::output::authors;
use crate::config::PortConfig;
use crate::modem::FileTransferProtocol;
use crate::port_manager::SerialPortHandle;
use crate::serial_params::{
    BAUD_PRESETS, DEFAULT_BREAK_MS, DataBits, FlowControl, Parity, StopBits,
};
use crate::storage::SqliteStorage;

/// Lines kept in the visible buffer. Older lines stay in the database.
const MAX_VISIBLE_LINES: usize = 50_000;
/// Lines fetched from storage per refresh.
const FETCH_BATCH: u32 = 500;
/// Status messages disappear after this long.
const STATUS_TTL: Duration = Duration::from_secs(5);

/// Restores the terminal when it goes out of scope.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, CliError> {
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}

/// Leave the alternate screen before a panic message is printed.
fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = io::stdout().execute(LeaveAlternateScreen);
            previous(info);
        }));
    });
}

/// Run the TUI monitor.
///
/// # Errors
/// Returns an error on terminal or I/O failure.
pub fn run_tui(
    port_name: &str,
    config: &PortConfig,
    storage: &Arc<std::sync::Mutex<SqliteStorage>>,
    write_port: &SerialPortHandle,
    runtime: &tokio::runtime::Handle,
) -> Result<(), CliError> {
    install_panic_hook();
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    run_app(
        &mut terminal,
        port_name,
        config,
        storage,
        write_port,
        runtime,
    )
}

#[derive(PartialEq, Eq)]
enum InputMode {
    Normal,
    SendFile,
    RecvFile,
    About,
    Configure,
}

struct AppState {
    lines: std::collections::VecDeque<(String, String)>,
    last_id: i64,
    scroll_offset: usize,
    auto_follow: bool,
    input: String,
    input_mode: InputMode,
    status_msg: Option<(String, Instant)>,
    port_name: String,
    /// Live port configuration; the single place the TUI keeps line settings.
    config: PortConfig,
    baud_index: usize,
    /// Show the capture time in front of every line.
    show_timestamps: bool,
    /// Render payloads as a hex dump instead of text.
    hex_view: bool,
}

impl AppState {
    fn new(port_name: &str, config: &PortConfig) -> Self {
        let baud_index = BAUD_PRESETS
            .iter()
            .position(|&b| b == config.baudrate)
            .unwrap_or(4);
        Self {
            lines: std::collections::VecDeque::new(),
            last_id: 0,
            scroll_offset: 0,
            auto_follow: true,
            input: String::new(),
            input_mode: InputMode::Normal,
            status_msg: None,
            port_name: port_name.to_string(),
            config: config.clone(),
            baud_index,
            show_timestamps: true,
            hex_view: false,
        }
    }

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status_msg = Some((msg.into(), Instant::now()));
    }

    fn push_line(&mut self, timestamp: String, payload: String) {
        self.lines.push_back((timestamp, payload));
        while self.lines.len() > MAX_VISIBLE_LINES {
            self.lines.pop_front();
            self.scroll_offset = self.scroll_offset.saturating_sub(1);
        }
    }

    fn note(&mut self, storage: &Arc<std::sync::Mutex<SqliteStorage>>, text: &str) {
        let ts = chrono::Utc::now().format("%H:%M:%S%.3f").to_string();
        self.push_line(ts, text.to_string());
        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let guard = storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = guard.insert_lines(&[(now, text)]);
    }
}

#[allow(clippy::too_many_lines)]
fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    port_name: &str,
    config: &PortConfig,
    storage: &Arc<std::sync::Mutex<SqliteStorage>>,
    write_port: &SerialPortHandle,
    runtime: &tokio::runtime::Handle,
) -> Result<(), CliError> {
    let mut state = AppState::new(port_name, config);

    loop {
        // Poll new lines from storage.
        let fetched = {
            let guard = storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.read_lines(state.last_id + 1, FETCH_BATCH).ok()
        };
        for line in fetched.into_iter().flatten() {
            let ts = chrono::DateTime::from_timestamp_nanos(line.timestamp_ns)
                .format("%H:%M:%S%.3f")
                .to_string();
            state.last_id = line.id;
            state.push_line(ts, line.payload);
        }

        if state.auto_follow && !state.lines.is_empty() {
            let visible = terminal.size()?.height.saturating_sub(6) as usize;
            state.scroll_offset = state.lines.len().saturating_sub(visible);
        }

        terminal.draw(|frame| render(frame, &state))?;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => break,
                KeyCode::Char('b') => {
                    send_break(runtime, write_port);
                    state.note(
                        storage,
                        &format!("━━━ SERIAL BREAK ({DEFAULT_BREAK_MS}ms) SENT ━━━"),
                    );
                    continue;
                }
                KeyCode::Char('s') if state.input_mode == InputMode::Normal => {
                    state.input_mode = InputMode::SendFile;
                    state.input.clear();
                    state.set_status("Enter a file path to send (ZMODEM)");
                    continue;
                }
                KeyCode::Char('r') if state.input_mode == InputMode::Normal => {
                    state.input_mode = InputMode::RecvFile;
                    state.input.clear();
                    state.set_status("Enter a directory to receive into (ZMODEM)");
                    continue;
                }
                KeyCode::Char('p') => {
                    state.input_mode = toggle(&state.input_mode, InputMode::Configure);
                    continue;
                }
                KeyCode::Char('t') => {
                    state.show_timestamps = !state.show_timestamps;
                    state.set_status(if state.show_timestamps {
                        "Timestamps on"
                    } else {
                        "Timestamps off"
                    });
                    continue;
                }
                KeyCode::Char('h') => {
                    state.hex_view = !state.hex_view;
                    state.set_status(if state.hex_view {
                        "Hex view on"
                    } else {
                        "Text view on"
                    });
                    continue;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::F(1) => {
                state.input_mode = toggle(&state.input_mode, InputMode::About);
                continue;
            }
            KeyCode::F(2) => {
                state.input_mode = toggle(&state.input_mode, InputMode::Configure);
                continue;
            }
            KeyCode::F(4) => {
                send_break(runtime, write_port);
                state.note(
                    storage,
                    &format!("━━━ SERIAL BREAK ({DEFAULT_BREAK_MS}ms) SENT ━━━"),
                );
                continue;
            }
            KeyCode::Esc => {
                state.input_mode = InputMode::Normal;
                state.input.clear();
                state.set_status("Ready");
                continue;
            }
            _ => {}
        }

        if state.input_mode == InputMode::Configure {
            handle_configure_key(&mut state, key.code, runtime, write_port, storage);
            continue;
        }

        match key.code {
            KeyCode::Char('q')
                if state.input_mode == InputMode::Normal && state.input.is_empty() =>
            {
                break;
            }
            KeyCode::Char(c) => state.input.push(c),
            KeyCode::Backspace => {
                state.input.pop();
            }
            KeyCode::Enter => handle_enter(&mut state, runtime, write_port),
            KeyCode::Up => {
                state.auto_follow = false;
                state.scroll_offset = state.scroll_offset.saturating_sub(1);
            }
            KeyCode::Down => {
                let visible = terminal.size()?.height.saturating_sub(6) as usize;
                let max = state.lines.len().saturating_sub(visible);
                state.scroll_offset = (state.scroll_offset + 1).min(max);
                state.auto_follow = state.scroll_offset >= max;
            }
            KeyCode::PageUp => {
                state.auto_follow = false;
                state.scroll_offset = state.scroll_offset.saturating_sub(20);
            }
            KeyCode::PageDown => {
                let visible = terminal.size()?.height.saturating_sub(6) as usize;
                let max = state.lines.len().saturating_sub(visible);
                state.scroll_offset = (state.scroll_offset + 20).min(max);
                state.auto_follow = state.scroll_offset >= max;
            }
            KeyCode::End => state.auto_follow = true,
            _ => {}
        }
    }

    Ok(())
}

fn toggle(current: &InputMode, target: InputMode) -> InputMode {
    if *current == target {
        InputMode::Normal
    } else {
        target
    }
}

fn send_break(runtime: &tokio::runtime::Handle, port: &SerialPortHandle) {
    let port = Arc::clone(port);
    runtime.block_on(async move {
        let guard = port.lock().await;
        let _ = guard.set_break(true);
        tokio::time::sleep(Duration::from_millis(DEFAULT_BREAK_MS)).await;
        let _ = guard.set_break(false);
    });
}

fn handle_enter(state: &mut AppState, runtime: &tokio::runtime::Handle, port: &SerialPortHandle) {
    match state.input_mode {
        InputMode::Normal if !state.input.is_empty() => {
            let mut data = state.input.as_bytes().to_vec();
            data.extend_from_slice(b"\r\n");
            let port = Arc::clone(port);
            runtime.block_on(async move {
                let _ = port.lock().await.write_all(&data).await;
            });
            state.input.clear();
        }
        InputMode::SendFile if !state.input.is_empty() => {
            let path = std::mem::take(&mut state.input);
            state.input_mode = InputMode::Normal;

            let (data, name) = match std::fs::read(&path) {
                Ok(data) => {
                    let name = std::path::Path::new(&path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file.bin")
                        .to_string();
                    (data, name)
                }
                Err(e) => {
                    state.set_status(format!("Read error: {e}"));
                    return;
                }
            };

            state.set_status(format!("Sending '{path}' via ZMODEM..."));
            let port = Arc::clone(port);
            let result = runtime.block_on(async move {
                let mut guard = port.lock().await;
                crate::modem::send(&mut *guard, FileTransferProtocol::Zmodem, &name, &data).await
            });
            match result {
                Ok(bytes) => state.set_status(format!("Sent {bytes} bytes via ZMODEM")),
                Err(e) => state.set_status(format!("ZMODEM send failed: {e}")),
            }
        }
        InputMode::RecvFile => {
            let dir = if state.input.is_empty() {
                ".".to_string()
            } else {
                std::mem::take(&mut state.input)
            };
            state.input_mode = InputMode::Normal;
            state.set_status(format!("Receiving into '{dir}' via ZMODEM..."));

            let port = Arc::clone(port);
            let result = runtime.block_on(async move {
                let mut guard = port.lock().await;
                crate::modem::receive(&mut *guard, FileTransferProtocol::Zmodem).await
            });
            match result {
                Ok((name, data)) => {
                    let target = std::path::Path::new(&dir).join(&name);
                    match std::fs::write(&target, &data) {
                        Ok(()) => state.set_status(format!("Received {}", target.display())),
                        Err(e) => state.set_status(format!("Save error: {e}")),
                    }
                }
                Err(e) => state.set_status(format!("ZMODEM receive failed: {e}")),
            }
        }
        _ => {}
    }
}

fn handle_configure_key(
    state: &mut AppState,
    code: KeyCode,
    runtime: &tokio::runtime::Handle,
    port: &SerialPortHandle,
    storage: &Arc<std::sync::Mutex<SqliteStorage>>,
) {
    match code {
        KeyCode::Up => state.baud_index = state.baud_index.saturating_sub(1),
        KeyCode::Down => state.baud_index = (state.baud_index + 1).min(BAUD_PRESETS.len() - 1),
        KeyCode::Char(c @ '1'..='8') => {
            let index = (c as usize - '1' as usize).min(BAUD_PRESETS.len() - 1);
            state.baud_index = index;
        }
        KeyCode::Char('p' | 'P') => {
            state.config.parity = next_parity(state.config.parity);
        }
        KeyCode::Char('d' | 'D') => {
            state.config.data_bits = next_data_bits(state.config.data_bits);
        }
        KeyCode::Char('s' | 'S') => {
            state.config.stop_bits = if state.config.stop_bits == StopBits::ONE {
                StopBits::TWO
            } else {
                StopBits::ONE
            };
        }
        KeyCode::Char('f' | 'F') => {
            state.config.flow_control = next_flow_control(state.config.flow_control);
        }
        KeyCode::Enter => {
            state.config.baudrate = BAUD_PRESETS[state.baud_index];
            let config = state.config.clone();
            let port = Arc::clone(port);
            let result = runtime.block_on(async move {
                let mut guard = port.lock().await;
                crate::port_manager::reconfigure_serial_port(&mut guard, &config)
            });

            match result {
                Ok(()) => {
                    let summary = state.config.framing_summary();
                    state.note(storage, &format!("━━━ RECONFIGURED: {summary} ━━━"));
                    state.set_status(format!("Port reconfigured to {summary}"));
                }
                Err(e) => state.set_status(format!("Reconfigure failed: {e}")),
            }
            state.input_mode = InputMode::Normal;
        }
        _ => {}
    }
}

const fn next_parity(current: Parity) -> Parity {
    match current {
        Parity::None => Parity::Even,
        Parity::Even => Parity::Odd,
        Parity::Odd => Parity::None,
    }
}

const fn next_data_bits(current: DataBits) -> DataBits {
    match current.get() {
        8 => DataBits::SEVEN,
        7 => DataBits::SIX,
        6 => DataBits::FIVE,
        _ => DataBits::EIGHT,
    }
}

const fn next_flow_control(current: FlowControl) -> FlowControl {
    match current {
        FlowControl::None => FlowControl::Software,
        FlowControl::Software => FlowControl::Hardware,
        FlowControl::Hardware => FlowControl::None,
    }
}

fn render(frame: &mut Frame, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // status bar
            Constraint::Min(5),    // buffer
            Constraint::Length(3), // input
        ])
        .split(frame.area());

    let follow = if state.auto_follow {
        "AUTO-FOLLOW"
    } else {
        "PAUSED"
    };
    let status_text = state
        .status_msg
        .as_ref()
        .filter(|(_, created)| created.elapsed() < STATUS_TTL)
        .map_or_else(String::new, |(msg, _)| format!(" [{msg}] | "));

    let view = match (state.show_timestamps, state.hex_view) {
        (true, true) => "time+hex",
        (true, false) => "time",
        (false, true) => "hex",
        (false, false) => "raw",
    };
    let status = format!(
        " {} | {} | Lines: {} | View: {view} | {status_text}{follow}",
        state.port_name,
        state.config.framing_summary(),
        state.lines.len(),
    );
    frame.render_widget(
        Paragraph::new(status).style(Style::default().bg(Color::DarkGray)),
        chunks[0],
    );

    let visible_height = chunks[1].height as usize;
    let end = (state.scroll_offset + visible_height).min(state.lines.len());
    let visible: Vec<Line> = state
        .lines
        .iter()
        .skip(state.scroll_offset)
        .take(end.saturating_sub(state.scroll_offset))
        .map(|(timestamp, payload)| {
            let body = payload_text(payload, state.hex_view);
            let colour = if state.hex_view {
                Color::White
            } else {
                severity_color(payload)
            };
            let mut spans = Vec::with_capacity(2);
            if state.show_timestamps {
                spans.push(Span::styled(
                    format!("{timestamp} "),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            spans.push(Span::styled(body, Style::default().fg(colour)));
            Line::from(spans)
        })
        .collect();

    frame.render_widget(
        Paragraph::new(visible).block(Block::default().borders(Borders::NONE)),
        chunks[1],
    );

    let mut scrollbar_state = ScrollbarState::new(state.lines.len())
        .position(state.scroll_offset)
        .viewport_content_length(visible_height);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight),
        chunks[1],
        &mut scrollbar_state,
    );

    let (prompt, title) = match state.input_mode {
        InputMode::Normal => (
            "> ",
            " Enter send | F2 config | Ctrl+B break | Ctrl+S send file | Ctrl+R recv file | Ctrl+T time | Ctrl+H hex | F1 about | Ctrl+C quit ",
        ),
        InputMode::SendFile => (
            "Send File Path: ",
            " Enter a file path to transmit (Esc to cancel) ",
        ),
        InputMode::RecvFile => (
            "Recv Output Dir: ",
            " Enter a directory to save the received file in (Esc to cancel) ",
        ),
        InputMode::About => ("", " About devserial (Esc or F1 to close) "),
        InputMode::Configure => (
            "",
            " Configure port settings (Enter to apply, Esc to cancel) ",
        ),
    };

    frame.render_widget(
        Paragraph::new(format!("{prompt}{}", state.input))
            .block(Block::default().borders(Borders::TOP).title(title)),
        chunks[2],
    );

    match state.input_mode {
        InputMode::About => render_about_popup(frame),
        InputMode::Configure => render_configure_popup(frame, state),
        _ => {}
    }
}

/// Render a payload for the active view.
///
/// The hex dump uses the same grouping as the GUI, so both monitors show one
/// byte stream in one form.
fn payload_text(payload: &str, hex_view: bool) -> String {
    if hex_view {
        crate::hex::format_dump(payload.as_bytes())
    } else {
        payload.to_string()
    }
}

/// Colour a line by the severity marker it carries.
fn severity_color(payload: &str) -> Color {
    if payload.contains("[ERROR]") || payload.contains("[PANIC]") {
        Color::Red
    } else if payload.contains("[WARN]") {
        Color::Yellow
    } else if payload.contains("[DEBUG]") {
        Color::DarkGray
    } else if payload.contains("TRANSFER")
        || payload.contains("BREAK")
        || payload.contains("RECONFIGURED")
    {
        Color::Cyan
    } else {
        Color::White
    }
}

fn render_configure_popup(frame: &mut Frame, state: &AppState) {
    let area = centered_rect(65, 65, frame.area());
    frame.render_widget(ratatui::widgets::Clear, area);

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                "⚙ Port Settings — ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(&state.port_name, Style::default().fg(Color::Yellow)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Baud Rate (Up/Down or 1-8):",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
    ];

    for (index, baud) in BAUD_PRESETS.iter().enumerate() {
        let selected = index == state.baud_index;
        let prefix = if selected { " ▶ " } else { "   " };
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(vec![
            Span::raw(format!("{prefix}[{}] ", index + 1)),
            Span::styled(format!("{baud:>7} baud"), style),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            "Framing & Control:  ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("[D]ata: {}  ", state.config.data_bits),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            format!("[P]arity: {}  ", state.config.parity),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            format!("[S]top: {}  ", state.config.stop_bits),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            format!("[F]low: {}", state.config.flow_control),
            Style::default().fg(Color::Green),
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Press [Enter] to apply, [Esc] to cancel",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" ⚙ Configure Serial Port ")
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .alignment(Alignment::Left),
        area,
    );
}

fn render_about_popup(frame: &mut Frame) {
    let area = centered_rect(65, 55, frame.area());
    frame.render_widget(ratatui::widgets::Clear, area);

    let bold = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let text = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "devserial ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("v{}", env!("CARGO_PKG_VERSION")),
                Style::default().fg(Color::Yellow),
            ),
        ]),
        Line::from(""),
        Line::from(env!("CARGO_PKG_DESCRIPTION")),
        Line::from(""),
        Line::from(vec![
            Span::styled("Author:     ", bold),
            Span::raw(authors()),
        ]),
        Line::from(vec![
            Span::styled("License:    ", bold),
            Span::raw(env!("CARGO_PKG_LICENSE")),
        ]),
        Line::from(vec![
            Span::styled("Repository: ", bold),
            Span::styled(
                env!("CARGO_PKG_REPOSITORY"),
                Style::default().fg(Color::Blue),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Press Esc or F1 to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" ℹ About devserial ")
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .alignment(Alignment::Center),
        area,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_buffer_is_capped() {
        let mut state = AppState::new("/dev/x", &PortConfig::default());
        for i in 0..(MAX_VISIBLE_LINES + 100) {
            state.push_line("00:00:00".into(), format!("line {i}"));
        }
        assert_eq!(state.lines.len(), MAX_VISIBLE_LINES);
        assert_eq!(
            state.lines.back().unwrap().1,
            format!("line {}", MAX_VISIBLE_LINES + 99)
        );
    }

    #[test]
    fn framing_cycles_through_every_value() {
        let mut parity = Parity::None;
        for _ in 0..3 {
            parity = next_parity(parity);
        }
        assert_eq!(parity, Parity::None);

        let mut bits = DataBits::EIGHT;
        for _ in 0..4 {
            bits = next_data_bits(bits);
        }
        assert_eq!(bits, DataBits::EIGHT);

        let mut flow = FlowControl::None;
        for _ in 0..3 {
            flow = next_flow_control(flow);
        }
        assert_eq!(flow, FlowControl::None);
    }

    #[test]
    fn baud_index_follows_the_configuration() {
        let config = PortConfig {
            baudrate: 9600,
            ..PortConfig::default()
        };
        let state = AppState::new("/dev/x", &config);
        assert_eq!(BAUD_PRESETS[state.baud_index], 9600);
    }

    #[test]
    fn the_hex_view_shows_the_bytes_of_the_payload() {
        assert_eq!(payload_text("AB", false), "AB");
        assert_eq!(payload_text("AB", true), "41 42");
        assert_eq!(payload_text("", true), "");
    }

    #[test]
    fn view_toggles_start_at_text_with_timestamps() {
        let state = AppState::new("/dev/x", &PortConfig::default());
        assert!(state.show_timestamps);
        assert!(!state.hex_view);
    }

    #[test]
    fn severity_colours_are_distinct() {
        assert_eq!(severity_color("[ERROR] boom"), Color::Red);
        assert_eq!(severity_color("[WARN] hmm"), Color::Yellow);
        assert_eq!(severity_color("plain"), Color::White);
    }

    #[test]
    fn mode_toggle_returns_to_normal() {
        assert!(matches!(
            toggle(&InputMode::About, InputMode::About),
            InputMode::Normal
        ));
        assert!(matches!(
            toggle(&InputMode::Normal, InputMode::About),
            InputMode::About
        ));
    }
}
