// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Modem protocol behaviour over an in-memory duplex link.
//!
//! The unit tests inside the modules cover the happy path of each protocol.
//! These exercise the dispatch layer, payload shapes that stress framing, and
//! the cancellation path.

use std::time::Duration;

use devserial::modem::{self, FileTransferProtocol};

/// Round-trip a payload through the shared dispatch functions.
async fn round_trip(
    protocol: FileTransferProtocol,
    name: &str,
    payload: Vec<u8>,
) -> (usize, String, Vec<u8>) {
    let (mut sender, mut receiver) = tokio::io::duplex(1 << 16);
    let name_owned = name.to_string();
    let payload_for_sender = payload.clone();

    let sending = tokio::spawn(async move {
        modem::send(&mut sender, protocol, &name_owned, &payload_for_sender).await
    });
    let receiving = tokio::spawn(async move { modem::receive(&mut receiver, protocol).await });

    let (send_outcome, receive_outcome) = tokio::join!(sending, receiving);
    let byte_count = send_outcome.expect("send task").expect("send result");
    let (file_name, data) = receive_outcome.expect("recv task").expect("recv result");
    (byte_count, file_name, data)
}

#[tokio::test]
async fn every_protocol_round_trips_a_text_payload() {
    let payload = b"devserial round trip payload 0123456789".to_vec();

    for protocol in [
        FileTransferProtocol::Xmodem,
        FileTransferProtocol::XmodemCrc,
        FileTransferProtocol::Xmodem1k,
        FileTransferProtocol::Ymodem,
        FileTransferProtocol::Zmodem,
    ] {
        let (sent, name, data) = round_trip(protocol, "payload.bin", payload.clone()).await;
        assert_eq!(sent, payload.len(), "{protocol:?} reported wrong length");
        assert!(!name.is_empty(), "{protocol:?} returned no file name");

        // The XMODEM family pads the last block, so the payload is a prefix.
        assert!(
            data.starts_with(&payload),
            "{protocol:?} did not deliver the payload"
        );
    }
}

#[tokio::test]
async fn protocols_carrying_metadata_preserve_the_file_name() {
    for protocol in [FileTransferProtocol::Ymodem, FileTransferProtocol::Zmodem] {
        let (_, name, _) = round_trip(protocol, "firmware_v2.bin", b"data".to_vec()).await;
        assert_eq!(name, "firmware_v2.bin", "{protocol:?} lost the file name");
    }
}

#[tokio::test]
async fn xmodem_family_uses_a_protocol_specific_fallback_name() {
    let (_, name, _) = round_trip(FileTransferProtocol::Xmodem, "ignored", b"x".to_vec()).await;
    assert_eq!(name, "xmodem_recv.bin");

    let (_, name, _) = round_trip(FileTransferProtocol::Xmodem1k, "ignored", b"x".to_vec()).await;
    assert_eq!(name, "xmodem1k_recv.bin");
}

#[tokio::test]
async fn zmodem_survives_control_bytes_that_need_escaping() {
    // ZDLE, XON, XOFF, CAN and a high-bit run all have to come back unchanged.
    let payload: Vec<u8> = vec![
        0x18, 0x11, 0x13, 0x91, 0x93, 0x0D, 0x8D, 0x7F, 0xFF, 0x00, 0x18, 0x18,
    ]
    .into_iter()
    .cycle()
    .take(4096)
    .collect();

    let (sent, _, data) =
        round_trip(FileTransferProtocol::Zmodem, "escaped.bin", payload.clone()).await;
    assert_eq!(sent, payload.len());
    assert_eq!(data, payload, "ZMODEM escaping is not transparent");
}

#[tokio::test]
async fn ymodem_transfers_a_payload_spanning_several_blocks() {
    let payload: Vec<u8> = (0..5000u32)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    let (sent, name, data) =
        round_trip(FileTransferProtocol::Ymodem, "multi.bin", payload.clone()).await;
    assert_eq!(sent, payload.len());
    assert_eq!(name, "multi.bin");
    // YMODEM knows the exact size, so no padding may leak through.
    assert_eq!(data, payload);
}

#[tokio::test]
async fn xmodem_strips_the_padding_of_the_final_block() {
    // One byte past a block boundary forces a padded final block. The receiver
    // trims the CP/M padding, so a payload that does not itself end in 0x1A
    // comes back byte for byte.
    let payload: Vec<u8> = (0..129u32)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    assert_ne!(
        payload.last(),
        Some(&0x1A),
        "test payload must not end in PAD"
    );

    let (sent, _, data) = round_trip(
        FileTransferProtocol::XmodemCrc,
        "padded.bin",
        payload.clone(),
    )
    .await;
    assert_eq!(sent, payload.len());
    assert_eq!(data, payload);
}

#[tokio::test]
async fn a_cancelling_receiver_aborts_the_sender() {
    use tokio::io::AsyncWriteExt;

    let (mut sender, mut receiver) = tokio::io::duplex(256);
    let send = tokio::spawn(async move {
        modem::send(
            &mut sender,
            FileTransferProtocol::Xmodem,
            "cancelled.bin",
            b"payload",
        )
        .await
    });

    // CAN instead of the usual start character.
    receiver.write_all(&[0x18]).await.unwrap();
    receiver.flush().await.unwrap();

    let result = tokio::time::timeout(Duration::from_secs(10), send)
        .await
        .expect("a cancelled transfer must not hang")
        .expect("send task");

    let error = result.expect_err("a cancelled transfer must fail");
    assert!(
        error.to_lowercase().contains("cancel"),
        "unexpected error: {error}"
    );
}

#[test]
fn protocol_names_parse_from_the_cli_and_json_forms() {
    use clap::ValueEnum;

    assert_eq!(
        FileTransferProtocol::from_str("xmodem-1k", true).unwrap(),
        FileTransferProtocol::Xmodem1k
    );
    assert_eq!(
        FileTransferProtocol::from_str("xmodem1k", true).unwrap(),
        FileTransferProtocol::Xmodem1k
    );
    assert!(FileTransferProtocol::from_str("kermit", true).is_err());

    let parsed: FileTransferProtocol = serde_json::from_str("\"xmodem-crc\"").unwrap();
    assert_eq!(parsed, FileTransferProtocol::XmodemCrc);
    assert!(serde_json::from_str::<FileTransferProtocol>("\"kermit\"").is_err());
}

#[test]
fn checksums_match_their_reference_values() {
    assert_eq!(modem::crc16_ccitt(b"123456789"), 0x31C3);
    assert_eq!(modem::crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(modem::checksum8(&[255, 1]), 0);
}
