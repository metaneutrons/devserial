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
    ///
    /// The timeout is the backstop for `cargo test`, which has none of its own:
    /// without it a blocking invocation hangs the whole run. In CI the backstop
    /// is nextest's `terminate-after` in `.config/nextest.toml`, and that one
    /// has to fire first, which is why this period sits above it. nextest names
    /// the test it killed; this timeout cannot, for the reason the test at the
    /// bottom of this file pins down.
    ///
    /// It was 20 seconds, and that was too close to how long a cold debug
    /// binary takes to start on a loaded Windows runner. The job that exposed
    /// it took 6m22s where the same job on the same code had taken 1m55s, and
    /// `a_missing_config_file_is_reported` failed with empty output.
    fn command(&self) -> Command {
        let mut cmd = Command::cargo_bin("devserial").expect("binary");
        cmd.timeout(std::time::Duration::from_secs(90))
            .env("DEVSERIAL_DATA_DIR", self.dir.path())
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
fn forced_color_never_enters_json_output() {
    let output = Sandbox::new()
        .command()
        .args(["--color", "always", "list", "--json"])
        .output()
        .expect("list as JSON");
    assert!(output.status.success());
    assert!(!output.stdout.contains(&0x1b));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value.get("managed").is_some());
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

/// A timeout kill passes `.failure()`, so it can stand in for the failure a
/// test was expecting.
///
/// This pins the harness rather than devserial, because the behaviour cost an
/// afternoon. `assert_cmd` kills a child that outruns its timeout and hands
/// back the kill as an ordinary failing command. A test that asserts
/// `.failure()` and then a message therefore reports only "Unexpected stderr",
/// with an empty string and nothing to go on, and on Windows the kill even
/// arrives as exit code 1 so the status gives nothing away either.
///
/// Two things follow. The timeout in `Sandbox::command` has to stay above
/// nextest's `terminate-after`, so the mechanism that can name the test
/// reports first in CI. And anyone tempted to lower it should read this and
/// know what a too-short period turns into.
///
/// Unix only. The property belongs to `assert_cmd` and is not
/// platform-specific; what is platform-specific is finding a sleeper that
/// behaves the same everywhere, and Windows `timeout` refuses a redirected
/// stdin.
#[cfg(unix)]
#[test]
fn a_timeout_kill_passes_failure_and_carries_no_message() {
    let output = Command::new("sleep")
        .arg("5")
        .timeout(std::time::Duration::from_millis(200))
        .assert()
        // The dangerous part: a killed process is accepted as a failing one.
        .failure()
        .get_output()
        .clone();

    // The part that makes the report useless: a process killed before it wrote
    // anything has nothing to match a message predicate against.
    assert!(
        output.stderr.is_empty(),
        "a sleeper killed before it writes should carry no stderr, got {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The daemon's idle period as `daemon --status` reports it.
fn idle_line(sandbox: &Sandbox) -> String {
    let output = sandbox
        .command()
        .args(["daemon", "--status"])
        .output()
        .expect("status");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|line| line.starts_with("Idle"))
        .unwrap_or_default()
        .to_string()
}

/// Start a daemon through an operation, as every client does.
fn start_daemon(sandbox: &Sandbox) {
    sandbox
        .command()
        .args(["stats", "/dev/definitely-not-a-port"])
        .assert()
        .failure();
    assert!(sandbox.wait_for_daemon(true), "the daemon did not start");
}

/// #114: the status names the idle period and where it came from, and a
/// runtime change wins over everything else until the daemon stops.
#[test]
fn the_idle_period_is_reported_and_changed_at_runtime() {
    let sandbox = Sandbox::new();
    start_daemon(&sandbox);
    assert_eq!(idle_line(&sandbox), "Idle period: 15m (built-in default)");

    sandbox
        .command()
        .args(["daemon", "--set-idle-timeout", "2h"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Idle period: 2h (changed at runtime)",
        ));
    assert_eq!(idle_line(&sandbox), "Idle period: 2h (changed at runtime)");

    sandbox
        .command()
        .args(["daemon", "--set-idle-timeout", "0"])
        .assert()
        .success();
    assert_eq!(idle_line(&sandbox), "Idle stop: never (changed at runtime)");
}

/// #114: the configuration file sets the period an auto-started daemon uses.
#[test]
fn the_configured_idle_period_applies_without_a_flag() {
    let sandbox = Sandbox::new();
    std::fs::write(
        sandbox.dir.path().join("devserial.toml"),
        "[daemon]\nidle_timeout = \"30m\"\n",
    )
    .expect("config");
    start_daemon(&sandbox);
    assert_eq!(idle_line(&sandbox), "Idle period: 30m (configuration)");
}

/// #114: `--idle-timeout` overrides the configuration for the daemon it
/// starts.
#[test]
fn the_start_flag_overrides_the_configuration() {
    let sandbox = Sandbox::new();
    std::fs::write(
        sandbox.dir.path().join("devserial.toml"),
        "[daemon]\nidle_timeout = \"30m\"\n",
    )
    .expect("config");
    let mut daemon = std::process::Command::new(assert_cmd::cargo::cargo_bin("devserial"))
        .args(["daemon", "--idle-timeout", "1h"])
        .env("DEVSERIAL_DATA_DIR", sandbox.dir.path())
        .env("DEVSERIAL_SOCKET", sandbox.dir.path().join("test.sock"))
        .env_remove("DEVSERIAL_CONFIG")
        .current_dir(sandbox.dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("daemon");
    let started = sandbox.wait_for_daemon(true);
    let line = idle_line(&sandbox);
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert!(started, "the foreground daemon did not come up");
    assert_eq!(line, "Idle period: 1h (start flag)");
}

/// #114: a duration without a unit is refused before anything starts.
#[test]
fn an_idle_period_without_a_unit_is_refused() {
    Sandbox::new()
        .command()
        .args(["daemon", "--set-idle-timeout", "15"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("needs a unit"));
}

/// #114: an MCP session holds the daemon for as long as the AI tool keeps it
/// open, and lets go when the tool disconnects.
#[test]
fn an_mcp_session_holds_the_daemon_until_it_ends() {
    let sandbox = Sandbox::new();
    let mut mcp = std::process::Command::new(assert_cmd::cargo::cargo_bin("devserial"))
        .arg("mcp")
        .env("DEVSERIAL_DATA_DIR", sandbox.dir.path())
        .env("DEVSERIAL_SOCKET", sandbox.dir.path().join("test.sock"))
        .env_remove("DEVSERIAL_CONFIG")
        .current_dir(sandbox.dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("mcp");

    let held = |sandbox: &Sandbox| {
        let output = sandbox
            .command()
            .args(["daemon", "--status"])
            .output()
            .expect("status");
        String::from_utf8_lossy(&output.stdout).contains("Held by: MCP")
    };
    let mut seen = false;
    for _ in 0..200 {
        if held(&sandbox) {
            seen = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Closing stdin is the AI tool disconnecting.
    drop(mcp.stdin.take());
    let ended = (0..200).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(50));
        matches!(mcp.try_wait(), Ok(Some(_)))
    });
    if !ended {
        let _ = mcp.kill();
    }
    let _ = mcp.wait();

    assert!(seen, "the MCP session did not hold the daemon");
    assert!(
        ended,
        "the MCP server did not end when its client went away"
    );
    assert!(!held(&sandbox), "the lease outlived the MCP session");
}
