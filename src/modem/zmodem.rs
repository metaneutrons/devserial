// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! ZMODEM protocol implementation (Streaming with 32-bit CRC and ZDLE escaping).

use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::crc32;

// ZMODEM Constants
const ZPAD: u8 = 0x2A; // '*'
const ZDLE: u8 = 0x18; // Ctrl-X / CAN
const ZBIN32: u8 = 0x43; // 'C' - Binary header with 32-bit CRC

// Frame types
const ZRQINIT: u8 = 0;
const ZRINIT: u8 = 1;
const ZFILE: u8 = 4;
const ZFIN: u8 = 8;
const ZRPOS: u8 = 9;
const ZDATA: u8 = 10;
const ZEOF: u8 = 11;

// Subpacket frame ends
const ZCRCE: u8 = 0x68; // End of frame, header follows
const ZCRCG: u8 = 0x69; // Streaming data subpacket (no wait)
const ZCRCW: u8 = 0x6B; // Wait for ACK

const TIMEOUT: Duration = Duration::from_secs(4);

/// Send a file with filename and payload using ZMODEM.
///
/// # Errors
/// Returns error on transmission failure, timeout, or cancellation.
#[allow(clippy::cast_possible_truncation)]
pub async fn send_file<S>(
    stream: &mut S,
    filename: &str,
    data: &[u8],
    mut on_progress: impl FnMut(usize, usize),
) -> Result<usize, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // 1. Send ZRQINIT to invite receiver
    send_header32(stream, ZRQINIT, &[0, 0, 0, 0]).await?;

    // 2. Wait for ZRINIT
    let (frame_type, _flags) = wait_for_header(stream, TIMEOUT).await?;
    if frame_type != ZRINIT {
        return Err(format!("expected ZRINIT, got frame type {frame_type}"));
    }

    // 3. Send ZFILE with filename and filesize metadata
    send_header32(stream, ZFILE, &[0, 0, 0, 0]).await?;
    let mut file_info = format!("{}\0{} 0\0", filename, data.len()).into_bytes();
    file_info.push(0);
    send_subpacket32(stream, &file_info, ZCRCW).await?;

    // 4. Wait for ZRPOS (receiver specifies starting offset, usually 0)
    let (mut frame_type, mut pos_flags) = wait_for_header(stream, TIMEOUT).await?;
    while frame_type == ZRINIT || frame_type == ZRQINIT {
        let (ft, pf) = wait_for_header(stream, TIMEOUT).await?;
        frame_type = ft;
        pos_flags = pf;
    }
    if frame_type != ZRPOS {
        return Err(format!("expected ZRPOS, got frame type {frame_type}"));
    }
    let start_offset = u32::from_le_bytes(pos_flags) as usize;

    // 5. Send ZDATA header with file offset
    let offset_bytes = (start_offset as u32).to_le_bytes();
    send_header32(stream, ZDATA, &offset_bytes).await?;

    // 6. Stream data subpackets (1024 bytes per subpacket)
    let total_bytes = data.len();
    let mut offset = start_offset;

    while offset < total_bytes {
        let chunk_end = (offset + 1024).min(total_bytes);
        let chunk = &data[offset..chunk_end];
        let is_last = chunk_end >= total_bytes;
        let frame_end = if is_last { ZCRCE } else { ZCRCG };

        send_subpacket32(stream, chunk, frame_end).await?;
        offset = chunk_end;
        on_progress(offset, total_bytes);
    }

    // 7. Send ZEOF
    let end_offset_bytes = (total_bytes as u32).to_le_bytes();
    send_header32(stream, ZEOF, &end_offset_bytes).await?;

    // 8. Wait for ZRINIT from receiver
    let (frame_type, _) = wait_for_header(stream, TIMEOUT).await?;
    if frame_type == ZRINIT {
        // Send ZFIN
        send_header32(stream, ZFIN, &[0, 0, 0, 0]).await?;
        // Wait for ZFIN echo
        let _ = wait_for_header(stream, TIMEOUT).await;
        // Send final 'OO' (Over and Out)
        stream.write_all(b"OO").await.ok();
        stream.flush().await.ok();
    }

    Ok(total_bytes)
}

/// Receive a file using ZMODEM, returning (filename, data).
///
/// # Errors
/// Returns error on transmission failure, timeout, or cancellation.
#[allow(clippy::cast_possible_truncation)]
pub async fn receive_file<S>(
    stream: &mut S,
    mut on_progress: impl FnMut(usize, usize),
) -> Result<(String, Vec<u8>), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // 1. Send ZRINIT to indicate readiness
    send_header32(stream, ZRINIT, &[0, 0, 0, 0x23]).await?;

    // 2. Wait for ZFILE or ZFIN
    let mut filename = String::new();
    let mut expected_size = 0usize;

    loop {
        let (frame_type, _) = wait_for_header(stream, TIMEOUT).await?;
        if frame_type == ZRQINIT {
            send_header32(stream, ZRINIT, &[0, 0, 0, 0x23]).await?;
            continue;
        }
        if frame_type == ZFIN {
            send_header32(stream, ZFIN, &[0, 0, 0, 0]).await?;
            return Ok((String::new(), Vec::new()));
        }
        if frame_type == ZFILE {
            let (file_info, _) = read_subpacket32(stream).await?;
            let info_str = String::from_utf8_lossy(&file_info);
            let parts: Vec<&str> = info_str.split('\0').collect();
            if !parts.is_empty() {
                filename = parts[0].to_string();
            }
            if parts.len() > 1 {
                let meta_parts: Vec<&str> = parts[1].split_whitespace().collect();
                if !meta_parts.is_empty() {
                    expected_size = meta_parts[0].parse::<usize>().unwrap_or(0);
                }
            }
            break;
        }
    }

    // 3. Send ZRPOS (offset 0)
    send_header32(stream, ZRPOS, &[0, 0, 0, 0]).await?;

    // 4. Wait for ZDATA
    let (frame_type, _) = wait_for_header(stream, TIMEOUT).await?;
    if frame_type != ZDATA {
        return Err(format!("expected ZDATA, got {frame_type}"));
    }

    // 5. Stream subpackets until ZEOF
    let mut received = Vec::with_capacity(expected_size);

    loop {
        let (data, frame_end) = read_subpacket32(stream).await?;
        received.extend_from_slice(&data);
        on_progress(received.len(), expected_size);

        if frame_end == ZCRCE || frame_end == ZCRCW {
            let (next_frame, _) = wait_for_header(stream, TIMEOUT).await?;
            if next_frame == ZEOF {
                break;
            }
        }
    }

    // 6. Send ZRINIT acknowledging file completion
    send_header32(stream, ZRINIT, &[0, 0, 0, 0x23]).await?;

    // 7. Handle ZFIN handshake
    if let Ok((ZFIN, _)) = wait_for_header(stream, Duration::from_secs(2)).await {
        send_header32(stream, ZFIN, &[0, 0, 0, 0]).await?;
        // Read final 'OO' if any
        let mut oo_buf = [0u8; 2];
        let _ = tokio::time::timeout(Duration::from_millis(200), stream.read(&mut oo_buf)).await;
    }

    Ok((filename, received))
}

async fn send_header32<S>(stream: &mut S, frame_type: u8, flags: &[u8; 4]) -> Result<(), String>
where
    S: AsyncWrite + Unpin,
{
    let mut raw = Vec::with_capacity(5);
    raw.push(frame_type);
    raw.extend_from_slice(flags);

    let crc = crc32(&raw);
    let crc_bytes = crc.to_le_bytes();

    let mut out = Vec::with_capacity(20);
    out.push(ZPAD);
    out.push(ZPAD);
    out.push(ZDLE);
    out.push(ZBIN32);

    for &b in &raw {
        escape_byte(b, &mut out);
    }
    for &b in &crc_bytes {
        escape_byte(b, &mut out);
    }

    stream
        .write_all(&out)
        .await
        .map_err(|e| format!("ZMODEM header write error: {e}"))?;
    stream.flush().await.ok();
    Ok(())
}

async fn send_subpacket32<S>(stream: &mut S, data: &[u8], frame_end: u8) -> Result<(), String>
where
    S: AsyncWrite + Unpin,
{
    let mut crc_payload = data.to_vec();
    crc_payload.push(frame_end);
    let crc = crc32(&crc_payload);
    let crc_bytes = crc.to_le_bytes();

    let mut out = Vec::with_capacity(data.len() * 2 + 10);
    for &b in data {
        escape_byte(b, &mut out);
    }
    out.push(ZDLE);
    out.push(frame_end);

    for &b in &crc_bytes {
        escape_byte(b, &mut out);
    }

    stream
        .write_all(&out)
        .await
        .map_err(|e| format!("ZMODEM subpacket write error: {e}"))?;
    stream.flush().await.ok();
    Ok(())
}

async fn read_subpacket32<S>(stream: &mut S) -> Result<(Vec<u8>, u8), String>
where
    S: AsyncRead + Unpin,
{
    let mut data = Vec::new();
    let frame_end;

    loop {
        let b = read_escaped_byte(stream).await?;
        if b.1 {
            // Byte was preceded by ZDLE
            match b.0 {
                ZCRCE | ZCRCG | ZCRCW => {
                    frame_end = b.0;
                    break;
                }
                other => data.push(other),
            }
        } else {
            data.push(b.0);
        }
    }

    // Read 4 CRC bytes
    let mut crc_bytes = [0u8; 4];
    for b in &mut crc_bytes {
        *b = read_escaped_byte(stream).await?.0;
    }

    let received_crc = u32::from_le_bytes(crc_bytes);
    let mut crc_payload = data.clone();
    crc_payload.push(frame_end);
    let calculated_crc = crc32(&crc_payload);

    if received_crc != calculated_crc {
        return Err("ZMODEM subpacket CRC-32 mismatch".to_string());
    }

    Ok((data, frame_end))
}

async fn wait_for_header<S>(stream: &mut S, timeout: Duration) -> Result<(u8, [u8; 4]), String>
where
    S: AsyncRead + Unpin,
{
    let deadline = tokio::time::Instant::now() + timeout;

    while tokio::time::Instant::now() < deadline {
        let mut buf = [0u8; 1];
        if tokio::time::timeout(Duration::from_millis(200), stream.read_exact(&mut buf))
            .await
            .is_err()
        {
            continue;
        }

        if buf[0] == ZDLE {
            let mut htype = [0u8; 1];
            stream
                .read_exact(&mut htype)
                .await
                .map_err(|e| e.to_string())?;

            if htype[0] == ZBIN32 {
                let mut raw = [0u8; 5];
                for b in &mut raw {
                    *b = read_escaped_byte(stream).await?.0;
                }

                let mut crc_bytes = [0u8; 4];
                for b in &mut crc_bytes {
                    *b = read_escaped_byte(stream).await?.0;
                }

                let expected_crc = u32::from_le_bytes(crc_bytes);
                if crc32(&raw) != expected_crc {
                    return Err("ZMODEM header CRC-32 mismatch".to_string());
                }

                let frame_type = raw[0];
                let mut flags = [0u8; 4];
                flags.copy_from_slice(&raw[1..5]);
                return Ok((frame_type, flags));
            }
        }
    }

    Err("ZMODEM header timeout".to_string())
}

fn escape_byte(b: u8, out: &mut Vec<u8>) {
    match b {
        0x10 | 0x11 | 0x13 | 0x18 | 0x7F | 0x90 | 0x91 | 0x93 => {
            out.push(ZDLE);
            out.push(b ^ 0x40);
        }
        _ => out.push(b),
    }
}

async fn read_escaped_byte<S>(stream: &mut S) -> Result<(u8, bool), String>
where
    S: AsyncRead + Unpin,
{
    let mut buf = [0u8; 1];
    stream
        .read_exact(&mut buf)
        .await
        .map_err(|e| format!("I/O error: {e}"))?;

    if buf[0] == ZDLE {
        stream
            .read_exact(&mut buf)
            .await
            .map_err(|e| format!("I/O error: {e}"))?;
        match buf[0] {
            ZCRCE | ZCRCG | ZCRCW => Ok((buf[0], true)),
            escaped => Ok((escaped ^ 0x40, false)),
        }
    } else {
        Ok((buf[0], false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_zmodem_streaming_roundtrip() {
        let (mut sender_io, mut receiver_io) = tokio::io::duplex(16384);
        let filename = "zmodem_test.bin";
        let test_data = b"DevSerial High Performance ZMODEM Streaming Protocol Testing 1234567890!";

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
