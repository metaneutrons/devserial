// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! ESP tooling via the `espflash` subprocess.

use tokio::process::Command;
use tokio::sync::OnceCell;

/// Name of the tool we shell out to.
const ESPFLASH: &str = "espflash";

/// Cached availability of the espflash binary.
static AVAILABLE: OnceCell<bool> = OnceCell::const_new();

/// Whether espflash is available in `PATH`.
///
/// The probe runs a subprocess, so it is asynchronous and cached. Calling it
/// from an async context used to block the runtime thread.
pub async fn is_available() -> bool {
    *AVAILABLE
        .get_or_init(|| async {
            match Command::new(ESPFLASH)
                .arg("--version")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await
            {
                Ok(output) if output.status.success() => {
                    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    tracing::info!(version = %version, "espflash detected");
                    true
                }
                Ok(_) => false,
                Err(e) => {
                    tracing::debug!(error = %e, "espflash not available");
                    false
                }
            }
        })
        .await
}

/// Flash firmware to an ESP device.
///
/// # Errors
/// Returns error if espflash fails or is not available.
pub async fn flash(port: &str, firmware_path: &str, baud: Option<u32>) -> Result<String, String> {
    let mut cmd = Command::new(ESPFLASH);
    cmd.arg("flash").arg("--port").arg(port);
    if let Some(baud) = baud {
        cmd.arg("--baud").arg(baud.to_string());
    }
    // `--` keeps a path that begins with a dash from being read as an option.
    cmd.arg("--").arg(firmware_path);
    run(cmd).await
}

/// Get board and chip information.
///
/// # Errors
/// Returns error if espflash fails or is not available.
pub async fn board_info(port: &str) -> Result<String, String> {
    let mut cmd = Command::new(ESPFLASH);
    cmd.arg("board-info").arg("--port").arg(port);
    run(cmd).await
}

/// Erase the entire flash.
///
/// # Errors
/// Returns error if espflash fails or is not available.
pub async fn erase_flash(port: &str) -> Result<String, String> {
    let mut cmd = Command::new(ESPFLASH);
    cmd.arg("erase-flash").arg("--port").arg(port);
    run(cmd).await
}

/// Write a binary file to a specific flash address.
///
/// # Errors
/// Returns error if espflash fails or is not available.
pub async fn write_bin(port: &str, file_path: &str, address: &str) -> Result<String, String> {
    let mut cmd = Command::new(ESPFLASH);
    cmd.arg("write-bin")
        .arg("--port")
        .arg(port)
        .arg("--")
        .arg(address)
        .arg(file_path);
    run(cmd).await
}

async fn run(mut cmd: Command) -> Result<String, String> {
    let output = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("failed to run {ESPFLASH}: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        Ok(join_streams(&stdout, &stderr))
    } else {
        let mut message = join_streams(&stderr, &stdout);
        if message.trim().is_empty() {
            message = format!("{ESPFLASH} exited with status {}", output.status);
        }
        Err(message)
    }
}

fn join_streams(primary: &str, secondary: &str) -> String {
    match (primary.trim().is_empty(), secondary.trim().is_empty()) {
        (true, _) => secondary.to_string(),
        (false, true) => primary.to_string(),
        (false, false) => format!("{primary}\n{secondary}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn availability_is_cached() {
        let first = is_available().await;
        let second = is_available().await;
        assert_eq!(first, second);
    }

    #[test]
    fn stream_joining_prefers_the_primary_stream() {
        assert_eq!(join_streams("out", ""), "out");
        assert_eq!(join_streams("", "err"), "err");
        assert_eq!(join_streams("out", "err"), "out\nerr");
    }

    #[tokio::test]
    async fn flash_reports_failure_for_a_missing_device() {
        if !is_available().await {
            return; // espflash is not installed on this machine
        }
        let result = flash(
            "/dev/nonexistent_port_xyz",
            "/nonexistent/firmware.elf",
            None,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn board_info_reports_failure_for_a_missing_device() {
        if !is_available().await {
            return;
        }
        assert!(board_info("/dev/nonexistent_port_xyz").await.is_err());
    }
}
