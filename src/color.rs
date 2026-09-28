// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Semantic colours for human-readable capture output only.
//! Stored payloads, exports and protocol responses never contain styling.

use std::io::IsTerminal as _;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Muted,
    Sent,
    Event,
    Warning,
    Error,
}

static CLI_MODE: OnceLock<ColorMode> = OnceLock::new();
static UI_PREFERENCE: OnceLock<AtomicBool> = OnceLock::new();

pub fn set_cli_mode(mode: ColorMode) {
    let _ = CLI_MODE.set(mode);
}

fn mode() -> ColorMode {
    *CLI_MODE.get().unwrap_or(&ColorMode::Auto)
}

fn no_color_requested() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
}

#[must_use]
pub fn terminal_enabled() -> bool {
    terminal_enabled_for(
        mode(),
        std::io::stdout().is_terminal(),
        no_color_requested(),
    )
}

#[must_use]
pub const fn terminal_enabled_for(mode: ColorMode, is_terminal: bool, no_color: bool) -> bool {
    match mode {
        ColorMode::Auto => is_terminal && !no_color,
        ColorMode::Always => true,
        ColorMode::Never => false,
    }
}

#[must_use]
pub fn ui_enabled() -> bool {
    ui_enabled_for(mode(), current_preference(), no_color_requested())
}

fn current_preference() -> bool {
    UI_PREFERENCE
        .get_or_init(|| AtomicBool::new(load_preference()))
        .load(Ordering::Acquire)
}

#[must_use]
pub const fn ui_enabled_for(mode: ColorMode, preference: bool, no_color: bool) -> bool {
    match mode {
        ColorMode::Auto => preference && !no_color,
        ColorMode::Always => true,
        ColorMode::Never => false,
    }
}

#[must_use]
pub fn ui_is_locked() -> bool {
    mode() != ColorMode::Auto || no_color_requested()
}

/// Only an anchored level marker is interpreted. Arbitrary serial data is not a log level.
#[must_use]
pub fn classify(payload: &str) -> Tone {
    let text = payload.trim_start();
    if text.starts_with("[ERROR]") || text.starts_with("[PANIC]") {
        Tone::Error
    } else if text.starts_with("[WARN]") {
        Tone::Warning
    } else if text.starts_with("[DEBUG]") || text.starts_with("[TRACE]") {
        Tone::Muted
    } else if text.starts_with("━━━ ") {
        Tone::Event
    } else {
        Tone::Plain
    }
}

#[must_use]
pub fn ansi(text: &str, tone: Tone, enabled: bool) -> String {
    let code = match tone {
        Tone::Plain => return text.to_owned(),
        Tone::Muted => "90",
        Tone::Sent | Tone::Event => "36",
        Tone::Warning => "33",
        Tone::Error => "31",
    };
    if enabled {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct DisplayPreference {
    color: Option<bool>,
}

fn preference_path() -> std::path::PathBuf {
    crate::paths::default_data_dir().join("display.json")
}

#[must_use]
pub fn load_preference() -> bool {
    load_preference_from(&preference_path())
}

fn load_preference_from(path: &std::path::Path) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<DisplayPreference>(&bytes).ok())
        .and_then(|preference| preference.color)
        .unwrap_or(true)
}

/// Save the display-only preference outside capture storage.
///
/// # Errors
/// Returns an error if the user data directory or preference file is not writable.
pub fn save_preference(enabled: bool) -> std::io::Result<()> {
    let path = preference_path();
    UI_PREFERENCE
        .get_or_init(|| AtomicBool::new(load_preference_from(&path)))
        .store(enabled, Ordering::Release);
    save_preference_to(&path, enabled)
}

fn save_preference_to(path: &std::path::Path, enabled: bool) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_vec_pretty(&DisplayPreference {
        color: Some(enabled),
    })?;
    std::fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_auto_respects_tty_and_no_color() {
        assert!(terminal_enabled_for(ColorMode::Auto, true, false));
        assert!(!terminal_enabled_for(ColorMode::Auto, false, false));
        assert!(!terminal_enabled_for(ColorMode::Auto, true, true));
        assert!(terminal_enabled_for(ColorMode::Always, false, true));
        assert!(!terminal_enabled_for(ColorMode::Never, true, false));
    }

    #[test]
    fn gui_default_is_on_but_can_be_overridden() {
        assert!(ui_enabled_for(ColorMode::Auto, true, false));
        assert!(!ui_enabled_for(ColorMode::Auto, false, false));
        assert!(!ui_enabled_for(ColorMode::Auto, true, true));
        assert!(!ui_enabled_for(ColorMode::Never, true, false));
    }

    #[test]
    fn only_explicit_leading_levels_get_severity() {
        assert_eq!(classify("[ERROR] failed"), Tone::Error);
        assert_eq!(classify("  [WARN] retry"), Tone::Warning);
        assert_eq!(classify("sensor error count: 3"), Tone::Plain);
        assert_eq!(classify("value [ERROR] is literal"), Tone::Plain);
    }

    #[test]
    fn ansi_never_changes_plain_or_disabled_output() {
        assert_eq!(ansi("hello", Tone::Error, false), "hello");
        assert_eq!(ansi("hello", Tone::Plain, true), "hello");
        assert_eq!(ansi("hello", Tone::Error, true), "\x1b[31mhello\x1b[0m");
    }

    #[test]
    fn gui_preference_round_trips_without_a_capture_database() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("display.json");
        assert!(load_preference_from(&path));
        save_preference_to(&path, false).unwrap();
        assert!(!load_preference_from(&path));
        save_preference_to(&path, true).unwrap();
        assert!(load_preference_from(&path));
    }
}
