// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, watch};

use crate::config::PortConfig;
use crate::storage::SqliteStorage;

/// Maximum reconnect backoff duration.
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(30);

/// Connection state of a port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionState {
    Connected,
    Disconnected { since_ms: u64, attempts: u32 },
    Reconnecting,
}

/// A line ready to be stored.
#[derive(Debug, Clone)]
pub struct IngestedLine {
    pub timestamp_ns: i64,
    pub payload: String,
}

/// A boxed future that resolves to an optional reader.
pub type ReconnectFuture<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>>
            + Send
            + 'a,
    >,
>;

/// Factory for creating new readers on reconnect.
pub trait ReaderFactory: Send + 'static {
    /// Attempt to create a new reader (reconnect).
    fn try_connect(&self) -> ReconnectFuture<'_>;
}

/// A no-op factory that never reconnects (for mock/test usage).
pub struct NoReconnect;

impl ReaderFactory for NoReconnect {
    fn try_connect(&self) -> ReconnectFuture<'_> {
        Box::pin(async { None })
    }
}

/// Handle to a running port reader actor.
pub struct PortReaderHandle {
    pub state_rx: watch::Receiver<ConnectionState>,
    shutdown_tx: mpsc::Sender<()>,
}

impl PortReaderHandle {
    /// Signal the reader to shut down gracefully.
    pub async fn shutdown(&self) {
        self.shutdown_tx.send(()).await.ok();
    }

    /// Get current connection state.
    #[must_use]
    pub fn state(&self) -> ConnectionState {
        self.state_rx.borrow().clone()
    }
}

/// Spawn a reader task that reads from an `AsyncRead` source, splits lines,
/// and writes them to storage. Supports auto-reconnect via a `ReaderFactory`.
pub fn spawn_reader<R>(
    reader: R,
    storage: &Arc<std::sync::Mutex<SqliteStorage>>,
    config: &PortConfig,
) -> PortReaderHandle
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    spawn_reader_with_reconnect(reader, storage, config, NoReconnect)
}

/// Spawn a reader with reconnect support.
pub fn spawn_reader_with_reconnect<R, F>(
    reader: R,
    storage: &Arc<std::sync::Mutex<SqliteStorage>>,
    config: &PortConfig,
    factory: F,
) -> PortReaderHandle
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    F: ReaderFactory,
{
    let (state_tx, state_rx) = watch::channel(ConnectionState::Connected);
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);
    let (line_tx, mut line_rx) = mpsc::channel::<Vec<IngestedLine>>(100);
    let delimiter = config.delimiter;
    let auto_reconnect = config.auto_reconnect;
    let base_interval_ms = config.reconnect_interval_ms;
    let max_buffer_lines = config.max_buffer_lines;

    // Writer task: receives batches and writes to storage
    let writer_storage = Arc::clone(storage);
    tokio::spawn(async move {
        let mut batches_since_trim: u32 = 0;
        while let Some(batch) = line_rx.recv().await {
            let storage = Arc::clone(&writer_storage);
            let trim = if max_buffer_lines > 0 {
                batches_since_trim += 1;
                // Trim every 100 batches (~10k lines at batch size 100)
                batches_since_trim % 100 == 0
            } else {
                false
            };
            tokio::task::spawn_blocking(move || {
                let s = storage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let lines: Vec<(i64, &str)> = batch
                    .iter()
                    .map(|l| (l.timestamp_ns, l.payload.as_str()))
                    .collect();
                if let Err(e) = s.insert_lines(&lines) {
                    tracing::error!("storage insert error: {e}");
                }
                if trim {
                    s.trim_lines(max_buffer_lines).ok();
                }
            })
            .await
            .ok();
        }
    });

    // Reader task with reconnect loop
    tokio::spawn(async move {
        let boxed: Box<dyn tokio::io::AsyncRead + Unpin + Send> = Box::new(reader);
        reader_with_reconnect(
            boxed,
            delimiter,
            auto_reconnect,
            base_interval_ms,
            line_tx,
            state_tx,
            shutdown_rx,
            factory,
        )
        .await;
    });

    PortReaderHandle {
        state_rx,
        shutdown_tx,
    }
}

#[allow(clippy::too_many_arguments)]
async fn reader_with_reconnect<F: ReaderFactory>(
    initial_reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    delimiter: u8,
    auto_reconnect: bool,
    base_interval_ms: u64,
    line_tx: mpsc::Sender<Vec<IngestedLine>>,
    state_tx: watch::Sender<ConnectionState>,
    mut shutdown_rx: mpsc::Receiver<()>,
    factory: F,
) {
    let mut reader = initial_reader;

    loop {
        // Read until disconnect or shutdown
        let disconnected = reader_loop(
            &mut reader,
            delimiter,
            &line_tx,
            &state_tx,
            &mut shutdown_rx,
        )
        .await;

        if !disconnected || !auto_reconnect {
            break;
        }

        // Reconnect loop with exponential backoff
        let disconnect_start = std::time::Instant::now();
        let mut attempts: u32 = 0;

        loop {
            attempts = attempts.saturating_add(1);
            let since_ms =
                u64::try_from(disconnect_start.elapsed().as_millis()).unwrap_or(u64::MAX);
            state_tx.send_replace(ConnectionState::Disconnected { since_ms, attempts });

            let backoff =
                Duration::from_millis(base_interval_ms.saturating_mul(u64::from(attempts.min(15))))
                    .min(MAX_RECONNECT_BACKOFF);

            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => return,
                () = tokio::time::sleep(backoff) => {}
            }

            state_tx.send_replace(ConnectionState::Reconnecting);
            tracing::info!(attempts, "attempting reconnect");

            if let Some(new_reader) = factory.try_connect().await {
                reader = new_reader;
                state_tx.send_replace(ConnectionState::Connected);
                tracing::info!(attempts, "reconnected");
                break;
            }
        }
    }
}

/// Returns `true` if disconnected (read error), `false` if shutdown or EOF.
async fn reader_loop(
    reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
    delimiter: u8,
    line_tx: &mpsc::Sender<Vec<IngestedLine>>,
    state_tx: &watch::Sender<ConnectionState>,
    shutdown_rx: &mut mpsc::Receiver<()>,
) -> bool {
    let mut buf = vec![0u8; 4096];
    let mut partial = Vec::new();
    let mut batch = Vec::new();
    let flush_interval = Duration::from_millis(100);

    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.recv() => {
                flush_batch(&mut batch, &mut partial, line_tx).await;
                return false;
            }
            result = reader.read(&mut buf) => {
                match result {
                    Ok(0) => {
                        flush_batch(&mut batch, &mut partial, line_tx).await;
                        return false;
                    }
                    Ok(n) => {
                        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
                        process_bytes(&buf[..n], delimiter, now_ns, &mut partial, &mut batch);

                        if batch.len() >= 1000
                            && line_tx.send(std::mem::take(&mut batch)).await.is_err()
                        {
                            return false;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("read error: {e}");
                        flush_batch(&mut batch, &mut partial, line_tx).await;
                        state_tx.send_replace(ConnectionState::Disconnected {
                            since_ms: 0,
                            attempts: 0,
                        });
                        return true;
                    }
                }
            }
            () = tokio::time::sleep(flush_interval) => {
                if !batch.is_empty()
                    && line_tx.send(std::mem::take(&mut batch)).await.is_err()
                {
                    return false;
                }
            }
        }
    }
}

fn process_bytes(
    data: &[u8],
    delimiter: u8,
    timestamp_ns: i64,
    partial: &mut Vec<u8>,
    batch: &mut Vec<IngestedLine>,
) {
    for &byte in data {
        if byte == delimiter {
            let payload = String::from_utf8_lossy(partial).into_owned();
            batch.push(IngestedLine {
                timestamp_ns,
                payload,
            });
            partial.clear();
        } else {
            partial.push(byte);
        }
    }
}

async fn flush_batch(
    batch: &mut Vec<IngestedLine>,
    partial: &mut Vec<u8>,
    line_tx: &mpsc::Sender<Vec<IngestedLine>>,
) {
    if !partial.is_empty() {
        let payload = String::from_utf8_lossy(partial).into_owned();
        batch.push(IngestedLine {
            timestamp_ns: chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
            payload,
        });
        partial.clear();
    }
    if !batch.is_empty() {
        line_tx.send(std::mem::take(batch)).await.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::mock_serial::mock_serial;

    fn test_storage() -> Arc<std::sync::Mutex<SqliteStorage>> {
        Arc::new(std::sync::Mutex::new(SqliteStorage::open_memory().unwrap()))
    }

    fn test_config() -> PortConfig {
        PortConfig::default()
    }

    #[tokio::test]
    async fn test_read_lines_from_mock() {
        let (mock, ctrl) = mock_serial(100);
        let storage = test_storage();
        let handle = spawn_reader(mock, &storage, &test_config());

        ctrl.feed_lines(&["hello", "world", "test"]).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.shutdown().await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let lines = storage.lock().unwrap().read_lines(1, 10).unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].payload, "hello");
        assert_eq!(lines[1].payload, "world");
        assert_eq!(lines[2].payload, "test");
    }

    #[tokio::test]
    async fn test_partial_line_buffering() {
        let (mock, ctrl) = mock_serial(100);
        let storage = test_storage();
        let handle = spawn_reader(mock, &storage, &test_config());

        ctrl.feed_data(b"partial").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        ctrl.feed_data(b" line\n").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.shutdown().await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let lines = storage.lock().unwrap().read_lines(1, 10).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].payload, "partial line");
    }

    #[tokio::test]
    async fn test_disconnect_state_change() {
        let (mock, ctrl) = mock_serial(100);
        let storage = test_storage();
        let handle = spawn_reader(mock, &storage, &test_config());

        assert_eq!(handle.state(), ConnectionState::Connected);
        ctrl.simulate_disconnect();
        ctrl.feed_data(b"x").await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        let state = handle.state();
        assert!(matches!(state, ConnectionState::Disconnected { .. }));
    }

    #[tokio::test]
    async fn test_timestamps_monotonic() {
        let (mock, ctrl) = mock_serial(100);
        let storage = test_storage();
        let handle = spawn_reader(mock, &storage, &test_config());

        for i in 0..10 {
            ctrl.feed_lines(&[&format!("line {i}")]).await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.shutdown().await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let lines = storage.lock().unwrap().read_lines(1, 10).unwrap();
        assert_eq!(lines.len(), 10);
        for window in lines.windows(2) {
            assert!(window[1].timestamp_ns >= window[0].timestamp_ns);
        }
    }

    #[tokio::test]
    async fn test_binary_data_no_panic() {
        let (mock, ctrl) = mock_serial(100);
        let storage = test_storage();
        let handle = spawn_reader(mock, &storage, &test_config());

        ctrl.feed_data(&[0xFF, 0xFE, 0x00, b'\n']).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.shutdown().await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let lines = storage.lock().unwrap().read_lines(1, 10).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].payload.is_empty());
    }

    #[tokio::test]
    async fn test_many_lines_no_loss() {
        let (mock, ctrl) = mock_serial(10_000);
        let storage = test_storage();
        let handle = spawn_reader(mock, &storage, &test_config());

        for i in 0..1000 {
            ctrl.feed_lines(&[&format!("line {i}")]).await;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        handle.shutdown().await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        let stats = storage.lock().unwrap().get_stats().unwrap();
        assert_eq!(stats.total_lines, 1000);
    }

    #[tokio::test]
    async fn test_auto_reconnect() {
        use std::sync::atomic::{AtomicU32, Ordering};

        struct TestFactory {
            count: Arc<AtomicU32>,
            ctrl: crate::testutil::mock_serial::MockSerialControl,
        }

        impl ReaderFactory for TestFactory {
            fn try_connect(&self) -> ReconnectFuture<'_> {
                Box::pin(async {
                    let n = self.count.fetch_add(1, Ordering::SeqCst);
                    if n >= 1 {
                        self.ctrl.simulate_reconnect();
                        let (new_mock, _) = mock_serial(100);
                        Some(Box::new(new_mock) as Box<dyn tokio::io::AsyncRead + Unpin + Send>)
                    } else {
                        None
                    }
                })
            }
        }

        let storage = test_storage();
        let (mock, ctrl) = mock_serial(100);

        let connect_count = Arc::new(AtomicU32::new(0));
        let ctrl_clone = ctrl.clone();

        let mut config = test_config();
        config.auto_reconnect = true;
        config.reconnect_interval_ms = 50;

        let factory = TestFactory {
            count: Arc::clone(&connect_count),
            ctrl: ctrl_clone,
        };

        let handle = spawn_reader_with_reconnect(mock, &storage, &config, factory);

        // Trigger disconnect
        ctrl.simulate_disconnect();
        ctrl.feed_data(b"x").await;
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Should have reconnected
        assert_eq!(handle.state(), ConnectionState::Connected);
        assert!(connect_count.load(Ordering::SeqCst) >= 2);

        handle.shutdown().await;
    }
}
