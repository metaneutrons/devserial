// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Typed serial line parameters.
//!
//! These types are the single source of truth for parity, flow control and
//! framing. Parsing rejects unknown values instead of silently falling back to
//! a default, and every consumer (CLI, MCP schema, TOML config, GUI, TUI)
//! shares the same representation and the same textual form.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Default baud rate used when nothing else is specified.
pub const DEFAULT_BAUD: u32 = 115_200;

/// Baud rates offered as presets in the GUI and TUI.
pub const BAUD_PRESETS: [u32; 8] = [
    9600, 19200, 38400, 57600, 115_200, 230_400, 460_800, 921_600,
];

/// Default duration of an RS-232 BREAK pulse.
pub const DEFAULT_BREAK_MS: u64 = 250;

/// Parity bit configuration.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    clap::ValueEnum,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
#[schemars(description = "Parity: none, odd or even")]
pub enum Parity {
    /// No parity bit.
    #[default]
    None,
    /// Odd parity.
    Odd,
    /// Even parity.
    Even,
}

impl Parity {
    /// Single-letter form used in framing summaries such as `8N1`.
    #[must_use]
    pub const fn letter(self) -> char {
        match self {
            Self::None => 'N',
            Self::Odd => 'O',
            Self::Even => 'E',
        }
    }

    /// Human-readable label for user interfaces.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "None (N)",
            Self::Odd => "Odd (O)",
            Self::Even => "Even (E)",
        }
    }

    /// Corresponding `serial2` value.
    #[must_use]
    pub const fn to_serial2(self) -> serial2::Parity {
        match self {
            Self::None => serial2::Parity::None,
            Self::Odd => serial2::Parity::Odd,
            Self::Even => serial2::Parity::Even,
        }
    }

    /// All variants, in presentation order.
    #[must_use]
    pub const fn all() -> [Self; 3] {
        [Self::None, Self::Even, Self::Odd]
    }
}

impl fmt::Display for Parity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Odd => "odd",
            Self::Even => "even",
        })
    }
}

impl FromStr for Parity {
    type Err = ParamError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "n" => Ok(Self::None),
            "odd" | "o" => Ok(Self::Odd),
            "even" | "e" => Ok(Self::Even),
            other => Err(ParamError::Parity(other.to_string())),
        }
    }
}

/// Hardware or software flow control.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    clap::ValueEnum,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
#[schemars(description = "Flow control: none, software (XON/XOFF) or hardware (RTS/CTS)")]
pub enum FlowControl {
    /// No flow control.
    #[default]
    None,
    /// XON/XOFF.
    Software,
    /// RTS/CTS.
    Hardware,
}

impl FlowControl {
    /// Human-readable label for user interfaces.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Software => "XON/XOFF",
            Self::Hardware => "RTS/CTS",
        }
    }

    /// Suffix appended to a framing summary, empty when no flow control is set.
    #[must_use]
    pub const fn summary_suffix(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Software => " (XON/XOFF)",
            Self::Hardware => " (RTS/CTS)",
        }
    }

    /// Corresponding `serial2` value.
    #[must_use]
    pub const fn to_serial2(self) -> serial2::FlowControl {
        match self {
            Self::None => serial2::FlowControl::None,
            Self::Software => serial2::FlowControl::XonXoff,
            Self::Hardware => serial2::FlowControl::RtsCts,
        }
    }

    /// All variants, in presentation order.
    #[must_use]
    pub const fn all() -> [Self; 3] {
        [Self::None, Self::Software, Self::Hardware]
    }
}

impl fmt::Display for FlowControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Software => "software",
            Self::Hardware => "hardware",
        })
    }
}

impl FromStr for FlowControl {
    type Err = ParamError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "off" => Ok(Self::None),
            "software" | "xonxoff" | "xon/xoff" => Ok(Self::Software),
            "hardware" | "rtscts" | "rts/cts" => Ok(Self::Hardware),
            other => Err(ParamError::FlowControl(other.to_string())),
        }
    }
}

/// Number of data bits per character, validated to 5 through 8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct DataBits(u8);

impl DataBits {
    /// Five data bits.
    pub const FIVE: Self = Self(5);
    /// Six data bits.
    pub const SIX: Self = Self(6);
    /// Seven data bits.
    pub const SEVEN: Self = Self(7);
    /// Eight data bits.
    pub const EIGHT: Self = Self(8);

    /// All valid values, in presentation order.
    #[must_use]
    pub const fn all() -> [Self; 4] {
        [Self::EIGHT, Self::SEVEN, Self::SIX, Self::FIVE]
    }

    /// Numeric value.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Corresponding `serial2` value.
    #[must_use]
    pub const fn to_serial2(self) -> serial2::CharSize {
        match self.0 {
            5 => serial2::CharSize::Bits5,
            6 => serial2::CharSize::Bits6,
            7 => serial2::CharSize::Bits7,
            _ => serial2::CharSize::Bits8,
        }
    }
}

impl Default for DataBits {
    fn default() -> Self {
        Self::EIGHT
    }
}

impl TryFrom<u8> for DataBits {
    type Error = ParamError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            5..=8 => Ok(Self(value)),
            other => Err(ParamError::DataBits(other)),
        }
    }
}

impl From<DataBits> for u8 {
    fn from(value: DataBits) -> Self {
        value.0
    }
}

impl fmt::Display for DataBits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for DataBits {
    type Err = ParamError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.trim()
            .parse::<u8>()
            .map_err(|_| ParamError::DataBitsText(s.to_string()))
            .and_then(Self::try_from)
    }
}

/// Number of stop bits, validated to 1 or 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct StopBits(u8);

impl StopBits {
    /// One stop bit.
    pub const ONE: Self = Self(1);
    /// Two stop bits.
    pub const TWO: Self = Self(2);

    /// All valid values, in presentation order.
    #[must_use]
    pub const fn all() -> [Self; 2] {
        [Self::ONE, Self::TWO]
    }

    /// Numeric value.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Corresponding `serial2` value.
    #[must_use]
    pub const fn to_serial2(self) -> serial2::StopBits {
        match self.0 {
            2 => serial2::StopBits::Two,
            _ => serial2::StopBits::One,
        }
    }
}

impl Default for StopBits {
    fn default() -> Self {
        Self::ONE
    }
}

impl TryFrom<u8> for StopBits {
    type Error = ParamError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 | 2 => Ok(Self(value)),
            other => Err(ParamError::StopBits(other)),
        }
    }
}

impl From<StopBits> for u8 {
    fn from(value: StopBits) -> Self {
        value.0
    }
}

impl fmt::Display for StopBits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for StopBits {
    type Err = ParamError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.trim()
            .parse::<u8>()
            .map_err(|_| ParamError::StopBitsText(s.to_string()))
            .and_then(Self::try_from)
    }
}

/// A rejected serial parameter value.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ParamError {
    #[error("invalid parity '{0}', expected one of: none, odd, even")]
    Parity(String),
    #[error("invalid flow control '{0}', expected one of: none, software, hardware")]
    FlowControl(String),
    #[error("invalid data bits {0}, expected 5, 6, 7 or 8")]
    DataBits(u8),
    #[error("invalid data bits '{0}', expected 5, 6, 7 or 8")]
    DataBitsText(String),
    #[error("invalid stop bits {0}, expected 1 or 2")]
    StopBits(u8),
    #[error("invalid stop bits '{0}', expected 1 or 2")]
    StopBitsText(String),
    #[error("invalid baud rate {0}, expected a value greater than zero")]
    Baud(u32),
}

/// Validate a baud rate.
///
/// # Errors
/// Returns an error if the rate is zero.
pub const fn validate_baud(baud: u32) -> Result<u32, ParamError> {
    if baud == 0 {
        Err(ParamError::Baud(baud))
    } else {
        Ok(baud)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parity_roundtrip() {
        for p in Parity::all() {
            assert_eq!(p.to_string().parse::<Parity>().unwrap(), p);
        }
    }

    #[test]
    fn parity_rejects_unknown() {
        let err = "bogus".parse::<Parity>().unwrap_err();
        assert!(err.to_string().contains("invalid parity 'bogus'"));
    }

    #[test]
    fn parity_accepts_shorthand_and_case() {
        assert_eq!("EVEN".parse::<Parity>().unwrap(), Parity::Even);
        assert_eq!("o".parse::<Parity>().unwrap(), Parity::Odd);
    }

    #[test]
    fn flow_control_roundtrip() {
        for f in FlowControl::all() {
            assert_eq!(f.to_string().parse::<FlowControl>().unwrap(), f);
        }
    }

    #[test]
    fn flow_control_rejects_unknown() {
        assert!("bogus".parse::<FlowControl>().is_err());
    }

    #[test]
    fn data_bits_range() {
        assert_eq!(DataBits::try_from(8).unwrap(), DataBits::EIGHT);
        assert!(DataBits::try_from(4).is_err());
        assert!(DataBits::try_from(9).is_err());
    }

    #[test]
    fn stop_bits_range() {
        assert_eq!(StopBits::try_from(2).unwrap(), StopBits::TWO);
        assert!(StopBits::try_from(0).is_err());
        assert!(StopBits::try_from(3).is_err());
    }

    #[test]
    fn serde_uses_plain_scalars() {
        let json = serde_json::to_string(&DataBits::SEVEN).unwrap();
        assert_eq!(json, "7");
        let back: DataBits = serde_json::from_str("7").unwrap();
        assert_eq!(back, DataBits::SEVEN);

        let json = serde_json::to_string(&Parity::Even).unwrap();
        assert_eq!(json, "\"even\"");
    }

    #[test]
    fn serde_rejects_invalid_scalars() {
        assert!(serde_json::from_str::<DataBits>("3").is_err());
        assert!(serde_json::from_str::<StopBits>("7").is_err());
        assert!(serde_json::from_str::<Parity>("\"sideways\"").is_err());
    }

    #[test]
    fn baud_validation() {
        assert!(validate_baud(0).is_err());
        assert_eq!(validate_baud(9600).unwrap(), 9600);
    }
}
