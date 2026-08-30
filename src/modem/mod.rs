// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Modem file transfer protocols (XMODEM, YMODEM, ZMODEM).
//!
//! Pure-Rust implementation with zero unsafe code.

pub mod xmodem;
pub mod ymodem;
pub mod zmodem;

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

/// Start of a 128-byte block.
pub(crate) const SOH: u8 = 0x01;
/// Start of a 1024-byte block.
pub(crate) const STX: u8 = 0x02;
/// End of transmission.
pub(crate) const EOT: u8 = 0x04;
/// Acknowledge.
pub(crate) const ACK: u8 = 0x06;
/// Negative acknowledge.
pub(crate) const NAK: u8 = 0x15;
/// Cancel.
pub(crate) const CAN: u8 = 0x18;
/// Receiver request for CRC mode.
pub(crate) const CRC_C: u8 = 0x43;
/// Block padding byte.
pub(crate) const PAD: u8 = 0x1A;

/// Retries before a block is considered lost.
pub(crate) const MAX_RETRIES: u32 = 10;
/// Per-byte timeout during a transfer.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(3);

/// Read a single byte with a timeout.
///
/// Shared by all three protocols so their timeout behaviour cannot drift.
pub(crate) async fn read_byte_with_timeout<S>(
    stream: &mut S,
    timeout: Duration,
) -> Result<u8, String>
where
    S: AsyncRead + Unpin,
{
    let mut buf = [0u8; 1];
    match tokio::time::timeout(timeout, stream.read_exact(&mut buf)).await {
        Ok(Ok(_)) => Ok(buf[0]),
        Ok(Err(e)) => Err(format!("I/O error reading byte: {e}")),
        Err(_) => Err("Timeout reading byte".to_string()),
    }
}

/// Supported file transfer protocols.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum FileTransferProtocol {
    /// Standard XMODEM (128-byte blocks, 8-bit checksum)
    Xmodem,
    /// XMODEM with 16-bit CRC (128-byte blocks)
    #[serde(alias = "xmodem_crc")]
    XmodemCrc,
    /// XMODEM-1K (1024-byte blocks, 16-bit CRC)
    #[value(alias = "xmodem-1k")]
    #[serde(alias = "xmodem-1k")]
    Xmodem1k,
    /// YMODEM-Batch (Block 0 file metadata + 1024-byte blocks + 16-bit CRC)
    Ymodem,
    /// ZMODEM (Streaming with sliding window, 32-bit CRC, ZDLE escaping)
    Zmodem,
}

impl FileTransferProtocol {
    /// Default file name used when a protocol carries no metadata.
    #[must_use]
    pub const fn fallback_filename(self) -> &'static str {
        match self {
            Self::Xmodem | Self::XmodemCrc => "xmodem_recv.bin",
            Self::Xmodem1k => "xmodem1k_recv.bin",
            Self::Ymodem => "ymodem_recv.bin",
            Self::Zmodem => "zmodem_recv.bin",
        }
    }

    /// Whether the protocol transmits 1024-byte blocks.
    #[must_use]
    pub const fn uses_1k_blocks(self) -> bool {
        matches!(self, Self::Xmodem1k)
    }

    /// Whether the receiver should request CRC framing.
    #[must_use]
    pub const fn uses_crc(self) -> bool {
        !matches!(self, Self::Xmodem)
    }
}

/// Send a file with the given protocol.
///
/// This dispatch exists once. Transports pass the protocol through rather than
/// repeating a match over all five variants.
///
/// # Errors
/// Returns the protocol error text on failure.
pub async fn send<S>(
    stream: &mut S,
    protocol: FileTransferProtocol,
    file_name: &str,
    data: &[u8],
) -> Result<usize, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match protocol {
        FileTransferProtocol::Xmodem
        | FileTransferProtocol::XmodemCrc
        | FileTransferProtocol::Xmodem1k => {
            xmodem::send(stream, data, protocol.uses_1k_blocks(), |_, _| {}).await
        }
        FileTransferProtocol::Ymodem => ymodem::send_file(stream, file_name, data, |_, _| {}).await,
        FileTransferProtocol::Zmodem => zmodem::send_file(stream, file_name, data, |_, _| {}).await,
    }
}

/// Receive a file with the given protocol.
///
/// Returns the file name reported by the protocol, or a protocol-specific
/// fallback for the XMODEM family, which transmits no metadata.
///
/// # Errors
/// Returns the protocol error text on failure.
pub async fn receive<S>(
    stream: &mut S,
    protocol: FileTransferProtocol,
) -> Result<(String, Vec<u8>), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match protocol {
        FileTransferProtocol::Xmodem
        | FileTransferProtocol::XmodemCrc
        | FileTransferProtocol::Xmodem1k => {
            let data = xmodem::receive(stream, protocol.uses_crc(), |_| {}).await?;
            Ok((protocol.fallback_filename().to_string(), data))
        }
        FileTransferProtocol::Ymodem => ymodem::receive_file(stream, |_, _| {}).await,
        FileTransferProtocol::Zmodem => zmodem::receive_file(stream, |_, _| {}).await,
    }
}

/// Progress report during file transfer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransferProgress {
    pub bytes_transferred: u64,
    pub total_bytes: Option<u64>,
    pub block_number: u32,
    pub retries: u32,
    pub message: String,
}

/// Calculate 16-bit CCITT CRC (polynomial 0x1021, initial value 0x0000).
#[must_use]
pub fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// Calculate 8-bit additive checksum (sum modulo 256).
#[must_use]
pub fn checksum8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
}

/// Calculate 32-bit CRC (polynomial 0xEDB88320, standard IEEE 802.3).
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crc16_ccitt() {
        assert_eq!(crc16_ccitt(b"123456789"), 0x31C3);
        assert_eq!(crc16_ccitt(b""), 0x0000);
    }

    #[test]
    fn test_checksum8() {
        assert_eq!(checksum8(&[1, 2, 3, 4]), 10);
        assert_eq!(checksum8(&[255, 1]), 0);
    }

    #[test]
    fn test_crc32() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
