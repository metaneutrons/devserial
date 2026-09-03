// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Export serialization, shared by the CLI, the MCP server and the GUI.
//!
//! One format definition for all callers. Previously each transport wrote its
//! own CSV and JSONL variant, so the same command produced different files
//! depending on how it was invoked.

use std::fmt;
use std::io::Write;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::storage::StoredLine;

/// Output format for exported capture data.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    clap::ValueEnum,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
#[schemars(description = "Export format: txt (raw lines), csv (RFC 4180) or jsonl")]
pub enum ExportFormat {
    /// Raw payloads, one per line.
    #[default]
    Txt,
    /// RFC 4180 comma separated values with a header row.
    Csv,
    /// One JSON object per line.
    Jsonl,
}

impl ExportFormat {
    /// Conventional file extension.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Txt => "txt",
            Self::Csv => "csv",
            Self::Jsonl => "jsonl",
        }
    }

    /// All variants, in presentation order.
    #[must_use]
    pub const fn all() -> [Self; 3] {
        [Self::Txt, Self::Csv, Self::Jsonl]
    }
}

impl fmt::Display for ExportFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Txt => "txt",
            Self::Csv => "csv",
            Self::Jsonl => "jsonl",
        })
    }
}

impl FromStr for ExportFormat {
    type Err = ExportError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "txt" | "text" | "log" => Ok(Self::Txt),
            "csv" => Ok(Self::Csv),
            "jsonl" | "ndjson" => Ok(Self::Jsonl),
            other => Err(ExportError::UnknownFormat(other.to_string())),
        }
    }
}

/// Export failures.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("unknown export format '{0}', expected one of: txt, csv, jsonl")]
    UnknownFormat(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Render a nanosecond timestamp in the canonical export form.
#[must_use]
pub fn format_timestamp(timestamp_ns: i64) -> String {
    if timestamp_ns <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp_nanos(timestamp_ns)
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// Write the header row a format requires, if any.
///
/// # Errors
/// Returns an error if writing fails.
pub fn write_header<W: Write>(writer: &mut W, format: ExportFormat) -> Result<(), ExportError> {
    if format == ExportFormat::Csv {
        writeln!(writer, "line,timestamp,timestamp_ns,payload")?;
    }
    Ok(())
}

/// Write a single line in the given format.
///
/// # Errors
/// Returns an error if writing fails.
pub fn write_line<W: Write>(
    writer: &mut W,
    line: &StoredLine,
    format: ExportFormat,
) -> Result<(), ExportError> {
    match format {
        ExportFormat::Txt => writeln!(writer, "{}", line.payload)?,
        ExportFormat::Csv => {
            let escaped = line.payload.replace('"', "\"\"");
            writeln!(
                writer,
                "{},{},{},\"{}\"",
                line.id,
                format_timestamp(line.timestamp_ns),
                line.timestamp_ns,
                escaped
            )?;
        }
        ExportFormat::Jsonl => {
            let obj = serde_json::json!({
                "line": line.id,
                "timestamp": format_timestamp(line.timestamp_ns),
                "timestamp_ns": line.timestamp_ns,
                "payload": line.payload,
            });
            writeln!(writer, "{obj}")?;
        }
    }
    Ok(())
}

/// Write a complete export, returning the number of lines written.
///
/// # Errors
/// Returns an error if writing fails.
pub fn write_all<W: Write>(
    writer: &mut W,
    lines: &[StoredLine],
    format: ExportFormat,
) -> Result<u64, ExportError> {
    write_header(writer, format)?;
    for line in lines {
        write_line(writer, line, format)?;
    }
    writer.flush()?;
    Ok(u64::try_from(lines.len()).unwrap_or(u64::MAX))
}

/// Render a complete export into a string.
///
/// # Errors
/// Returns an error only if formatting fails, which cannot happen for an
/// in-memory buffer.
pub fn to_string(lines: &[StoredLine], format: ExportFormat) -> Result<String, ExportError> {
    let mut buf = Vec::new();
    write_all(&mut buf, lines, format)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Replace the extension of a path with the one matching a format.
#[must_use]
pub fn with_format_extension(path: &str, format: ExportFormat) -> String {
    let ext = format.extension();
    let candidate = std::path::Path::new(path);
    candidate
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map_or_else(
            || format!("{path}.{ext}"),
            |stem| {
                candidate
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .map_or_else(
                        || format!("{stem}.{ext}"),
                        |parent| {
                            parent
                                .join(format!("{stem}.{ext}"))
                                .to_string_lossy()
                                .into_owned()
                        },
                    )
            },
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<StoredLine> {
        vec![
            StoredLine {
                id: 1,
                timestamp_ns: 1_767_225_600_000_000_000,
                payload: "boot ok".into(),
            },
            StoredLine {
                id: 2,
                timestamp_ns: 1_767_225_600_500_000_000,
                payload: "value = \"42\"".into(),
            },
        ]
    }

    #[test]
    fn txt_writes_payloads_only() {
        let out = to_string(&sample(), ExportFormat::Txt).unwrap();
        assert_eq!(out, "boot ok\nvalue = \"42\"\n");
    }

    #[test]
    fn csv_has_header_and_doubles_quotes() {
        let out = to_string(&sample(), ExportFormat::Csv).unwrap();
        let mut rows = out.lines();
        assert_eq!(rows.next().unwrap(), "line,timestamp,timestamp_ns,payload");
        assert!(rows.next().unwrap().starts_with("1,2026-01-01T"));
        assert!(out.contains("\"value = \"\"42\"\"\""));
    }

    #[test]
    fn jsonl_is_one_object_per_line() {
        let out = to_string(&sample(), ExportFormat::Jsonl).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed["line"], 2);
        assert_eq!(parsed["payload"], "value = \"42\"");
        assert_eq!(parsed["timestamp_ns"], 1_767_225_600_500_000_000_i64);
    }

    #[test]
    fn format_parsing_rejects_unknown() {
        assert!("xyz".parse::<ExportFormat>().is_err());
        assert_eq!("CSV".parse::<ExportFormat>().unwrap(), ExportFormat::Csv);
        assert_eq!(
            "ndjson".parse::<ExportFormat>().unwrap(),
            ExportFormat::Jsonl
        );
    }

    #[test]
    fn extension_swap() {
        assert_eq!(
            with_format_extension("/tmp/log.txt", ExportFormat::Csv),
            "/tmp/log.csv"
        );
        assert_eq!(
            with_format_extension("log", ExportFormat::Jsonl),
            "log.jsonl"
        );
    }

    #[test]
    fn zero_timestamp_renders_empty() {
        assert_eq!(format_timestamp(0), "");
    }

    #[test]
    fn empty_export_still_writes_csv_header() {
        let out = to_string(&[], ExportFormat::Csv).unwrap();
        assert_eq!(out, "line,timestamp,timestamp_ns,payload\n");
        assert_eq!(to_string(&[], ExportFormat::Txt).unwrap(), "");
    }
}
