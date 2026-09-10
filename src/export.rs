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

/// Upper bound on lines pulled into memory for one export.
///
/// Shared by both surfaces, so a terminal export cannot quietly take more than
/// the window would.
pub const MAX_EXPORT_LINES: u32 = 500_000;

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

/// Render a timestamp for a record, exact and unambiguous.
///
/// RFC 3339 in UTC with all nine fractional digits, so the value survives a
/// round trip and sorts correctly as a string. It was three digits, which threw
/// away everything below a millisecond that the store holds; at 115200 baud a
/// byte takes about 87 µs, so those digits are the ones anyone measuring a
/// device's response latency needs.
#[must_use]
pub fn format_timestamp(timestamp_ns: i64) -> String {
    if timestamp_ns <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp_nanos(timestamp_ns)
        .format("%Y-%m-%dT%H:%M:%S%.9fZ")
        .to_string()
}

/// Render a timestamp for a person to read on a screen.
///
/// Local time, milliseconds, no date. A monitor shows one line per row, and a
/// full RFC 3339 stamp would spend thirty of eighty columns on a date that does
/// not change during a session. Local rather than UTC because the reader is
/// sitting next to the device; the zone is named once by the surface rather
/// than on every line.
#[must_use]
pub fn format_time_of_day(timestamp_ns: i64) -> String {
    if timestamp_ns <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp_nanos(timestamp_ns)
        .with_timezone(&chrono::Local)
        .format("%H:%M:%S%.3f")
        .to_string()
}

/// Render the current instant for a person to read on a screen.
///
/// The surfaces stamp a line they sent themselves, which has no id in the
/// store yet and therefore no stored timestamp to render.
#[must_use]
pub fn now_time_of_day() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}

/// Render the current instant for a filename.
///
/// Local time, because a filename is something a person looks for. Export
/// names were local and archive names were UTC, which gave the same moment two
/// names an offset apart.
#[must_use]
pub fn now_for_filename() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

/// What the surfaces put next to their timestamps, once.
///
/// Displayed times are local and carry no zone of their own; a record carries
/// UTC and says so with a `Z`. Without this said somewhere a reader comparing
/// the two finds an offset and no explanation.
pub const DISPLAY_ZONE_NOTE: &str = "times local";

/// One captured line as JSON.
///
/// The `jsonl` export and the HTTP interface hand out the same object, from
/// here, so a file on disk and a response on the wire carry the same records
/// rather than two shapes kept in step by hand. A test compares them.
///
/// `timestamp_ns` is a string on purpose. A nanosecond epoch in 2026 is 198.6
/// times above JavaScript's `MAX_SAFE_INTEGER`, where a double's step is
/// 256 ns, so `JSON.parse` rounds it silently. That is nothing against a UART
/// bit at 8.7 µs, but it breaks handing the value back as a filter, where an
/// exclusive boundary then repeats or skips a line.
#[must_use]
pub fn line_object(line: &StoredLine) -> serde_json::Value {
    serde_json::json!({
        "id": line.id,
        "timestamp": format_timestamp(line.timestamp_ns),
        "timestamp_ns": line.timestamp_ns.to_string(),
        "payload": line.payload,
    })
}

/// Write the header row a format requires, if any.
///
/// # Errors
/// Returns an error if writing fails.
pub fn write_header<W: Write>(writer: &mut W, format: ExportFormat) -> Result<(), ExportError> {
    if format == ExportFormat::Csv {
        writeln!(writer, "id,timestamp,timestamp_ns,payload")?;
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
            writeln!(writer, "{}", line_object(line))?;
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
        assert_eq!(rows.next().unwrap(), "id,timestamp,timestamp_ns,payload");
        assert!(rows.next().unwrap().starts_with("1,2026-01-01T"));
        assert!(out.contains("\"value = \"\"42\"\"\""));
    }

    #[test]
    fn jsonl_is_one_object_per_line() {
        let out = to_string(&sample(), ExportFormat::Jsonl).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed["id"], 2);
        assert_eq!(parsed["payload"], "value = \"42\"");
        // A string, not a number: see the comment where it is written.
        assert_eq!(parsed["timestamp_ns"], "1767225600500000000");
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
        // The function returns a native path, so the expectation is built
        // natively too. A literal "/tmp/log.csv" passes on Unix and fails on
        // Windows, where Path::join writes a backslash. PathBuf comparison
        // would hide the difference; this test compares strings on purpose,
        // because that is what callers put in front of a user.
        let expected = std::path::Path::new("/tmp")
            .join("log.csv")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            with_format_extension("/tmp/log.txt", ExportFormat::Csv),
            expected
        );
        // No parent, so no separator and nothing platform dependent.
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
        assert_eq!(out, "id,timestamp,timestamp_ns,payload\n");
        assert_eq!(to_string(&[], ExportFormat::Txt).unwrap(), "");
    }
}

#[cfg(test)]
mod timestamp_tests {
    use super::*;

    const MODULES: &[(&str, &str)] = &[
        ("monitor.rs", include_str!("monitor.rs")),
        ("tui.rs", include_str!("tui.rs")),
        ("server.rs", include_str!("server.rs")),
        ("paths.rs", include_str!("paths.rs")),
        ("cli/output.rs", include_str!("cli/output.rs")),
        ("engine.rs", include_str!("engine.rs")),
        ("reader.rs", include_str!("reader.rs")),
        ("standalone.rs", include_str!("standalone.rs")),
    ];

    /// A record keeps every digit the store holds.
    ///
    /// Three digits threw away the microseconds, and a byte at 115200 baud
    /// takes 87 µs, so the thrown-away part is where a latency measurement
    /// lives.
    #[test]
    fn a_record_carries_all_nine_digits_in_utc() {
        let rendered = format_timestamp(1_767_225_600_123_456_789);
        assert_eq!(rendered, "2026-01-01T00:00:00.123456789Z");
        assert!(rendered.ends_with('Z'), "a record has to name its zone");
        assert_eq!(format_timestamp(0), "", "no time is not the epoch");
        assert_eq!(format_timestamp(-1), "");
    }

    /// A screen shows local time of day, and nothing else.
    #[test]
    fn a_screen_shows_local_time_of_day() {
        let ns = 1_767_225_600_123_456_789;
        let rendered = format_time_of_day(ns);

        let expected = chrono::DateTime::from_timestamp_nanos(ns)
            .with_timezone(&chrono::Local)
            .format("%H:%M:%S%.3f")
            .to_string();
        assert_eq!(rendered, expected, "the screen form is not local");
        assert_eq!(rendered.len(), "00:00:00.123".len());
        assert!(
            !rendered.contains('Z') && !rendered.contains('-'),
            "the screen form carries neither a zone nor a date: {rendered}"
        );
        assert_eq!(format_time_of_day(0), "");
    }

    /// The two forms name the same instant, and differ only as intended.
    ///
    /// Without this the pair could drift into two clocks rather than two views
    /// of one, which is the failure the zone split invites.
    #[test]
    fn both_forms_describe_the_same_instant() {
        let ns = 1_767_268_496_000_000_000;
        let record = format_timestamp(ns);
        let screen = format_time_of_day(ns);

        let parsed = chrono::DateTime::parse_from_rfc3339(&record).expect("the record parses");
        assert_eq!(parsed.timestamp_nanos_opt(), Some(ns));
        assert_eq!(
            parsed
                .with_timezone(&chrono::Local)
                .format("%H:%M:%S%.3f")
                .to_string(),
            screen
        );
    }

    /// No module renders a timestamp on its own.
    ///
    /// The display format was written out seven times across five files and the
    /// filename format six times, all of them UTC without saying so. A comment
    /// would not have stopped the eighth copy.
    #[test]
    fn no_module_formats_a_timestamp_of_its_own() {
        // Assembled so this file does not match itself.
        let needles = [
            ["%H:%M", ":%S"].concat(),
            ["%Y%m%d", "_%H%M%S"].concat(),
            ["%Y-%m-%d", "T%H"].concat(),
        ];
        for (name, source) in MODULES {
            for needle in &needles {
                assert!(
                    !source.contains(needle.as_str()),
                    "{name} renders a timestamp itself with {needle}; \
                     use export::format_timestamp, format_time_of_day, \
                     now_time_of_day or now_for_filename"
                );
            }
        }
    }

    /// Nothing outside this module reaches for the clock to name a file.
    #[test]
    fn no_module_stamps_a_filename_of_its_own() {
        for (name, source) in MODULES {
            assert!(
                !source.contains("Local::now()") || name == &"reader.rs",
                "{name} takes the local clock itself; use export::now_for_filename"
            );
        }
    }
}
