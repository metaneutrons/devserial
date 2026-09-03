// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Integration test: full lifecycle via public APIs.

use std::sync::Arc;
use std::time::Duration;

use devserial::config::PortConfig;
use devserial::port_manager::PortManagerHandle;
use devserial::reader::ConnectionState;
use devserial::storage::SqliteStorage;
use devserial::testutil::mock_serial::mock_serial;

/// Full lifecycle: open → stream → read → search → export → clear → close.
#[tokio::test]
async fn test_full_lifecycle() {
    let mgr = PortManagerHandle::new();
    let (mock, ctrl) = mock_serial(10_000);
    let storage = Arc::new(std::sync::Mutex::new(SqliteStorage::open_memory().unwrap()));

    // Open
    mgr.open(
        "test_port".into(),
        mock,
        PortConfig::default(),
        Arc::clone(&storage),
    )
    .await
    .unwrap();

    // Stream data
    for i in 0..500 {
        let line = if i % 50 == 0 {
            format!("[ERROR] failure at step {i}")
        } else {
            format!("[INFO] heartbeat #{i}")
        };
        ctrl.feed_lines(&[&line]).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify state
    let conn_state = mgr.get_state("test_port").await.unwrap();
    assert_eq!(conn_state, ConnectionState::Connected);

    // Read from storage
    let lines = storage.lock().unwrap().read_lines(1, 10).unwrap();
    assert_eq!(lines.len(), 10);
    assert!(lines[0].payload.contains("[ERROR] failure at step 0"));

    // Get stats
    let stats = storage.lock().unwrap().get_stats().unwrap();
    assert_eq!(stats.total_lines, 500);

    // Search
    let results = storage
        .lock()
        .unwrap()
        .search_substring("ERROR", None, 100)
        .unwrap();
    assert_eq!(results.len(), 10);

    // Export
    let dir = tempfile::tempdir().unwrap();
    let export_path = dir.path().join("export.jsonl");
    let lines_to_export = storage.lock().unwrap().export_range(1, 100).unwrap();
    assert_eq!(lines_to_export.len(), 100);

    // Write export file
    {
        use std::io::Write;
        let file = std::fs::File::create(&export_path).unwrap();
        let mut writer = std::io::BufWriter::new(file);
        for line in &lines_to_export {
            writeln!(writer, "{}", line.payload).unwrap();
        }
    }
    let content = std::fs::read_to_string(&export_path).unwrap();
    assert_eq!(content.lines().count(), 100);

    // Clear
    storage.lock().unwrap().clear(None).unwrap();
    let stats = storage.lock().unwrap().get_stats().unwrap();
    assert_eq!(stats.total_lines, 0);

    // Close
    mgr.close("test_port").await.unwrap();
    let ports = mgr.list().await;
    assert!(ports.is_empty());
}

/// Multi-port: two ports streaming simultaneously, no cross-contamination.
#[tokio::test]
async fn test_multi_port_isolation() {
    let mgr = PortManagerHandle::new();

    let (mock1, ctrl1) = mock_serial(1000);
    let (mock2, ctrl2) = mock_serial(1000);
    let storage1 = Arc::new(std::sync::Mutex::new(SqliteStorage::open_memory().unwrap()));
    let storage2 = Arc::new(std::sync::Mutex::new(SqliteStorage::open_memory().unwrap()));

    mgr.open(
        "port1".into(),
        mock1,
        PortConfig::default(),
        Arc::clone(&storage1),
    )
    .await
    .unwrap();
    mgr.open(
        "port2".into(),
        mock2,
        PortConfig::default(),
        Arc::clone(&storage2),
    )
    .await
    .unwrap();

    // Stream different data to each port
    for i in 0..100 {
        ctrl1.feed_lines(&[&format!("PORT1-line-{i}")]).await;
        ctrl2.feed_lines(&[&format!("PORT2-line-{i}")]).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify isolation
    let lines1 = storage1.lock().unwrap().read_lines(1, 200).unwrap();
    let lines2 = storage2.lock().unwrap().read_lines(1, 200).unwrap();

    assert_eq!(lines1.len(), 100);
    assert_eq!(lines2.len(), 100);
    assert!(lines1.iter().all(|l| l.payload.starts_with("PORT1-")));
    assert!(lines2.iter().all(|l| l.payload.starts_with("PORT2-")));

    mgr.close("port1").await.unwrap();
    mgr.close("port2").await.unwrap();
}

/// Disconnect and reconnect: state transitions correctly.
#[tokio::test]
async fn test_disconnect_recovery() {
    let mgr = PortManagerHandle::new();
    let (mock, ctrl) = mock_serial(100);
    let storage = Arc::new(std::sync::Mutex::new(SqliteStorage::open_memory().unwrap()));

    mgr.open(
        "port1".into(),
        mock,
        PortConfig::default(),
        Arc::clone(&storage),
    )
    .await
    .unwrap();

    // Feed some data
    ctrl.feed_lines(&["before disconnect"]).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Disconnect
    ctrl.simulate_disconnect();
    ctrl.feed_data(b"trigger").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let conn_state = mgr.get_state("port1").await.unwrap();
    assert!(matches!(conn_state, ConnectionState::Disconnected { .. }));

    // Verify data before disconnect was stored
    let stats = storage.lock().unwrap().get_stats().unwrap();
    assert_eq!(stats.total_lines, 1);

    mgr.close("port1").await.unwrap();
}
