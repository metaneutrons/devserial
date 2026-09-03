// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! CLI integration tests.
//!
//! Every invocation runs against an isolated data directory and endpoint. The
//! previous version let `devserial list` auto-start a daemon on the developer's
//! real socket, which outlived the test run.

use assert_cmd::Command;
use predicates::prelude::*;

/// A sandbox with its own data directory and IPC endpoint.
struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("temp dir"),
        }
    }

    /// A devserial invocation that cannot touch production state.
    fn command(&self) -> Command {
        let mut cmd = Command::cargo_bin("devserial").expect("binary");
        cmd.env("DEVSERIAL_DATA_DIR", self.dir.path())
            .env("DEVSERIAL_SOCKET", self.dir.path().join("test.sock"))
            .env_remove("DEVSERIAL_CONFIG")
            // A stray configuration file in the working directory must not
            // influence the tests.
            .current_dir(self.dir.path());
        cmd
    }

    /// Poll `daemon --status` until it reports the expected state.
    fn wait_for_daemon(&self, running: bool) -> bool {
        let wanted = if running { "RUNNING" } else { "STOPPED" };
        for _ in 0..100 {
            let output = self
                .command()
                .args(["daemon", "--status"])
                .output()
                .expect("status");
            if String::from_utf8_lossy(&output.stdout).contains(wanted) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    }
}

impl Drop for Sandbox {
    /// Stop a daemon a test may have started, so no process outlives the run.
    fn drop(&mut self) {
        if let Ok(mut cmd) = Command::cargo_bin("devserial") {
            let _ = cmd
                .env("DEVSERIAL_DATA_DIR", self.dir.path())
                .env("DEVSERIAL_SOCKET", self.dir.path().join("test.sock"))
                .args(["daemon", "--stop"])
                .timeout(std::time::Duration::from_secs(10))
                .output();
        }
    }
}

#[test]
fn help_lists_the_main_commands() {
    Sandbox::new()
        .command()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage: devserial"))
        .stdout(predicate::str::contains("mcp"))
        .stdout(predicate::str::contains("list"))
        .stdout(predicate::str::contains("--config"));
}

#[test]
fn version_is_reported() {
    Sandbox::new()
        .command()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn about_comes_from_the_manifest() {
    Sandbox::new()
        .command()
        .arg("about")
        .assert()
        .success()
        .stdout(predicate::str::contains("GPL-3.0-or-later"))
        .stdout(predicate::str::contains(env!("CARGO_PKG_REPOSITORY")));
}

#[test]
fn list_works_without_a_running_daemon() {
    Sandbox::new()
        .command()
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("Available Serial Hardware"))
        .stdout(predicate::str::contains("daemon not running"));
}

#[test]
fn list_json_is_valid_json() {
    let output = Sandbox::new()
        .command()
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: serde_json::Value =
        serde_json::from_slice(&output).expect("list --json must emit JSON");
    assert!(parsed.get("available_hardware").is_some());
}

#[test]
fn daemon_status_reports_a_stopped_daemon() {
    Sandbox::new()
        .command()
        .args(["daemon", "--status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("STOPPED"));
}

#[test]
fn stopping_an_absent_daemon_is_not_an_error() {
    Sandbox::new()
        .command()
        .args(["daemon", "--stop"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not running"));
}

#[test]
fn invalid_parity_is_refused_by_the_parser() {
    Sandbox::new()
        .command()
        .args(["open", "/dev/nonexistent", "--parity", "sideways"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("possible values"));
}

#[test]
fn invalid_data_bits_are_refused_by_the_parser() {
    Sandbox::new()
        .command()
        .args(["open", "/dev/nonexistent", "--data-bits", "9"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("9"));
}

#[test]
fn invalid_export_format_is_refused_by_the_parser() {
    Sandbox::new()
        .command()
        .args(["export", "/dev/nonexistent", "out.txt", "--format", "xml"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("possible values"));
}

#[test]
fn invalid_search_mode_is_refused_by_the_parser() {
    Sandbox::new()
        .command()
        .args(["search", "/dev/nonexistent", "x", "--mode", "fuzzy"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("possible values"));
}

#[test]
fn hex_and_newline_cannot_be_combined() {
    Sandbox::new()
        .command()
        .args(["write", "/dev/nonexistent", "41", "--hex", "--newline"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be combined"));
}

#[test]
fn a_missing_config_file_is_reported() {
    Sandbox::new()
        .command()
        .args(["--config", "/definitely/not/here.toml", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

#[test]
fn an_unparseable_config_file_is_reported() {
    let sandbox = Sandbox::new();
    let config = sandbox.dir.path().join("broken.toml");
    std::fs::write(&config, "[global]\nflush_intervall_ms = 1\n").unwrap();

    sandbox
        .command()
        .args(["--config", config.to_str().unwrap(), "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to parse"));
}

#[test]
fn the_daemon_starts_on_demand_and_stops_again() {
    let sandbox = Sandbox::new();

    // The port does not exist, so the command fails, but only after the daemon
    // has answered. That proves auto-spawn, error propagation and the exit code
    // in one go.
    sandbox
        .command()
        .args(["stats", "/dev/definitely-not-a-port"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("devserial:"));

    assert!(
        sandbox.wait_for_daemon(true),
        "the daemon should be reachable after an operation"
    );

    sandbox
        .command()
        .args(["daemon", "--stop"])
        .assert()
        .success();

    assert!(
        sandbox.wait_for_daemon(false),
        "the daemon should be gone after --stop"
    );
}

#[test]
fn break_help_documents_the_duration() {
    Sandbox::new()
        .command()
        .args(["break", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("duration-ms"))
        .stdout(predicate::str::contains("250"));
}

#[test]
fn send_and_recv_help_list_the_protocols() {
    let sandbox = Sandbox::new();
    sandbox
        .command()
        .args(["send", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("zmodem"))
        .stdout(predicate::str::contains("xmodem1k"));

    sandbox
        .command()
        .args(["recv", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("output"));
}

#[test]
fn signal_and_macro_help_are_present() {
    let sandbox = Sandbox::new();
    sandbox
        .command()
        .args(["signal", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dtr"));
    sandbox
        .command()
        .args(["macro", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("name"));
}

#[test]
fn read_help_documents_the_incremental_options() {
    Sandbox::new()
        .command()
        .args(["read", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--after"))
        .stdout(predicate::str::contains("--since"))
        .stdout(predicate::str::contains("--wait-ms"));
}

#[test]
#[cfg(feature = "esp")]
fn flash_help_is_present() {
    Sandbox::new()
        .command()
        .args(["flash", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("firmware"));
}

#[test]
#[cfg(feature = "monitor")]
fn the_gui_subcommand_exists_for_desktop_launchers() {
    // The desktop entry runs `devserial gui`. Without a terminal on stdin the
    // bare `devserial` would start the MCP server instead of a window.
    Sandbox::new()
        .command()
        .args(["gui", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("connection manager"));
}

#[test]
#[cfg(feature = "monitor")]
fn monitor_help_documents_the_line_options() {
    Sandbox::new()
        .command()
        .args(["monitor", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--data-bits"))
        .stdout(predicate::str::contains("--parity"))
        .stdout(predicate::str::contains("--stop-bits"))
        .stdout(predicate::str::contains("--flow-control"));
}

#[test]
#[cfg(feature = "tui")]
fn tui_help_documents_the_line_options() {
    Sandbox::new()
        .command()
        .args(["tui", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--data-bits"))
        .stdout(predicate::str::contains("--flow-control"));
}
