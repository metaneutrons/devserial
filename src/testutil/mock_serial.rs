// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Mutex;
use tokio::sync::mpsc;

/// Mock serial port for testing. Simulates a serial device with injectable data
/// and controllable disconnect/reconnect behavior.
pub struct MockSerial {
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,
    written: Arc<Mutex<Vec<u8>>>,
    disconnected: Arc<AtomicBool>,
    pending_buf: Mutex<Vec<u8>>,
}

/// Handle for controlling the mock from test code.
#[derive(Clone)]
pub struct MockSerialControl {
    feed_tx: mpsc::Sender<Vec<u8>>,
    written: Arc<Mutex<Vec<u8>>>,
    disconnected: Arc<AtomicBool>,
}

/// Create a new mock serial port and its control handle.
#[must_use]
pub fn mock_serial(buffer_size: usize) -> (MockSerial, MockSerialControl) {
    let (feed_tx, feed_rx) = mpsc::channel(buffer_size);
    let written = Arc::new(Mutex::new(Vec::new()));
    let disconnected = Arc::new(AtomicBool::new(false));

    let mock = MockSerial {
        rx: Mutex::new(feed_rx),
        written: Arc::clone(&written),
        disconnected: Arc::clone(&disconnected),
        pending_buf: Mutex::new(Vec::new()),
    };

    let control = MockSerialControl {
        feed_tx,
        written,
        disconnected,
    };

    (mock, control)
}

impl MockSerialControl {
    /// Inject raw bytes as if received from hardware.
    pub async fn feed_data(&self, data: &[u8]) {
        self.feed_tx.send(data.to_vec()).await.ok();
    }

    /// Inject newline-terminated lines.
    pub async fn feed_lines(&self, lines: &[&str]) {
        for line in lines {
            let mut data = line.as_bytes().to_vec();
            data.push(b'\n');
            self.feed_data(&data).await;
        }
    }

    /// Get all bytes that were "written" to the device.
    pub async fn drain_written(&self) -> Vec<u8> {
        let mut w = self.written.lock().await;
        std::mem::take(&mut *w)
    }

    /// Simulate a device disconnect — reads will return `BrokenPipe`.
    pub fn simulate_disconnect(&self) {
        self.disconnected.store(true, Ordering::SeqCst);
    }

    /// Simulate reconnection — reads resume normally.
    pub fn simulate_reconnect(&self) {
        self.disconnected.store(false, Ordering::SeqCst);
    }

    /// Check if currently in disconnected state.
    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::SeqCst)
    }
}

impl AsyncRead for MockSerial {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.disconnected.load(Ordering::SeqCst) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "simulated disconnect",
            )));
        }

        let this = self.get_mut();

        // First drain any pending buffer from a previous partial read
        let Ok(mut pending) = this.pending_buf.try_lock() else {
            return Poll::Pending;
        };

        if !pending.is_empty() {
            let to_copy = pending.len().min(buf.remaining());
            buf.put_slice(&pending[..to_copy]);
            pending.drain(..to_copy);
            return Poll::Ready(Ok(()));
        }
        drop(pending);

        // Try to receive new data
        let Ok(mut rx) = this.rx.try_lock() else {
            return Poll::Pending;
        };

        match rx.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let to_copy = data.len().min(buf.remaining());
                buf.put_slice(&data[..to_copy]);
                if to_copy < data.len()
                    && let Ok(mut pending) = this.pending_buf.try_lock()
                {
                    pending.extend_from_slice(&data[to_copy..]);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for MockSerial {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.disconnected.load(Ordering::SeqCst) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "simulated disconnect",
            )));
        }

        let this = self.get_mut();
        if let Ok(mut written) = this.written.try_lock() {
            written.extend_from_slice(buf);
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn test_feed_and_read() {
        let (mut mock, ctrl) = mock_serial(100);
        ctrl.feed_data(b"hello world").await;

        let mut buf = [0u8; 64];
        let n = mock.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello world");
    }

    #[tokio::test]
    async fn test_feed_lines() {
        let (mut mock, ctrl) = mock_serial(100);
        ctrl.feed_lines(&["line1", "line2"]).await;

        let mut buf = [0u8; 64];
        let n = mock.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"line1\n");

        let n = mock.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"line2\n");
    }

    #[tokio::test]
    async fn test_write_capture() {
        let (mut mock, ctrl) = mock_serial(100);
        mock.write_all(b"sent to device").await.unwrap();

        let written = ctrl.drain_written().await;
        assert_eq!(written, b"sent to device");
    }

    #[tokio::test]
    async fn test_disconnect_read() {
        let (mut mock, ctrl) = mock_serial(100);
        ctrl.simulate_disconnect();

        let mut buf = [0u8; 64];
        let err = mock.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn test_disconnect_write() {
        let (mut mock, ctrl) = mock_serial(100);
        ctrl.simulate_disconnect();

        let err = mock.write_all(b"data").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn test_reconnect() {
        let (mut mock, ctrl) = mock_serial(100);
        ctrl.simulate_disconnect();

        let mut buf = [0u8; 64];
        assert!(mock.read(&mut buf).await.is_err());

        ctrl.simulate_reconnect();
        ctrl.feed_data(b"back online").await;

        let n = mock.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"back online");
    }
}
