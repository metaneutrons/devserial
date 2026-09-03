// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Configuration model and file discovery.
//!
//! Every setting declared here has exactly one consumer path. A field that is
//! not read anywhere does not belong in this file, because the documented
//! configuration is a promise to the user.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths;
use crate::serial_params::{DEFAULT_BAUD, DataBits, FlowControl, Parity, StopBits};

/// Names of the macros devserial ships with.
pub const BUILTIN_MACRO_NAMES: [&str; 3] = ["reset", "enter_bootloader", "break"];

/// Top-level configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Global settings.
    pub global: GlobalConfig,
    /// Per-port configurations keyed by port path.
    pub ports: HashMap<String, PortConfig>,
    /// User-defined macros keyed by name.
    pub macros: HashMap<String, MacroConfig>,
}

impl Config {
    /// Port configuration for a port, falling back to the defaults.
    #[must_use]
    pub fn port(&self, name: &str) -> PortConfig {
        self.ports.get(name).cloned().unwrap_or_default()
    }

    /// Steps of a macro, whether user defined or built in.
    ///
    /// User definitions take precedence, so a configuration file can override a
    /// built-in macro for a specific setup.
    #[must_use]
    pub fn macro_steps(&self, name: &str) -> Option<Vec<MacroStep>> {
        self.macros
            .get(name)
            .map(|m| m.steps.clone())
            .or_else(|| builtin_macro(name))
    }

    /// All macro names that can be executed, user defined first.
    #[must_use]
    pub fn available_macros(&self) -> Vec<String> {
        let mut names: Vec<String> = self.macros.keys().cloned().collect();
        names.sort();
        for builtin in BUILTIN_MACRO_NAMES {
            if !self.macros.contains_key(builtin) {
                names.push(builtin.to_string());
            }
        }
        names
    }
}

/// Global server settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalConfig {
    /// Directory for database files.
    pub data_dir: PathBuf,
    /// Directory for archived databases.
    pub archive_dir: PathBuf,
    /// Interval after which a partial batch of captured lines is written.
    pub flush_interval_ms: u64,
    /// Number of captured lines that forces an immediate write.
    pub flush_batch_size: usize,
    /// Log level (trace, debug, info, warn, error).
    pub log_level: String,
}

impl GlobalConfig {
    /// Tracing filter directive derived from [`Self::log_level`].
    ///
    /// An unparseable level falls back to `info` with a warning, because a
    /// typo in the log level must not prevent the daemon from starting.
    #[must_use]
    pub fn log_directive(&self) -> String {
        let level = self.log_level.trim().to_ascii_lowercase();
        match level.as_str() {
            "trace" | "debug" | "info" | "warn" | "error" | "off" => level,
            other => {
                eprintln!("devserial: unknown log_level '{other}', using 'info'");
                "info".to_string()
            }
        }
    }

    /// Flush interval as a duration.
    #[must_use]
    pub const fn flush_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.flush_interval_ms)
    }
}

impl Default for GlobalConfig {
    fn default() -> Self {
        let data_dir = paths::default_data_dir();
        Self {
            archive_dir: data_dir.join("archive"),
            data_dir,
            flush_interval_ms: 100,
            flush_batch_size: 1000,
            log_level: "info".into(),
        }
    }
}

/// Per-port configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PortConfig {
    /// Baud rate.
    pub baudrate: u32,
    /// Data bits (5, 6, 7, 8).
    pub data_bits: DataBits,
    /// Parity.
    pub parity: Parity,
    /// Stop bits (1, 2).
    pub stop_bits: StopBits,
    /// Flow control.
    pub flow_control: FlowControl,
    /// Line delimiter byte.
    pub delimiter: u8,
    /// Enable auto-reconnect on disconnect.
    pub auto_reconnect: bool,
    /// Reconnect interval in milliseconds.
    pub reconnect_interval_ms: u64,
    /// Maximum reconnect backoff in milliseconds.
    pub reconnect_max_backoff_ms: u64,
    /// Maximum lines to keep in buffer (0 = unlimited).
    pub max_buffer_lines: u64,
}

impl PortConfig {
    /// Compact framing summary, for example `115200 8N1 (RTS/CTS)`.
    #[must_use]
    pub fn framing_summary(&self) -> String {
        format!("{self}")
    }

    /// Maximum reconnect backoff as a duration.
    #[must_use]
    pub const fn reconnect_max_backoff(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.reconnect_max_backoff_ms)
    }
}

impl fmt::Display for PortConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}{}{}{}",
            self.baudrate,
            self.data_bits,
            self.parity.letter(),
            self.stop_bits,
            self.flow_control.summary_suffix()
        )
    }
}

/// A user-defined macro: a sequence of steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MacroConfig {
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Ordered steps to execute.
    pub steps: Vec<MacroStep>,
}

/// A single step in a macro sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum MacroStep {
    /// Write bytes to the port (hex string or UTF-8).
    Write { value: String },
    /// Set DTR line.
    Dtr { value: bool },
    /// Set RTS line.
    Rts { value: bool },
    /// Delay in milliseconds.
    Delay { ms: u64 },
}

/// Steps of a built-in macro.
///
/// This is the only definition of the built-in sequences. Every transport and
/// the GUI resolve macros through [`Config::macro_steps`].
#[must_use]
pub fn builtin_macro(name: &str) -> Option<Vec<MacroStep>> {
    match name {
        "reset" => Some(vec![
            MacroStep::Dtr { value: false },
            MacroStep::Delay { ms: 100 },
            MacroStep::Dtr { value: true },
        ]),
        "enter_bootloader" => Some(vec![
            MacroStep::Rts { value: true },
            MacroStep::Dtr { value: false },
            MacroStep::Delay { ms: 50 },
            MacroStep::Dtr { value: true },
            MacroStep::Delay { ms: 50 },
            MacroStep::Rts { value: false },
        ]),
        "break" => Some(vec![MacroStep::Write {
            value: "0x00".into(),
        }]),
        _ => None,
    }
}

impl Default for PortConfig {
    fn default() -> Self {
        Self {
            baudrate: DEFAULT_BAUD,
            data_bits: DataBits::EIGHT,
            parity: Parity::None,
            stop_bits: StopBits::ONE,
            flow_control: FlowControl::None,
            delimiter: b'\n',
            auto_reconnect: true,
            reconnect_interval_ms: 1000,
            reconnect_max_backoff_ms: 30_000,
            max_buffer_lines: 0,
        }
    }
}

/// Load configuration from an explicit path, the environment, or discovery.
///
/// Discovery order:
/// 1. `explicit_path` (from `--config`)
/// 2. `DEVSERIAL_CONFIG`
/// 3. `./devserial.toml`
/// 4. `~/.config/devserial/config.toml` (`%APPDATA%\devserial\config.toml`)
///
/// If no file is found, returns the default configuration.
///
/// # Errors
/// Returns an error if a file exists but cannot be read or parsed, and if an
/// explicitly requested file does not exist.
pub fn load_config(explicit_path: Option<&Path>) -> Result<Config, ConfigError> {
    if let Some(path) = explicit_path {
        return load_required(path);
    }
    if let Some(path) = paths::config_override() {
        return load_required(&path);
    }

    let mut candidates = vec![PathBuf::from("./devserial.toml")];
    if let Some(dir) = paths::config_dir() {
        candidates.push(dir.join("config.toml"));
    }

    for path in &candidates {
        if path.exists() {
            let config = load_required(path)?;
            tracing::info!(path = %path.display(), "loaded configuration");
            return Ok(config);
        }
    }

    tracing::info!("no configuration file found, using defaults");
    Ok(Config::default())
}

fn load_required(path: &Path) -> Result<Config, ConfigError> {
    if !path.exists() {
        return Err(ConfigError::Missing(path.to_path_buf()));
    }
    let content =
        std::fs::read_to_string(path).map_err(|e| ConfigError::Io(path.to_path_buf(), e))?;
    toml::from_str(&content).map_err(|e| ConfigError::Parse(path.to_path_buf(), e))
}

/// Configuration loading errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("configuration file not found: {}", .0.display())]
    Missing(PathBuf),
    #[error("failed to read config file {}: {}", .0.display(), .1)]
    Io(PathBuf, std::io::Error),
    #[error("failed to parse config file {}: {}", .0.display(), .1)]
    Parse(PathBuf, toml::de::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.global.flush_interval_ms, 100);
        assert_eq!(config.global.flush_batch_size, 1000);
        assert!(config.ports.is_empty());
        assert!(config.macros.is_empty());
    }

    #[test]
    fn test_deserialize_empty_toml() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.global.flush_interval_ms, 100);
        assert_eq!(config.global.flush_batch_size, 1000);
    }

    #[test]
    fn test_deserialize_partial_toml() {
        let toml_str = r"
[global]
flush_interval_ms = 50
";
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.global.flush_interval_ms, 50);
        assert_eq!(config.global.flush_batch_size, 1000); // default
    }

    #[test]
    fn test_deserialize_full_config() {
        let toml_str = r#"
[global]
data_dir = "/tmp/serial"
archive_dir = "/tmp/serial/archive"
flush_interval_ms = 200
flush_batch_size = 500
log_level = "debug"

[ports."/dev/ttyUSB0"]
baudrate = 9600
data_bits = 8
parity = "none"
stop_bits = 1
flow_control = "none"
auto_reconnect = true

[ports."/dev/ttyUSB1"]
baudrate = 115200

[macros.reset_esp32]
description = "Reset ESP32 via DTR toggle"
steps = [
    { action = "dtr", value = false },
    { action = "delay", ms = 100 },
    { action = "dtr", value = true },
]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.global.flush_interval_ms, 200);
        assert_eq!(config.ports.len(), 2);
        assert_eq!(config.ports["/dev/ttyUSB0"].baudrate, 9600);
        assert_eq!(config.ports["/dev/ttyUSB1"].baudrate, 115_200);
        assert_eq!(config.macros.len(), 1);
        assert_eq!(config.macros["reset_esp32"].steps.len(), 3);
    }

    #[test]
    fn test_invalid_toml() {
        let result: Result<Config, _> = toml::from_str("invalid = [[[");
        assert!(result.is_err());
    }

    #[test]
    fn test_unknown_key_is_rejected() {
        let result: Result<Config, _> = toml::from_str("[global]\nflush_intervall_ms = 5\n");
        assert!(
            result.is_err(),
            "a misspelled key must not be silently ignored"
        );
    }

    #[test]
    fn test_invalid_parity_is_rejected() {
        let result: Result<Config, _> =
            toml::from_str("[ports.\"/dev/x\"]\nparity = \"sideways\"\n");
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_data_bits_is_rejected() {
        let result: Result<Config, _> = toml::from_str("[ports.\"/dev/x\"]\ndata_bits = 9\n");
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_explicit_config_is_an_error() {
        let err = load_config(Some(Path::new("/nonexistent/path.toml"))).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_load_config_with_tempfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        std::fs::write(&path, "[global]\nflush_interval_ms = 42\n").unwrap();

        let config = load_config(Some(&path)).unwrap();
        assert_eq!(config.global.flush_interval_ms, 42);
    }

    #[test]
    fn test_load_config_invalid_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "not valid toml [[[").unwrap();

        let result = load_config(Some(&path));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("failed to parse"));
    }

    #[test]
    fn test_port_config_defaults() {
        let port = PortConfig::default();
        assert_eq!(port.baudrate, 115_200);
        assert_eq!(port.data_bits, DataBits::EIGHT);
        assert_eq!(port.parity, Parity::None);
        assert_eq!(port.stop_bits, StopBits::ONE);
        assert!(port.auto_reconnect);
    }

    #[test]
    fn test_framing_summary() {
        let mut port = PortConfig::default();
        assert_eq!(port.framing_summary(), "115200 8N1");
        port.parity = Parity::Even;
        port.data_bits = DataBits::SEVEN;
        port.flow_control = FlowControl::Hardware;
        assert_eq!(port.framing_summary(), "115200 7E1 (RTS/CTS)");
    }

    #[test]
    fn test_macro_step_serialization() {
        let toml_str = r#"
description = "test"
steps = [
    { action = "write", value = "AT+RST\r\n" },
    { action = "delay", ms = 500 },
    { action = "dtr", value = true },
    { action = "rts", value = false },
]
"#;
        let m: MacroConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(m.steps.len(), 4);
        assert!(matches!(&m.steps[0], MacroStep::Write { value } if value == "AT+RST\r\n"));
        assert!(matches!(&m.steps[1], MacroStep::Delay { ms: 500 }));
        assert!(matches!(&m.steps[2], MacroStep::Dtr { value: true }));
        assert!(matches!(&m.steps[3], MacroStep::Rts { value: false }));
    }

    #[test]
    fn builtin_macros_resolve() {
        let config = Config::default();
        assert_eq!(config.macro_steps("reset").unwrap().len(), 3);
        assert_eq!(config.macro_steps("enter_bootloader").unwrap().len(), 6);
        assert_eq!(config.macro_steps("break").unwrap().len(), 1);
        assert!(config.macro_steps("nope").is_none());
    }

    #[test]
    fn user_macro_overrides_builtin() {
        let mut config = Config::default();
        config.macros.insert(
            "reset".into(),
            MacroConfig {
                description: String::new(),
                steps: vec![MacroStep::Delay { ms: 1 }],
            },
        );
        assert_eq!(config.macro_steps("reset").unwrap().len(), 1);
        let names = config.available_macros();
        assert_eq!(names.iter().filter(|n| *n == "reset").count(), 1);
        assert!(names.contains(&"enter_bootloader".to_string()));
    }

    #[test]
    fn log_directive_falls_back_for_unknown_level() {
        let mut global = GlobalConfig::default();
        assert_eq!(global.log_directive(), "info");
        global.log_level = "DEBUG".into();
        assert_eq!(global.log_directive(), "debug");
        global.log_level = "loud".into();
        assert_eq!(global.log_directive(), "info");
    }
}
