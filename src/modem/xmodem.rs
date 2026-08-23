// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! XMODEM protocol implementation (Standard, CRC, 1K).

use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{checksum8, crc16_ccitt};

const SOH: u8 = 0x01; // 128-byte block header
const STX: u8 = 0x02; // 1024-byte block header (1K)
const EOT: u8 = 0x04; // End of transmission
const ACK: u8 = 0x06; // Acknowledge
const NAK: u8 = 0x15; // Negative acknowledge
const CAN: u8 = 0x18; // Cancel
const CRC_C: u8 = 0x43; // 'C' character for CRC handshake
const PAD: u8 = 0x1A; // Standard CPM/XMODEM EOF padding

const MAX_RETRIES: u32 = 10;
const TIMEOUT: Duration = Duration::from_secs(3);

/// Send data using XMODEM.
///
/// # Errors
/// Returns error on transmission failure, timeout, or cancellation.
pub async fn send<S>(
    stream: &mut S,
    data: &[u8],
    use_1k: bool,
    mut on_progress: impl FnMut(usize, usize),
) -> Result<usize, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Wait for receiver start signal ('C' for CRC or NAK for checksum)
    let use_crc = wait_for_start(stream).await?;

    let block_size = if use_1k { 1024 } else { 128 };
    let header_byte = if use_1k { STX } else { SOH };

    let total_bytes = data.len();
    let mut offset = 0;
    let mut block_num: u8 = 1;

    while offset < total_bytes || offset == 0 {
        let chunk_end = (offset + block_size).min(total_bytes);
        let mut block_data = data[offset..chunk_end].to_vec();
        // Pad block if needed
        while block_data.len() < block_size {
            block_data.push(PAD);
        }

        // Build packet
        let mut packet = Vec::with_capacity(block_size + 5);
        packet.push(header_byte);
        packet.push(block_num);
        packet.push(!block_num);
        packet.extend_from_slice(&block_data);

        if use_crc {
            let crc = crc16_ccitt(&block_data);
            packet.push((crc >> 8) as u8);
            packet.push((crc & 0xFF) as u8);
        } else {
            packet.push(checksum8(&block_data));
        }

        // Send block with retries
        let mut retries = 0;
        loop {
            stream
                .write_all(&packet)
                .await
                .map_err(|e| format!("XMODEM write error: {e}"))?;
            stream
                .flush()
                .await
                .map_err(|e| format!("XMODEM flush error: {e}"))?;

            let response = read_byte_with_timeout(stream, TIMEOUT).await?;
            if response == ACK {
                break;
            } else if response == CAN {
                return Err("XMODEM transfer cancelled by receiver".to_string());
            }

            retries += 1;
            if retries >= MAX_RETRIES {
                return Err(format!(
                    "XMODEM block {block_num} failed after {MAX_RETRIES} retries"
                ));
            }
        }

        offset = chunk_end;
        on_progress(offset, total_bytes);
        block_num = block_num.wrapping_add(1);

        if offset >= total_bytes {
            break;
        }
    }

    // Send EOT
    let mut retries = 0;
    loop {
        stream
            .write_all(&[EOT])
            .await
            .map_err(|e| format!("XMODEM write EOT error: {e}"))?;
        stream
            .flush()
            .await
            .map_err(|e| format!("XMODEM flush EOT error: {e}"))?;

        let response = read_byte_with_timeout(stream, TIMEOUT).await?;
        if response == ACK {
            break;
        }
        retries += 1;
        if retries >= MAX_RETRIES {
            return Err("XMODEM EOT not acknowledged".to_string());
        }
    }

    Ok(total_bytes)
}

/// Receive data using XMODEM.
///
/// # Errors
/// Returns error on transmission failure, timeout, or cancellation.
pub async fn receive<S>(
    stream: &mut S,
    use_crc: bool,
    mut on_progress: impl FnMut(usize),
) -> Result<Vec<u8>, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut received = Vec::new();
    let mut expected_block: u8 = 1;
    let start_char = if use_crc { CRC_C } else { NAK };

    // Send start character up to 10 times
    let mut started = false;
    for _ in 0..MAX_RETRIES {
        stream
            .write_all(&[start_char])
            .await
            .map_err(|e| format!("XMODEM write error: {e}"))?;
        stream.flush().await.ok();

        if let Ok(b) = read_byte_with_timeout(stream, Duration::from_secs(1)).await {
            if b == SOH || b == STX || b == EOT || b == CAN {
                started = true;
                // Process this first byte
                if b == EOT {
                    stream.write_all(&[ACK]).await.ok();
                    stream.flush().await.ok();
                    return Ok(received);
                }
                if b == CAN {
                    return Err("XMODEM cancelled by sender".to_string());
                }
                // Handle SOH / STX block
                handle_incoming_block(stream, b, expected_block, use_crc, &mut received).await?;
                expected_block = expected_block.wrapping_add(1);
                on_progress(received.len());
                break;
            }
        }
    }

    if !started {
        return Err("XMODEM receive timeout waiting for sender".to_string());
    }

    // Main receive loop
    loop {
        let b = read_byte_with_timeout(stream, TIMEOUT).await?;
        if b == EOT {
            stream.write_all(&[ACK]).await.ok();
            stream.flush().await.ok();
            break;
        }
        if b == CAN {
            return Err("XMODEM cancelled by sender".to_string());
        }
        if b == SOH || b == STX {
            match handle_incoming_block(stream, b, expected_block, use_crc, &mut received).await {
                Ok(true) => {
                    expected_block = expected_block.wrapping_add(1);
                    on_progress(received.len());
                }
                Ok(false) => {
                    // Duplicate block re-acknowledged
                }
                Err(e) => {
                    tracing::warn!("XMODEM block error: {e}");
                }
            }
        }
    }

    // Trim trailing CPM PAD (0x1A) bytes
    while received.last() == Some(&PAD) {
        received.pop();
    }

    Ok(received)
}

async fn handle_incoming_block<S>(
    stream: &mut S,
    header: u8,
    expected_block: u8,
    use_crc: bool,
    output: &mut Vec<u8>,
) -> Result<bool, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let block_size = if header == STX { 1024 } else { 128 };
    let check_size = if use_crc { 2 } else { 1 };

    let mut buf = vec![0u8; 2 + block_size + check_size];
    stream
        .read_exact(&mut buf)
        .await
        .map_err(|e| format!("XMODEM read block error: {e}"))?;

    let blk = buf[0];
    let inv_blk = buf[1];
    let data = &buf[2..2 + block_size];
    let checksum = &buf[2 + block_size..];

    if inv_blk != !blk {
        stream.write_all(&[NAK]).await.ok();
        stream.flush().await.ok();
        return Err("XMODEM block number complement mismatch".to_string());
    }

    // Verify CRC or checksum
    let is_valid = if use_crc {
        let expected_crc = ((u16::from(checksum[0])) << 8) | u16::from(checksum[1]);
        crc16_ccitt(data) == expected_crc
    } else {
        checksum8(data) == checksum[0]
    };

    if !is_valid {
        stream.write_all(&[NAK]).await.ok();
        stream.flush().await.ok();
        return Err("XMODEM checksum/CRC validation failed".to_string());
    }

    if blk == expected_block {
        output.extend_from_slice(data);
        stream.write_all(&[ACK]).await.ok();
        stream.flush().await.ok();
        Ok(true)
    } else if blk == expected_block.wrapping_sub(1) {
        // Duplicate block (previous ACK was lost) -> re-send ACK without appending
        stream.write_all(&[ACK]).await.ok();
        stream.flush().await.ok();
        Ok(false)
    } else {
        stream.write_all(&[CAN]).await.ok();
        stream.flush().await.ok();
        Err(format!(
            "XMODEM block synchronization lost (expected {expected_block}, got {blk})"
        ))
    }
}

async fn wait_for_start<S>(stream: &mut S) -> Result<bool, String>
where
    S: AsyncRead + Unpin,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        match read_byte_with_timeout(stream, Duration::from_millis(500)).await {
            Ok(CRC_C) => return Ok(true),
            Ok(NAK) => return Ok(false),
            Ok(CAN) => return Err("XMODEM cancelled by receiver".to_string()),
            _ => {}
        }
    }
    Err("XMODEM timeout waiting for receiver start signal ('C' or NAK)".to_string())
}

async fn read_byte_with_timeout<S>(stream: &mut S, timeout: Duration) -> Result<u8, String>
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_xmodem_crc_transfer_roundtrip() {
        let (mut sender_io, mut receiver_io) = tokio::io::duplex(4096);
        let test_data = b"Hello from devserial pure Rust XMODEM CRC file transfer engine!";

        let send_handle =
            tokio::spawn(async move { send(&mut sender_io, test_data, false, |_, _| {}).await });

        let recv_handle =
            tokio::spawn(async move { receive(&mut receiver_io, true, |_| {}).await });

        let (send_res, recv_res) = tokio::join!(send_handle, recv_handle);
        let sent_bytes = send_res.unwrap().unwrap();
        let received_data = recv_res.unwrap().unwrap();

        assert_eq!(sent_bytes, test_data.len());
        assert_eq!(received_data, test_data);
    }

    #[tokio::test]
    async fn test_xmodem_1k_transfer_roundtrip() {
        let (mut sender_io, mut receiver_io) = tokio::io::duplex(8192);
        // Create 2500 bytes of data (multiple 1K blocks)
        let test_data: Vec<u8> = (0..2500u16)
            .map(|i| u8::try_from(i % 256).unwrap())
            .collect();
        let test_data_clone = test_data.clone();

        let send_handle =
            tokio::spawn(async move { send(&mut sender_io, &test_data, true, |_, _| {}).await });

        let recv_handle =
            tokio::spawn(async move { receive(&mut receiver_io, true, |_| {}).await });

        let (send_res, recv_res) = tokio::join!(send_handle, recv_handle);
        let sent_bytes = send_res.unwrap().unwrap();
        let received_data = recv_res.unwrap().unwrap();

        assert_eq!(sent_bytes, test_data_clone.len());
        assert_eq!(received_data, test_data_clone);
    }
}
