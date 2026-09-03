// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

use chrono::{DateTime, Duration, Utc};

/// Generates realistic serial output for testing.
pub struct TestDataGenerator;

impl TestDataGenerator {
    /// Generate a typical device boot sequence.
    #[must_use]
    pub fn boot_sequence() -> Vec<String> {
        vec![
            "[0.000] Bootloader v2.1.0".into(),
            "[0.001] CPU: ARM Cortex-M4 @ 168MHz".into(),
            "[0.002] RAM: 192KB, Flash: 1MB".into(),
            "[0.010] Initializing peripherals...".into(),
            "[0.015] UART0: 115200 8N1".into(),
            "[0.020] SPI0: 42MHz".into(),
            "[0.025] I2C0: 400kHz".into(),
            "[0.100] Firmware: app v1.3.7 (build 2026-01-15)".into(),
            "[0.101] Starting main loop".into(),
        ]
    }

    /// Generate error burst lines.
    #[must_use]
    pub fn error_burst(count: usize) -> Vec<String> {
        (0..count)
            .map(|i| {
                format!(
                    "[ERROR] fault at 0x{:08X}: segmentation violation (iter {})",
                    0x2000_0000 + i * 4,
                    i
                )
            })
            .collect()
    }

    /// Generate N sequential lines with timestamps.
    #[must_use]
    pub fn sequential_lines(count: usize) -> Vec<String> {
        (0..count)
            .map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f64 * 0.001;
                format!("[{t:.3}] sensor reading #{i}: value={}", i * 42)
            })
            .collect()
    }

    /// Generate lines with mixed content for search testing.
    #[must_use]
    pub fn mixed_traffic(count: usize) -> Vec<String> {
        (0..count)
            .map(|i| match i % 10 {
                0 => format!("[ERROR] watchdog timeout at tick {i}"),
                1 => format!("[WARN] buffer usage at {}%", 50 + (i % 50)),
                2 => format!("[PANIC] stack overflow in task_{}", i % 5),
                3 => format!("[DEBUG] heap free: {} bytes", 1024 * (100 - i % 100)),
                _ => format!("[INFO] heartbeat #{i} ok"),
            })
            .collect()
    }

    /// Generate lines with specific patterns for search regression testing.
    #[must_use]
    pub fn search_corpus() -> Vec<String> {
        let mut lines = Vec::new();
        // Exact match targets
        lines.push("exact match".into());
        // Substring targets
        for i in 0..50 {
            lines.push(format!("[ERROR] connection refused (attempt {i})"));
        }
        // Regex targets: lines starting with ISO timestamp
        for i in 0..20 {
            lines.push(format!(
                "[2026-01-15T10:{i:02}:00Z] scheduled task completed"
            ));
        }
        // Filler
        for i in 0..30 {
            lines.push(format!("normal operation line {i}"));
        }
        lines
    }

    /// Generate timestamps for N lines starting from a base time.
    #[must_use]
    pub fn timestamps(count: usize, base: DateTime<Utc>, interval_ms: i64) -> Vec<i64> {
        (0..count)
            .map(|i| {
                let offset = interval_ms.saturating_mul(i64::try_from(i).unwrap_or(i64::MAX));
                (base + Duration::milliseconds(offset))
                    .timestamp_nanos_opt()
                    .unwrap_or(0)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_boot_sequence_not_empty() {
        let lines = TestDataGenerator::boot_sequence();
        assert!(!lines.is_empty());
        assert!(lines[0].contains("Bootloader"));
    }

    #[test]
    fn test_error_burst_count() {
        let lines = TestDataGenerator::error_burst(100);
        assert_eq!(lines.len(), 100);
        assert!(lines.iter().all(|l| l.contains("[ERROR]")));
    }

    #[test]
    fn test_sequential_lines_count() {
        let lines = TestDataGenerator::sequential_lines(500);
        assert_eq!(lines.len(), 500);
    }

    #[test]
    fn test_mixed_traffic_has_errors() {
        let lines = TestDataGenerator::mixed_traffic(100);
        assert_eq!(lines.iter().filter(|l| l.contains("[ERROR]")).count(), 10);
    }

    #[test]
    fn test_search_corpus_structure() {
        let lines = TestDataGenerator::search_corpus();
        assert!(lines.contains(&"exact match".to_string()));
        let error_count = lines.iter().filter(|l| l.contains("[ERROR]")).count();
        assert_eq!(error_count, 50);
    }

    #[test]
    fn test_timestamps_monotonic() {
        let ts = TestDataGenerator::timestamps(100, Utc::now(), 10);
        for window in ts.windows(2) {
            assert!(window[1] > window[0]);
        }
    }
}
