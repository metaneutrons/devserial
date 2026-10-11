// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Foreground execution of the background daemon.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::cli::{CliError, bootstrap};
use crate::engine::CommandEngine;
use crate::ipc::IpcServer;
use crate::paths;
use crate::protocol::IdleSource;
use crate::state::StateDb;

/// How often the daemon looks at its idle timer.
///
/// Often enough that an idle stop comes within seconds of being due, rarely
/// enough that the line counts it reads cost nothing.
const IDLE_CHECK: Duration = Duration::from_secs(5);

/// Run the daemon until it is asked to shut down or has been idle too long.
///
/// `idle_timeout` overrides the configured idle period for this run.
///
/// # Errors
/// Returns an error on initialization or endpoint failure.
pub fn run_daemon(
    socket: Option<PathBuf>,
    config_path: Option<&Path>,
    idle_timeout: Option<Duration>,
) -> Result<(), CliError> {
    let startup = bootstrap::start(config_path, "daemon")?;
    let config = Arc::clone(&startup.config);
    let endpoint = socket.unwrap_or_else(paths::default_socket_path);
    let pid_path = paths::pid_path(&endpoint);

    startup.runtime.block_on(async move {
        let port_manager = bootstrap::port_manager(&config);
        let state_db = Arc::new(Mutex::new(
            StateDb::open(&config.global.data_dir).map_err(|e| CliError::msg(e.to_string()))?,
        ));

        let restored = bootstrap::restore_ports(&port_manager, &state_db, &config).await;
        if restored > 0 {
            tracing::info!(restored, "restored previously open ports");
        }

        let engine = CommandEngine::new(port_manager, state_db, Arc::clone(&config));
        if let Some(period) = idle_timeout {
            engine.lifecycle().set_period(period, IdleSource::StartFlag);
        }

        // Starts the HTTP interface if the configuration asks for it. A failed
        // bind is logged and kept in the state rather than ending the daemon,
        // which exists to hold serial ports.
        #[cfg(feature = "rest")]
        engine.rest_autostart();

        let watched = engine.clone();
        let server = IpcServer::new(engine, endpoint, pid_path);
        let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);

        let on_signal = shutdown_tx.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = on_signal.send(());
            }
        });
        tokio::spawn(watch_idle(watched, IDLE_CHECK, shutdown_tx));

        server.run(shutdown_rx).await?;
        Ok(())
    })
}

/// Stop the daemon once it has been idle for its idle period (#114).
///
/// The stop goes through the same channel as Ctrl+C, so it leaves the same
/// trail: the endpoint and the PID file are removed, and the ports stay
/// recorded as open, to be restored on the next start.
pub async fn watch_idle(
    engine: CommandEngine,
    every: Duration,
    shutdown: tokio::sync::broadcast::Sender<()>,
) {
    let mut ticks = tokio::time::interval(every);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        if engine.idle_expired().await {
            let state = engine.lifecycle().state();
            tracing::info!(
                idle_seconds = state.idle_seconds,
                source = %state.source,
                "idle for the whole idle period, stopping"
            );
            let _ = shutdown.send(());
            return;
        }
    }
}

#[cfg(all(test, unix, feature = "testutil"))]
mod tests {
    use super::*;
    use crate::lifecycle::Lifecycle;
    use crate::lifecycle::tests::ManualClock;
    use crate::protocol::{LeaseKind, RequestPayload};

    const MIN: Duration = Duration::from_secs(60);

    /// A daemon on a temporary socket with one open mock port, recorded for
    /// restore, whose idle timer runs on a clock the test moves.
    struct Idle {
        _dir: tempfile::TempDir,
        socket: PathBuf,
        state: Arc<Mutex<StateDb>>,
        engine: CommandEngine,
        clock: Arc<ManualClock>,
        feed: crate::testutil::mock_serial::MockSerialControl,
        server: tokio::task::JoinHandle<()>,
        /// Held as the daemon's Ctrl+C task holds it. Without a second sender
        /// the channel closes when the watcher returns, the server stops on
        /// the closed channel, and a watcher that never sent would pass.
        _signal: tokio::sync::broadcast::Sender<()>,
    }

    impl Idle {
        async fn start() -> Self {
            let dir = tempfile::tempdir().expect("dir");
            let socket = dir.path().join("idle.sock");
            let mut settings = crate::config::Config::default();
            settings.global.data_dir = dir.path().to_path_buf();
            settings.global.flush_interval_ms = 10;
            let config = Arc::new(settings);

            let ports = crate::port_manager::PortManagerHandle::new();
            let (mock, feed) = crate::testutil::mock_serial::mock_serial(64);
            let storage = Arc::new(Mutex::new(
                crate::storage::SqliteStorage::open(&dir.path().join("idle.db")).expect("db"),
            ));
            ports
                .open(
                    "mock_idle".into(),
                    mock,
                    crate::config::PortConfig::default(),
                    storage,
                )
                .await
                .expect("open");
            let state = Arc::new(Mutex::new(StateDb::open(dir.path()).expect("state")));
            state
                .lock()
                .unwrap()
                .port_opened("mock_idle", &crate::config::PortConfig::default())
                .expect("record");

            let clock = ManualClock::new();
            let engine = CommandEngine::new(ports, Arc::clone(&state), config)
                .with_lifecycle(Lifecycle::new(15 * MIN, IdleSource::Default, clock.clone()));
            let server =
                IpcServer::new(engine.clone(), socket.clone(), dir.path().join("idle.pid"));
            let (tx, rx) = tokio::sync::broadcast::channel(1);
            let server = tokio::spawn(async move {
                server.run(rx).await.expect("server");
            });
            while !socket.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let signal = tx.clone();
            tokio::spawn(watch_idle(engine.clone(), Duration::from_millis(20), tx));
            Self {
                _dir: dir,
                socket,
                state,
                engine,
                clock,
                feed,
                server,
                _signal: signal,
            }
        }

        /// Give the watcher a few ticks to see what the clock and the port say.
        async fn settle() {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }

        async fn advance(&self, by: Duration) {
            self.clock.advance(by);
            Self::settle().await;
        }

        fn running(&self) -> bool {
            !self.server.is_finished() && self.socket.exists()
        }

        fn recorded(&self) -> Vec<String> {
            self.state
                .lock()
                .unwrap()
                .active_ports()
                .expect("ports")
                .into_iter()
                .map(|entry| entry.name)
                .collect()
        }
    }

    /// The whole idle period with a silent port and nobody asking ends the
    /// daemon the way `--stop` does, and the port stays recorded for restore.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_idle_daemon_stops_and_keeps_its_ports_for_restore() {
        let daemon = Idle::start().await;
        Idle::settle().await;
        daemon.advance(14 * MIN).await;
        assert!(daemon.running(), "stopped before the idle period was over");

        daemon.advance(MIN).await;
        tokio::time::timeout(Duration::from_secs(2), daemon.server)
            .await
            .expect("the daemon did not stop")
            .expect("server task");
        assert!(!daemon.socket.exists(), "the endpoint was left behind");
        assert_eq!(
            daemon_recorded(&daemon.state),
            vec!["mock_idle".to_string()]
        );
    }

    fn daemon_recorded(state: &Arc<Mutex<StateDb>>) -> Vec<String> {
        state
            .lock()
            .unwrap()
            .active_ports()
            .expect("ports")
            .into_iter()
            .map(|entry| entry.name)
            .collect()
    }

    /// Data arriving on the port resets the timer.
    #[tokio::test(flavor = "multi_thread")]
    async fn arriving_data_keeps_the_daemon() {
        let daemon = Idle::start().await;
        Idle::settle().await;
        daemon.advance(14 * MIN).await;
        daemon.feed.feed_lines(&["boot ok"]).await;
        Idle::settle().await;
        daemon.advance(14 * MIN).await;
        assert!(daemon.running(), "data did not reset the idle timer");
        assert_eq!(daemon.recorded(), vec!["mock_idle".to_string()]);
    }

    /// A request resets the timer.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_keeps_the_daemon() {
        let daemon = Idle::start().await;
        daemon.advance(14 * MIN).await;
        daemon
            .engine
            .execute(RequestPayload::ListPorts)
            .await
            .expect("request");
        daemon.advance(14 * MIN).await;
        assert!(daemon.running(), "a request did not reset the idle timer");
    }

    /// A renewed lease holds the daemon past any idle period, and once it is
    /// released the idle period runs from there.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_lease_holds_the_daemon_until_released() {
        let daemon = Idle::start().await;
        for _ in 0..30 {
            daemon.engine.lifecycle().renew("tui-test", LeaseKind::Tui);
            daemon.clock.advance(MIN);
        }
        Idle::settle().await;
        assert!(daemon.running(), "a held lease did not keep the daemon");

        daemon.engine.lifecycle().release("tui-test");
        daemon.advance(15 * MIN).await;
        tokio::time::timeout(Duration::from_secs(2), daemon.server)
            .await
            .expect("the daemon did not stop after the release")
            .expect("server task");
    }

    /// A client that is killed renews no more and never releases. Its lease
    /// holds for three minutes after the last renewal and then lapses. The
    /// idle period is one minute here, so the lease alone holds the daemon in
    /// between. Renewals and the stop go through the real endpoint and
    /// watcher; only the clock is the test's.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_killed_client_releases_the_daemon_once_its_lease_lapses() {
        let daemon = Idle::start().await;
        daemon
            .engine
            .execute(RequestPayload::SetIdleTimeout { seconds: 60 })
            .await
            .expect("set");
        let client = crate::ipc::IpcClient::new(daemon.socket.clone());
        for _ in 0..5 {
            client
                .send(RequestPayload::RenewLease {
                    client: "killed-tui".to_string(),
                    kind: LeaseKind::Tui,
                })
                .await
                .expect("renew");
            daemon.advance(MIN).await;
        }
        // Killed one minute ago: no more renewals, and no release.
        daemon.advance(MIN).await;
        assert!(
            daemon.running(),
            "the lease stopped holding before three minutes"
        );

        daemon.advance(MIN).await;
        tokio::time::timeout(Duration::from_secs(2), daemon.server)
            .await
            .expect("the daemon did not stop once the lease had lapsed")
            .expect("server task");
    }

    /// `0` turns the idle stop off.
    #[tokio::test(flavor = "multi_thread")]
    async fn zero_never_stops() {
        let daemon = Idle::start().await;
        daemon
            .engine
            .execute(RequestPayload::SetIdleTimeout { seconds: 0 })
            .await
            .expect("set");
        daemon.advance(24 * 60 * MIN).await;
        assert!(daemon.running());
    }
}
