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
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("home")).expect("home");
        Self { dir }
    }

    /// The home every invocation sees. AI tools and their skills folders are
    /// looked up under it, so nothing a test starts reaches the real ones
    /// (#124).
    fn home(&self) -> std::path::PathBuf {
        self.dir.path().join("home")
    }

    /// A devserial process that cannot touch production state.
    fn std_command(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("devserial"));
        cmd.env("DEVSERIAL_DATA_DIR", self.dir.path())
            .env("DEVSERIAL_SOCKET", self.dir.path().join("test.sock"))
            .env("HOME", self.home())
            .env("USERPROFILE", self.home())
            .env_remove("DEVSERIAL_CONFIG")
            // A stray configuration file in the working directory must not
            // influence the tests.
            .current_dir(self.dir.path());
        for var in [
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
            "COPILOT_HOME",
            "XDG_CONFIG_HOME",
        ] {
            cmd.env_remove(var);
        }
        cmd
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
        let mut cmd = Command::from_std(self.std_command());
        cmd.timeout(std::time::Duration::from_secs(90));
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
        let _ = Command::from_std(self.std_command())
            .args(["daemon", "--stop"])
            .timeout(std::time::Duration::from_secs(10))
            .output();
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

/// A socket path longer than the system can address is explained, by the
/// command that needs the daemon and by the daemon itself, instead of failing
/// with `path must be shorter than SUN_LEN`.
#[cfg(unix)]
#[test]
fn a_socket_path_that_does_not_fit_is_explained() {
    let sandbox = Sandbox::new();
    let long = sandbox
        .dir
        .path()
        .join("s".repeat(120))
        .join("devserial.sock");

    let client = sandbox
        .command()
        .env("DEVSERIAL_SOCKET", &long)
        .args(["read", "/dev/ttyUSB0", "--tail", "1"])
        .output()
        .expect("run devserial read");
    assert_eq!(client.status.code(), Some(1));
    let text = String::from_utf8_lossy(&client.stderr);
    assert!(text.contains("socket path is"), "{text}");
    assert!(text.contains("--socket"), "{text}");
    assert!(!text.contains("did not become reachable"), "{text}");

    let daemon = sandbox
        .command()
        .env("DEVSERIAL_SOCKET", &long)
        .arg("daemon")
        .output()
        .expect("run devserial daemon");
    assert_eq!(daemon.status.code(), Some(1));
    let text = String::from_utf8_lossy(&daemon.stderr);
    assert!(text.contains("socket path is"), "{text}");
    assert!(!text.contains("SUN_LEN"), "{text}");
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

/// `delete` needs exactly one side, before anything is sent to a daemon, and
/// says that a copy is made unless it is declined.
#[test]
fn delete_needs_exactly_one_side_and_says_it_keeps_a_copy() {
    let sandbox = Sandbox::new();
    sandbox
        .command()
        .args(["delete", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--no-archive"))
        .stdout(predicate::str::contains("--dry-run"));
    // Neither side, and both sides, are refused without reaching a daemon.
    sandbox
        .command()
        .args(["delete", "/dev/x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--above"));
    sandbox
        .command()
        .args(["delete", "/dev/x", "--above", "1", "--below", "2"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
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
    let mut daemon = sandbox
        .std_command()
        .args(["daemon", "--idle-timeout", "1h"])
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
    let mut mcp = sandbox
        .std_command()
        .arg("mcp")
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

/// A devserial invocation that sees only the AI tools under `home`, which
/// is the sandbox's home.
fn skill_command(sandbox: &Sandbox, home: &std::path::Path) -> Command {
    assert_eq!(home, sandbox.home(), "the sandbox decides the home");
    sandbox.command()
}

fn skill_status(sandbox: &Sandbox, home: &std::path::Path) -> String {
    let output = skill_command(sandbox, home)
        .arg("skill")
        .output()
        .expect("status");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// #124: the skill goes only where the user agreed, and comes out again.
#[test]
fn the_skill_is_installed_only_with_consent() {
    let sandbox = Sandbox::new();
    let home = sandbox.home();
    std::fs::create_dir_all(home.join(".claude")).expect("claude");
    let skill = home.join(".claude/skills/devserial");

    let status = skill_status(&sandbox, &home);
    assert!(status.contains("Claude Code"), "{status}");
    assert!(status.contains("waiting for your consent"), "{status}");

    // No terminal to ask on and no --yes: nothing happens.
    skill_command(&sandbox, &home)
        .args(["skill", "--install"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--yes"));
    assert!(!skill.exists(), "installed without consent");

    skill_command(&sandbox, &home)
        .args(["skill", "--install", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("installed"));
    let skill_md = std::fs::read_to_string(skill.join("SKILL.md")).expect("SKILL.md");
    assert!(skill_md.contains("name: \"devserial\""), "{skill_md}");
    assert!(
        skill_md.contains("installed-by: \"devserial\""),
        "{skill_md}"
    );
    assert!(skill.join("references/cli.md").exists());
    assert!(skill.join("references/mcp.md").exists());
    assert!(skill_status(&sandbox, &home).contains("installed and up to date"));

    skill_command(&sandbox, &home)
        .args(["skill", "--remove"])
        .assert()
        .success()
        .stdout(predicates::str::contains("removed"));
    assert!(!skill.exists(), "the skill outlived --remove");
}

/// #124: a declined place is not offered again; a tool found later is asked
/// about on its own; an explicit install still goes everywhere.
#[test]
fn a_declined_place_is_not_offered_again() {
    let sandbox = Sandbox::new();
    let home = sandbox.home();
    std::fs::create_dir_all(home.join(".claude")).expect("claude");

    skill_command(&sandbox, &home)
        .args(["skill", "--decline"])
        .assert()
        .success();
    let status = skill_status(&sandbox, &home);
    assert!(status.contains("declined"), "{status}");
    assert!(!status.contains("waiting for your consent"), "{status}");

    std::fs::create_dir_all(home.join(".codex")).expect("codex");
    let status = skill_status(&sandbox, &home);
    assert!(status.contains("declined"), "{status}");
    assert!(status.contains("waiting for your consent"), "{status}");

    skill_command(&sandbox, &home)
        .args(["skill", "--install", "--yes"])
        .assert()
        .success();
    assert!(home.join(".claude/skills/devserial/SKILL.md").exists());
    assert!(home.join(".agents/skills/devserial/SKILL.md").exists());
}

/// #124: with no AI tool on the machine there is nothing to offer.
#[test]
fn without_an_ai_tool_nothing_is_offered() {
    let sandbox = Sandbox::new();
    let home = sandbox.home();
    std::fs::create_dir_all(&home).expect("home");
    let status = skill_status(&sandbox, &home);
    assert!(status.contains("No AI tool"), "{status}");
}

/// Install the skill under `home` as devserial 0.0.1 would have.
fn install_older_copy(sandbox: &Sandbox, home: &std::path::Path) -> std::path::PathBuf {
    use agent_skill_installer::{AgentSkill, Environment, Program};

    std::fs::create_dir_all(home.join(".claude")).expect("claude");
    let skill = devserial::agent_skill::skill().expect("skill");
    let older = Program::new("devserial", "0.0.1").expect("program");
    let agent = AgentSkill::new(skill, older, sandbox.dir.path().join("older-consent"))
        .expect("home")
        .with_environment(Environment::new(home));
    let installed = agent
        .offer()
        .expect("offer")
        .expect("a place to offer")
        .accept()
        .expect("install");
    assert_eq!(installed.len(), 1);
    let skill_md = home.join(".claude/skills/devserial/SKILL.md");
    assert_eq!(installed_version(&skill_md).as_deref(), Some("0.0.1"));
    skill_md
}

/// The version in a skill's installation record.
fn installed_version(skill_md: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(skill_md).ok()?;
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with("installed-version:"))?;
    Some(line.split('"').nth(1)?.to_string())
}

/// Wait until the skill carries this build's version.
fn updated(skill_md: &std::path::Path) -> bool {
    (0..100).any(|_| {
        if installed_version(skill_md).as_deref() == Some(env!("CARGO_PKG_VERSION")) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        false
    })
}

/// #124: a copy an older devserial installed is brought up to date, even
/// when no place is new.
#[test]
fn an_older_copy_is_updated_by_install() {
    let sandbox = Sandbox::new();
    let home = sandbox.home();
    install_older_copy(&sandbox, &home);

    skill_command(&sandbox, &home)
        .args(["skill", "--install", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("updated from 0.0.1"));
    assert!(skill_status(&sandbox, &home).contains("installed and up to date"));
}

/// #124: the daemon brings an older copy up to date when it starts, without
/// being asked. Which commands do is pinned down by
/// `long_running_commands_update_the_skill`; a one-shot command ends too soon
/// for a test here to tell.
#[test]
fn the_daemon_updates_the_skill_when_it_starts() {
    let sandbox = Sandbox::new();
    let home = sandbox.home();
    let skill_md = install_older_copy(&sandbox, &home);

    skill_command(&sandbox, &home)
        .args(["stats", "/dev/definitely-not-a-port"])
        .assert()
        .failure();
    assert!(sandbox.wait_for_daemon(true), "the daemon did not start");
    let done = updated(&skill_md);
    skill_command(&sandbox, &home)
        .args(["daemon", "--stop"])
        .assert()
        .success();
    assert!(done, "the daemon did not update the skill");
}

/// #124: in MCP mode the update happens without a byte on stdout, which
/// belongs to the protocol.
#[test]
fn the_mcp_server_updates_the_skill_without_writing_to_stdout() {
    let sandbox = Sandbox::new();
    let home = sandbox.home();
    let skill_md = install_older_copy(&sandbox, &home);

    let mut mcp = sandbox
        .std_command()
        .arg("mcp")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("mcp");
    let done = updated(&skill_md);

    // Closing stdin is the AI tool disconnecting.
    drop(mcp.stdin.take());
    let output = mcp.wait_with_output().expect("mcp output");
    skill_command(&sandbox, &home)
        .args(["daemon", "--stop"])
        .assert()
        .success();
    assert!(done, "the MCP server did not update the skill");
    assert!(
        output.stdout.is_empty(),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Start a daemon that claims another commit, as a stale build would (#115).
fn start_other_build(sandbox: &Sandbox) -> std::process::Child {
    let daemon = sandbox
        .std_command()
        .arg("daemon")
        .env("DEVSERIAL_TEST_COMMIT", "deadbeef0000")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("daemon");
    assert!(sandbox.wait_for_daemon(true), "the daemon did not start");
    daemon
}

fn stop(sandbox: &Sandbox, mut daemon: std::process::Child) {
    let _ = sandbox.command().args(["daemon", "--stop"]).output();
    let _ = daemon.wait();
}

/// #115: another build is named on stderr, `--json` on stdout stays
/// parseable, and the opt-out silences it.
#[test]
fn another_build_is_named_on_stderr_and_json_stays_clean() {
    let sandbox = Sandbox::new();
    let daemon = start_other_build(&sandbox);

    // The client claims a commit of its own, so the comparison does not
    // depend on whether this tree has git. A build from a source archive has
    // no commit, and the warning then says it cannot tell instead.
    let output = sandbox
        .command()
        .env("DEVSERIAL_TEST_COMMIT", "cafe00000000")
        .args(["list", "--json"])
        .output()
        .expect("list");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let parsed: Result<serde_json::Value, _> = serde_json::from_slice(&output.stdout);

    let quiet = sandbox
        .command()
        .env("DEVSERIAL_TEST_COMMIT", "cafe00000000")
        .env("DEVSERIAL_NO_VERSION_WARNING", "1")
        .args(["list", "--json"])
        .output()
        .expect("list");
    stop(&sandbox, daemon);

    assert!(output.status.success(), "{output:?}");
    assert!(
        parsed.is_ok(),
        "stdout is not JSON: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(stderr.contains("warning: the daemon is"), "{stderr}");
    assert!(stderr.contains("deadbeef0000"), "{stderr}");
    assert!(stderr.contains("devserial daemon --restart"), "{stderr}");
    assert!(
        !String::from_utf8_lossy(&quiet.stderr).contains("warning"),
        "the opt-out did not silence it: {}",
        String::from_utf8_lossy(&quiet.stderr)
    );
}

/// #115: the same build raises no warning.
#[test]
fn the_same_build_raises_no_warning() {
    let sandbox = Sandbox::new();
    start_daemon(&sandbox);
    let output = sandbox
        .command()
        .args(["list", "--json"])
        .output()
        .expect("list");
    let _ = sandbox.command().args(["daemon", "--stop"]).output();
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("warning"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// #115: `--restart` replaces the daemon with one of the caller's build.
#[test]
fn restart_brings_up_the_callers_build() {
    let sandbox = Sandbox::new();
    let mut daemon = start_other_build(&sandbox);
    let status = |sandbox: &Sandbox| {
        let output = sandbox
            .command()
            .args(["daemon", "--status"])
            .output()
            .expect("status");
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let before = status(&sandbox);

    let restarted = sandbox
        .command()
        .args(["daemon", "--restart"])
        .output()
        .expect("restart");
    let exited = (0..100).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(50));
        matches!(daemon.try_wait(), Ok(Some(_)))
    });
    let after = status(&sandbox);
    let _ = sandbox.command().args(["daemon", "--stop"]).output();
    if !exited {
        let _ = daemon.kill();
    }

    assert!(before.contains("deadbeef0000"), "{before}");
    assert!(restarted.status.success(), "{restarted:?}");
    let said = String::from_utf8_lossy(&restarted.stdout);
    assert!(said.contains("Daemon restarted."), "{said}");
    assert!(exited, "the old daemon kept running");
    assert!(after.contains("RUNNING"), "{after}");
    assert!(
        !after.contains("deadbeef0000"),
        "still the other build: {after}"
    );
}
