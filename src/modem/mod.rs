// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Modem file transfer protocols (XMODEM, YMODEM, ZMODEM).
//!
//! Pure-Rust implementation with zero unsafe code.

pub mod xmodem;
pub mod ymodem;
pub mod zmodem;

use serde::{Deserialize, Serialize};

/// Supported file transfer protocols.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum FileTransferProtocol {
    /// Standard XMODEM (128-byte blocks, 8-bit checksum)
    Xmodem,
    /// XMODEM with 16-bit CRC (128-byte blocks)
    XmodemCrc,
    /// XMODEM-1K (1024-byte blocks, 16-bit CRC)
    Xmodem1k,
    /// YMODEM-Batch (Block 0 file metadata + 1024-byte blocks + 16-bit CRC)
    Ymodem,
    /// ZMODEM (Streaming with sliding window, 32-bit CRC, ZDLE escaping)
    Zmodem,
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
