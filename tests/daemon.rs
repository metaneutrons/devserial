// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::sync::{Arc, Mutex};
use std::time::Duration;

use devserial::config::{Config, PortConfig};
use devserial::engine::CommandEngine;
use devserial::ipc::{IpcClient, IpcServer};
use devserial::port_manager::PortManagerHandle;
use devserial::protocol::{RequestPayload, ResponsePayload};
use devserial::state::StateDb;
use devserial::storage::SqliteStorage;
use devserial::testutil::mock_serial::mock_serial;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_daemon_full_rpc_lifecycle() {
    let _ = std::fs::create_dir_all("./target/tmp");
    let dir = tempfile::Builder::new().tempdir_in("./target/tmp").unwrap();
    let socket_path = dir.path().join("daemon_lifecycle.sock");
    let pid_path = dir.path().join("daemon_lifecycle.pid");

    let pm = PortManagerHandle::new();
    let state_db = Arc::new(Mutex::new(StateDb::open_memory().unwrap()));
    let config = Arc::new(Config::default());
    let engine = CommandEngine::new(pm.clone(), state_db, config, dir.path().to_path_buf());

    let server = IpcServer::new(engine, socket_path.clone(), pid_path.clone());
    let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);

    tokio::spawn(async move {
        let _ = server.run(shutdown_rx).await;
    });

    // Wait for server socket readiness
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = IpcClient::new(socket_path);

    // 1. Ping
    let pong = client.send(RequestPayload::Ping).await.unwrap();
    assert_eq!(pong, ResponsePayload::Pong);

    // 2. Open mock port
    let (mock, handle) = mock_serial(1024);
    let storage = Arc::new(Mutex::new(SqliteStorage::open_memory().unwrap()));
    pm.open(
        "mock_daemon_port".into(),
        Box::new(mock),
        PortConfig::default(),
        storage,
    )
    .await
    .unwrap();

    // 3. List ports
    let list = client.send(RequestPayload::ListPorts).await.unwrap();
    if let ResponsePayload::PortList(ports) = list {
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].name, "mock_daemon_port");
    } else {
        panic!("expected PortList");
    }

    // 4. Feed data into mock serial
    handle
        .feed_lines(&[
            "[INIT] system ready",
            "[INFO] connected to server",
            "[ERROR] packet drop",
        ])
        .await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    // 5. Read lines
    let read_resp = client
        .send(RequestPayload::ReadLines {
            port: "mock_daemon_port".into(),
            start_id: Some(1),
            count: Some(10),
            tail: None,
        })
        .await
        .unwrap();

    if let ResponsePayload::Lines(lines) = read_resp {
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].payload, "[INIT] system ready");
        assert_eq!(lines[2].payload, "[ERROR] packet drop");
    } else {
        panic!("expected Lines");
    }

    // 6. Search
    let search_resp = client
        .send(RequestPayload::Search {
            port: "mock_daemon_port".into(),
            query: "ERROR".into(),
            is_regex: false,
            start_ns: None,
            end_ns: None,
            limit: Some(10),
        })
        .await
        .unwrap();

    if let ResponsePayload::SearchResults(results) = search_resp {
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].payload, "[ERROR] packet drop");
    } else {
        panic!("expected SearchResults");
    }

    // 7. Stats
    let stats_resp = client
        .send(RequestPayload::GetStats {
            port: "mock_daemon_port".into(),
        })
        .await
        .unwrap();

    if let ResponsePayload::Stats(stats) = stats_resp {
        assert_eq!(stats.total_lines, 3);
    } else {
        panic!("expected Stats");
    }

    // 8. Test Break and Transfer error handling on closed or mock ports
    let break_resp = client
        .send(RequestPayload::SendBreak {
            port: "mock_daemon_port".into(),
            duration_ms: Some(50),
        })
        .await;
    // On mock port with no real hardware handle, it returns an error Result
    assert!(break_resp.is_err());

    let send_resp = client
        .send(RequestPayload::SendFile {
            port: "nonexistent".into(),
            file_path: "/tmp/test.bin".into(),
            protocol: devserial::modem::FileTransferProtocol::Zmodem,
        })
        .await;
    assert!(send_resp.is_err());

    // 9. Close port
    let close_resp = client
        .send(RequestPayload::ClosePort {
            name: "mock_daemon_port".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        close_resp,
        ResponsePayload::PortClosed {
            name: "mock_daemon_port".into()
        }
    );

    // 10. Shutdown
    let _ = shutdown_tx.send(());
}
