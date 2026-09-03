// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Hex encoding and decoding, used by every path that accepts `0x…` payloads.

/// A rejected hex string.
#[derive(Debug, Clone, thiserror::Error)]
pub enum HexError {
    #[error("hex string must have an even number of digits, got {0}")]
    OddLength(usize),
    #[error("invalid hex digit '{digit}' at position {position}")]
    InvalidDigit { digit: String, position: usize },
}

/// Decode a hex string into bytes.
///
/// Whitespace is ignored, an optional `0x` or `0X` prefix is stripped. Any
/// invalid digit is an error, never a silently dropped byte.
///
/// # Errors
/// Returns an error for an odd number of digits or an invalid digit.
pub fn decode(input: &str) -> Result<Vec<u8>, HexError> {
    let stripped = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
        .unwrap_or(input);
    let digits: Vec<char> = stripped.chars().filter(|c| !c.is_whitespace()).collect();

    if !digits.len().is_multiple_of(2) {
        return Err(HexError::OddLength(digits.len()));
    }

    digits
        .chunks(2)
        .enumerate()
        .map(|(index, pair)| {
            let text: String = pair.iter().collect();
            u8::from_str_radix(&text, 16).map_err(|_| HexError::InvalidDigit {
                digit: text,
                position: index * 2,
            })
        })
        .collect()
}

/// Whether a payload string is meant to be interpreted as hex.
#[must_use]
pub fn has_prefix(input: &str) -> bool {
    input.starts_with("0x") || input.starts_with("0X")
}

/// Encode bytes as uppercase hex without separators.
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02X}");
        out
    })
}

/// Encode bytes as a readable dump, grouped in blocks of 16 bytes.
#[must_use]
pub fn format_dump(bytes: &[u8]) -> String {
    bytes
        .chunks(16)
        .map(|chunk| {
            chunk
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("  ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_plain_and_prefixed() {
        assert_eq!(decode("414243").unwrap(), b"ABC");
        assert_eq!(decode("0x0D0A").unwrap(), vec![0x0D, 0x0A]);
        assert_eq!(decode("0X0d0a").unwrap(), vec![0x0D, 0x0A]);
    }

    #[test]
    fn ignores_all_whitespace() {
        assert_eq!(decode("41 42\t43\n").unwrap(), b"ABC");
    }

    #[test]
    fn rejects_odd_length() {
        let err = decode("ABC").unwrap_err();
        assert!(err.to_string().contains("even number"));
    }

    #[test]
    fn rejects_invalid_digit_instead_of_dropping_it() {
        let err = decode("41ZZ43").unwrap_err();
        assert!(err.to_string().contains("invalid hex digit 'ZZ'"));
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert!(decode("").unwrap().is_empty());
        assert!(decode("0x").unwrap().is_empty());
    }

    #[test]
    fn encode_roundtrip() {
        let bytes = [0x00, 0x7F, 0xFF, 0xAB];
        assert_eq!(encode(&bytes), "007FFFAB");
        assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
    }

    #[test]
    fn dump_groups_in_sixteens() {
        let bytes: Vec<u8> = (0..18).collect();
        let dump = format_dump(&bytes);
        assert!(dump.contains("00 01 02"));
        assert!(dump.contains("  10 11"));
    }

    #[test]
    fn non_ascii_is_rejected_without_panicking() {
        assert!(decode("41ü2").is_err());
    }
}
