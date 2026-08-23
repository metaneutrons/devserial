// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! TUI serial monitor using ratatui + crossterm.

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

use crate::storage::SqliteStorage;

/// Run the TUI monitor.
///
/// # Errors
/// Returns error on terminal or I/O failure.
pub fn run_tui(
    port_name: &str,
    baud: u32,
    storage: &Arc<std::sync::Mutex<SqliteStorage>>,
    write_port: &Arc<tokio::sync::Mutex<serial2_tokio::SerialPort>>,
    rt: &tokio::runtime::Runtime,
) -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let result = run_app(&mut terminal, port_name, baud, storage, write_port, rt);

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    result
}

#[derive(PartialEq, Eq)]
enum InputMode {
    Normal,
    SendFile,
    RecvFile,
}

struct AppState {
    lines: Vec<(String, String)>, // (timestamp, payload)
    last_id: i64,
    scroll_offset: usize,
    auto_follow: bool,
    input: String,
    input_mode: InputMode,
    status_msg: Option<(String, Instant)>,
    port_name: String,
    baud: u32,
}

impl AppState {
    fn set_status(&mut self, msg: impl Into<String>) {
        self.status_msg = Some((msg.into(), Instant::now()));
    }
}

#[allow(clippy::too_many_lines)]
fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    port_name: &str,
    baud: u32,
    storage: &Arc<std::sync::Mutex<SqliteStorage>>,
    write_port: &Arc<tokio::sync::Mutex<serial2_tokio::SerialPort>>,
    rt: &tokio::runtime::Runtime,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = AppState {
        lines: Vec::new(),
        last_id: 0,
        scroll_offset: 0,
        auto_follow: true,
        input: String::new(),
        input_mode: InputMode::Normal,
        status_msg: None,
        port_name: port_name.to_string(),
        baud,
    };

    loop {
        // Poll new lines from storage
        {
            let s = storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Ok(new_lines) = s.read_lines(state.last_id + 1, 500) {
                for line in &new_lines {
                    let ts = chrono::DateTime::from_timestamp_nanos(line.timestamp_ns)
                        .format("%H:%M:%S%.3f")
                        .to_string();
                    state.lines.push((ts, line.payload.clone()));
                    state.last_id = line.id;
                }
            }
        }

        if state.auto_follow && !state.lines.is_empty() {
            let visible_height = terminal.size()?.height.saturating_sub(6) as usize;
            state.scroll_offset = state.lines.len().saturating_sub(visible_height);
        }

        terminal.draw(|frame| render(frame, &state))?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                // Global hotkeys
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match key.code {
                        KeyCode::Char('c') => break,
                        KeyCode::Char('b') => {
                            // Send BREAK
                            let wp = Arc::clone(write_port);
                            rt.block_on(async {
                                let guard = wp.lock().await;
                                guard.set_break(true).ok();
                                tokio::time::sleep(Duration::from_millis(250)).await;
                                guard.set_break(false).ok();
                            });
                            state.set_status("SERIAL BREAK (250ms) SENT");
                            continue;
                        }
                        KeyCode::Char('s') if state.input_mode == InputMode::Normal => {
                            state.input_mode = InputMode::SendFile;
                            state.input.clear();
                            state.set_status("Enter file path to send (ZMODEM)");
                            continue;
                        }
                        KeyCode::Char('r') if state.input_mode == InputMode::Normal => {
                            state.input_mode = InputMode::RecvFile;
                            state.input.clear();
                            state.set_status("Enter directory to receive file (ZMODEM)");
                            continue;
                        }
                        _ => {}
                    }
                }

                if key.code == KeyCode::F(4) {
                    let wp = Arc::clone(write_port);
                    rt.block_on(async {
                        let guard = wp.lock().await;
                        guard.set_break(true).ok();
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        guard.set_break(false).ok();
                    });
                    state.set_status("SERIAL BREAK (250ms) SENT");
                    continue;
                }

                if key.code == KeyCode::Esc {
                    state.input_mode = InputMode::Normal;
                    state.input.clear();
                    state.set_status("Ready");
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
                    KeyCode::Enter => match state.input_mode {
                        InputMode::Normal if !state.input.is_empty() => {
                            let mut data = state.input.as_bytes().to_vec();
                            data.extend_from_slice(b"\r\n");
                            let wp = Arc::clone(write_port);
                            rt.block_on(async {
                                wp.lock().await.write_all(&data).await.ok();
                            });
                            state.input.clear();
                        }
                        InputMode::SendFile if !state.input.is_empty() => {
                            let file_path = state.input.clone();
                            state.input.clear();
                            state.input_mode = InputMode::Normal;
                            state.set_status(format!("Sending '{file_path}' via ZMODEM..."));

                            let wp = Arc::clone(write_port);
                            let (file_bytes, filename) = match std::fs::read(&file_path) {
                                Ok(b) => {
                                    let name = std::path::Path::new(&file_path)
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or("file.bin")
                                        .to_string();
                                    (b, name)
                                }
                                Err(e) => {
                                    state.set_status(format!("Read error: {e}"));
                                    continue;
                                }
                            };

                            let res = rt.block_on(async move {
                                let mut guard = wp.lock().await;
                                crate::modem::zmodem::send_file(
                                    &mut *guard,
                                    &filename,
                                    &file_bytes,
                                    |_, _| {},
                                )
                                .await
                            });

                            match res {
                                Ok(n) => {
                                    state
                                        .set_status(format!("ZMODEM: Sent {n} bytes successfully"));
                                }
                                Err(e) => state.set_status(format!("ZMODEM Error: {e}")),
                            }
                        }
                        InputMode::RecvFile => {
                            let out_dir = if state.input.is_empty() {
                                ".".to_string()
                            } else {
                                state.input.clone()
                            };
                            state.input.clear();
                            state.input_mode = InputMode::Normal;
                            state.set_status(format!("Receiving into '{out_dir}' via ZMODEM..."));

                            let wp = Arc::clone(write_port);
                            let res = rt.block_on(async move {
                                let mut guard = wp.lock().await;
                                crate::modem::zmodem::receive_file(&mut *guard, |_, _| {}).await
                            });

                            match res {
                                Ok((name, data)) => {
                                    let target = std::path::Path::new(&out_dir).join(&name);
                                    let _ = std::fs::write(&target, &data);
                                    state.set_status(format!(
                                        "ZMODEM: Received {} ({} bytes)",
                                        name,
                                        data.len()
                                    ));
                                }
                                Err(e) => state.set_status(format!("ZMODEM Recv Error: {e}")),
                            }
                        }
                        _ => {}
                    },
                    KeyCode::Up => {
                        state.auto_follow = false;
                        state.scroll_offset = state.scroll_offset.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        let visible_height = terminal
                            .size()
                            .map_or(20, |s| s.height.saturating_sub(6) as usize);
                        let max = state.lines.len().saturating_sub(visible_height);
                        state.scroll_offset = (state.scroll_offset + 1).min(max);
                        if state.scroll_offset >= max {
                            state.auto_follow = true;
                        }
                    }
                    KeyCode::PageUp => {
                        state.auto_follow = false;
                        state.scroll_offset = state.scroll_offset.saturating_sub(20);
                    }
                    KeyCode::PageDown => {
                        let visible_height = terminal
                            .size()
                            .map_or(20, |s| s.height.saturating_sub(6) as usize);
                        let max = state.lines.len().saturating_sub(visible_height);
                        state.scroll_offset = (state.scroll_offset + 20).min(max);
                        if state.scroll_offset >= max {
                            state.auto_follow = true;
                        }
                    }
                    KeyCode::End => {
                        state.auto_follow = true;
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

fn render(frame: &mut Frame, state: &AppState) {
    let area = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // status bar
            Constraint::Min(5),    // buffer
            Constraint::Length(3), // input
        ])
        .split(area);

    // Status bar
    let follow_indicator = if state.auto_follow {
        "AUTO-FOLLOW"
    } else {
        "PAUSED"
    };

    let status_text = if let Some((msg, created)) = &state.status_msg {
        if created.elapsed() < Duration::from_secs(5) {
            format!(" [{msg}] | ")
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let status = format!(
        " {} | {} 8N1 | Lines: {} | {}{} | Ctrl+B Break | Ctrl+S Send | Ctrl+R Recv",
        state.port_name,
        state.baud,
        state.lines.len(),
        status_text,
        follow_indicator
    );
    let status_widget = Paragraph::new(status).style(Style::default().bg(Color::DarkGray));
    frame.render_widget(status_widget, chunks[0]);

    // Buffer
    let visible_height = chunks[1].height as usize;
    let end = (state.scroll_offset + visible_height).min(state.lines.len());
    let visible_lines: Vec<Line> = state.lines[state.scroll_offset..end]
        .iter()
        .map(|(ts, payload)| {
            let color = if payload.contains("[ERROR]") || payload.contains("[PANIC]") {
                Color::Red
            } else if payload.contains("[WARN]") {
                Color::Yellow
            } else if payload.contains("[DEBUG]") {
                Color::DarkGray
            } else if payload.contains("TRANSFER") || payload.contains("BREAK") {
                Color::Cyan
            } else {
                Color::White
            };
            Line::from(vec![
                Span::styled(format!("{ts} "), Style::default().fg(Color::DarkGray)),
                Span::styled(payload.as_str(), Style::default().fg(color)),
            ])
        })
        .collect();

    let buffer_widget =
        Paragraph::new(visible_lines).block(Block::default().borders(Borders::NONE));
    frame.render_widget(buffer_widget, chunks[1]);

    // Scrollbar
    let mut scrollbar_state = ScrollbarState::new(state.lines.len())
        .position(state.scroll_offset)
        .viewport_content_length(visible_height);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight),
        chunks[1],
        &mut scrollbar_state,
    );

    // Input title & prompt based on mode
    let (prompt, title) = match state.input_mode {
        InputMode::Normal => (
            "> ",
            " Send (Enter) | Ctrl+B Break | Ctrl+S Send File | Ctrl+R Recv File | Ctrl+C quit ",
        ),
        InputMode::SendFile => (
            "Send File Path: ",
            " Enter file path to transmit (Esc to cancel) ",
        ),
        InputMode::RecvFile => (
            "Recv Output Dir: ",
            " Enter directory to save received file (Esc to cancel) ",
        ),
    };

    let input_widget = Paragraph::new(format!("{prompt}{}", state.input))
        .block(Block::default().borders(Borders::TOP).title(title));
    frame.render_widget(input_widget, chunks[2]);
}
