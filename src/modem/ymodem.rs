// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! YMODEM protocol implementation (YMODEM-Batch with metadata).

use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{
    ACK, CAN, CRC_C, EOT, MAX_RETRIES, NAK, PAD, SOH, STX, TIMEOUT, crc16_ccitt,
    read_byte_with_timeout,
};

/// Send a file with filename and payload using YMODEM.
///
/// # Errors
/// Returns error on transmission failure, timeout, or cancellation.
pub async fn send_file<S>(
    stream: &mut S,
    filename: &str,
    data: &[u8],
    mut on_progress: impl FnMut(usize, usize),
) -> Result<usize, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // 1. Wait for initial 'C'
    wait_for_char(stream, CRC_C, Duration::from_secs(30)).await?;

    // 2. Send Block 0 (Header with filename and filesize)
    send_header_block(stream, filename, data.len()).await?;

    // Receiver should ACK then send 'C'
    let ack = read_byte_with_timeout(stream, TIMEOUT).await?;
    if ack != ACK {
        return Err(format!("YMODEM header ACK missing (got {ack:#04x})"));
    }
    let c = read_byte_with_timeout(stream, TIMEOUT).await?;
    if c != CRC_C {
        return Err(format!("YMODEM data start 'C' missing (got {c:#04x})"));
    }

    // 3. Send file data blocks (1024-byte STX blocks)
    let total_bytes = data.len();
    let mut offset = 0;
    let mut block_num: u8 = 1;

    while offset < total_bytes || offset == 0 {
        let chunk_end = (offset + 1024).min(total_bytes);
        let mut block_data = data[offset..chunk_end].to_vec();
        while block_data.len() < 1024 {
            block_data.push(PAD);
        }

        let mut packet = Vec::with_capacity(1029);
        packet.push(STX);
        packet.push(block_num);
        packet.push(!block_num);
        packet.extend_from_slice(&block_data);

        let crc = crc16_ccitt(&block_data);
        packet.push((crc >> 8) as u8);
        packet.push((crc & 0xFF) as u8);

        let mut retries = 0;
        loop {
            stream
                .write_all(&packet)
                .await
                .map_err(|e| format!("YMODEM write error: {e}"))?;
            stream
                .flush()
                .await
                .map_err(|e| format!("YMODEM flush error: {e}"))?;

            let response = read_byte_with_timeout(stream, TIMEOUT).await?;
            if response == ACK {
                break;
            } else if response == CAN {
                return Err("YMODEM transfer cancelled by receiver".to_string());
            }

            retries += 1;
            if retries >= MAX_RETRIES {
                return Err(format!(
                    "YMODEM block {block_num} failed after {MAX_RETRIES} retries"
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

    // 4. Send EOT -> NAK -> EOT -> ACK -> 'C' sequence (standard YMODEM)
    stream.write_all(&[EOT]).await.ok();
    stream.flush().await.ok();

    let resp1 = read_byte_with_timeout(stream, TIMEOUT).await?;
    if resp1 == NAK {
        stream.write_all(&[EOT]).await.ok();
        stream.flush().await.ok();
        let resp2 = read_byte_with_timeout(stream, TIMEOUT).await?;
        if resp2 != ACK {
            return Err("YMODEM EOT second ACK missing".to_string());
        }
    }

    // Receiver may send 'C' for next file
    if let Ok(c) = read_byte_with_timeout(stream, Duration::from_millis(500)).await
        && c == CRC_C
    {
        // 5. Send Null Block 0 to terminate batch
        send_null_block(stream).await?;
        let _ = read_byte_with_timeout(stream, TIMEOUT).await;
    }

    Ok(total_bytes)
}

/// Receive a file using YMODEM, returning (filename, data).
///
/// # Errors
/// Returns error on transmission failure, timeout, or cancellation.
pub async fn receive_file<S>(
    stream: &mut S,
    mut on_progress: impl FnMut(usize, usize),
) -> Result<(String, Vec<u8>), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // 1. Send 'C' until Block 0 arrives
    let mut filename = String::new();
    let mut expected_size: Option<usize> = None;

    let mut started = false;
    for _ in 0..MAX_RETRIES {
        stream.write_all(&[CRC_C]).await.ok();
        stream.flush().await.ok();

        if let Ok(b) = read_byte_with_timeout(stream, Duration::from_secs(1)).await
            && (b == SOH || b == STX)
        {
            // Parse Block 0
            let (name, size) = read_header_block(stream, b).await?;
            if name.is_empty() {
                // Empty header -> end of batch
                stream.write_all(&[ACK]).await.ok();
                stream.flush().await.ok();
                return Ok((String::new(), Vec::new()));
            }
            filename = name;
            expected_size = size;
            started = true;
            break;
        }
    }

    if !started {
        return Err("YMODEM timeout waiting for Block 0 header".to_string());
    }

    // Acknowledge Block 0 and request file data with 'C'
    stream.write_all(&[ACK, CRC_C]).await.ok();
    stream.flush().await.ok();

    // 2. Receive file data blocks
    let mut received = Vec::new();
    let mut expected_block: u8 = 1;

    loop {
        let b = read_byte_with_timeout(stream, TIMEOUT).await?;
        if b == EOT {
            // Send NAK for first EOT (YMODEM standard handshake)
            stream.write_all(&[NAK]).await.ok();
            stream.flush().await.ok();

            let next_eot = read_byte_with_timeout(stream, TIMEOUT).await?;
            if next_eot == EOT {
                stream.write_all(&[ACK, CRC_C]).await.ok();
                stream.flush().await.ok();
            }
            break;
        }

        if b == SOH || b == STX {
            let block_size = if b == STX { 1024 } else { 128 };
            let mut buf = vec![0u8; 2 + block_size + 2];
            stream
                .read_exact(&mut buf)
                .await
                .map_err(|e| format!("YMODEM read error: {e}"))?;

            let blk = buf[0];
            let inv_blk = buf[1];
            let data = &buf[2..2 + block_size];
            let crc_bytes = &buf[2 + block_size..];

            if inv_blk != !blk {
                stream.write_all(&[NAK]).await.ok();
                stream.flush().await.ok();
                continue;
            }

            let expected_crc = (u16::from(crc_bytes[0]) << 8) | u16::from(crc_bytes[1]);
            if crc16_ccitt(data) != expected_crc {
                stream.write_all(&[NAK]).await.ok();
                stream.flush().await.ok();
                continue;
            }

            if blk == expected_block {
                received.extend_from_slice(data);
                stream.write_all(&[ACK]).await.ok();
                stream.flush().await.ok();
                expected_block = expected_block.wrapping_add(1);
                on_progress(received.len(), expected_size.unwrap_or(0));
            } else {
                stream.write_all(&[ACK]).await.ok();
                stream.flush().await.ok();
            }
        }
    }

    // Truncate to exact file size if metadata provided
    if let Some(size) = expected_size {
        received.truncate(size);
    } else {
        while received.last() == Some(&PAD) {
            received.pop();
        }
    }

    // 3. Read termination block 0
    if let Ok(b) = read_byte_with_timeout(stream, Duration::from_millis(500)).await
        && (b == SOH || b == STX)
    {
        let _ = read_header_block(stream, b).await;
        stream.write_all(&[ACK]).await.ok();
        stream.flush().await.ok();
    }

    Ok((filename, received))
}

async fn send_header_block<S>(stream: &mut S, filename: &str, filesize: usize) -> Result<(), String>
where
    S: AsyncWrite + Unpin,
{
    let mut payload = vec![0u8; 128];
    let name_bytes = filename.as_bytes();
    let size_str = format!("{filesize}");
    let size_bytes = size_str.as_bytes();

    let mut cursor = 0;
    payload[cursor..cursor + name_bytes.len()].copy_from_slice(name_bytes);
    cursor += name_bytes.len();
    payload[cursor] = 0; // null separator
    cursor += 1;
    payload[cursor..cursor + size_bytes.len()].copy_from_slice(size_bytes);

    let mut packet = Vec::with_capacity(133);
    packet.push(SOH);
    packet.push(0x00);
    packet.push(0xFF);
    packet.extend_from_slice(&payload);

    let crc = crc16_ccitt(&payload);
    packet.push((crc >> 8) as u8);
    packet.push((crc & 0xFF) as u8);

    stream
        .write_all(&packet)
        .await
        .map_err(|e| format!("YMODEM header write error: {e}"))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("YMODEM header flush error: {e}"))?;
    Ok(())
}

async fn send_null_block<S>(stream: &mut S) -> Result<(), String>
where
    S: AsyncWrite + Unpin,
{
    let payload = [0u8; 128];
    let mut packet = Vec::with_capacity(133);
    packet.push(SOH);
    packet.push(0x00);
    packet.push(0xFF);
    packet.extend_from_slice(&payload);

    let crc = crc16_ccitt(&payload);
    packet.push((crc >> 8) as u8);
    packet.push((crc & 0xFF) as u8);

    stream
        .write_all(&packet)
        .await
        .map_err(|e| format!("YMODEM null block write error: {e}"))?;
    stream.flush().await.ok();
    Ok(())
}

async fn read_header_block<S>(stream: &mut S, header: u8) -> Result<(String, Option<usize>), String>
where
    S: AsyncRead + Unpin,
{
    let block_size = if header == STX { 1024 } else { 128 };
    let mut buf = vec![0u8; 2 + block_size + 2];
    stream
        .read_exact(&mut buf)
        .await
        .map_err(|e| format!("YMODEM header read error: {e}"))?;

    let payload = &buf[2..2 + block_size];

    // Find filename null terminator
    let Some(name_end) = payload.iter().position(|&b| b == 0) else {
        return Ok((String::new(), None));
    };

    let filename = String::from_utf8_lossy(&payload[..name_end]).to_string();
    if filename.is_empty() {
        return Ok((String::new(), None));
    }

    let rest = &payload[name_end + 1..];
    let size = rest
        .iter()
        .position(|&b| b == 0 || b == b' ')
        .and_then(|size_end| {
            let size_str = String::from_utf8_lossy(&rest[..size_end]);
            size_str.parse::<usize>().ok()
        });

    Ok((filename, size))
}

async fn wait_for_char<S>(stream: &mut S, expected: u8, timeout: Duration) -> Result<(), String>
where
    S: AsyncRead + Unpin,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(b) = read_byte_with_timeout(stream, Duration::from_millis(500)).await {
            if b == expected {
                return Ok(());
            }
            if b == CAN {
                return Err("YMODEM cancelled by receiver".to_string());
            }
        }
    }
    Err(format!("YMODEM timeout waiting for {expected:#04x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ymodem_batch_roundtrip() {
        let (mut sender_io, mut receiver_io) = tokio::io::duplex(8192);
        let filename = "firmware_v1.0.bin";
        let test_data = b"DevSerial embedded YMODEM-Batch payload testing 1234567890!";

        let send_handle =
            tokio::spawn(
                async move { send_file(&mut sender_io, filename, test_data, |_, _| {}).await },
            );

        let recv_handle =
            tokio::spawn(async move { receive_file(&mut receiver_io, |_, _| {}).await });

        let (send_res, recv_res) = tokio::join!(send_handle, recv_handle);
        let sent_bytes = send_res.unwrap().unwrap();
        let (recv_name, recv_data) = recv_res.unwrap().unwrap();

        assert_eq!(sent_bytes, test_data.len());
        assert_eq!(recv_name, filename);
        assert_eq!(recv_data, test_data);
    }
}
