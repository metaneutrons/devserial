// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! What a monitoring surface can do, and which surface can do it.
//!
//! devserial shows the same port through a window and through a terminal. The
//! two drifted: the window grew macros, control lines, export, a filter and a
//! firmware dialog while the terminal kept the set it started with. Nothing
//! noticed, because nothing was comparing them.
//!
//! This module is that comparison. Every capability is named once here, each
//! surface declares which ones it offers, and the test below fails when the
//! difference between them changes. Closing a gap and opening one both require
//! editing `KNOWN_GAPS`, so neither happens by accident.
//!
//! Written as plain text rather than a link: `KNOWN_GAPS` only exists when both
//! surfaces are compiled in, and a link to it fails the documentation build for
//! every other feature set.
//!
//! What the registry covers is what a person can do through a surface: with
//! the port it is showing, and with the daemon behind it. The second half was
//! added when the HTTP interface arrived, because its switch is offered by both
//! surfaces and leaving it out would have meant the register no longer
//! describes what they do.
//!
//! What it leaves out is how a surface is *drawn*. The two cannot meet there in
//! any case: the window scales itself, while the size of a terminal belongs to
//! the terminal and not to the program running inside it. Interface scale,
//! window size and full screen are therefore absent from the list rather than
//! recorded as gaps, which would claim the terminal still owes something it
//! cannot deliver.

/// One thing a person can do through a surface.
///
/// Named after the effect, not after the control that triggers it, so the same
/// entry covers a toolbar button and a key combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    /// Send a line of input to the device.
    SendLine,
    /// Recall previously sent lines.
    InputHistory,
    /// Choose the line ending that is appended when sending.
    LineEnding,
    /// Send an RS-232 BREAK.
    Break,
    /// Set DTR and RTS by hand.
    ControlLines,
    /// Run a named macro such as `reset`.
    Macros,
    /// Change baud rate, framing and flow control while the port is open.
    PortSettings,
    /// Release the port and take it back without closing the surface.
    ToggleConnection,
    /// Show what the reader says about the hardware.
    LinkState,
    /// Show or hide timestamps.
    Timestamps,
    /// Show the buffer as hex.
    HexView,
    /// Stop following the tail and scroll back.
    ScrollBack,
    /// Narrow the view to matching lines.
    Filter,
    /// Discard the buffer.
    ClearBuffer,
    /// Write the buffer out in a chosen format.
    ///
    /// The window additionally offers the clipboard. That is a convenience of
    /// a graphical surface rather than a capability of its own: a terminal has
    /// no portable clipboard, and claiming one would be a promise that breaks
    /// on the first machine without a helper installed.
    Export,
    /// Send and receive files with a modem protocol.
    FileTransfer,
    /// Choose which modem protocol a transfer uses.
    TransferProtocol,
    /// Write firmware to the device.
    FlashFirmware,
    /// Show version, licence and origin.
    About,
    /// Show and change the HTTP interface of the daemon.
    ///
    /// Not a thing done with the port, which is why the register's definition
    /// widened rather than this being recorded as a gap: both surfaces offer
    /// it, so leaving it out would describe them wrongly.
    #[cfg(feature = "rest")]
    RestServer,
}

impl Capability {
    /// Every capability, so a new one cannot be forgotten by either surface.
    pub const ALL: &'static [Self] = &[
        Self::SendLine,
        Self::InputHistory,
        Self::LineEnding,
        Self::Break,
        Self::ControlLines,
        Self::Macros,
        Self::PortSettings,
        Self::ToggleConnection,
        Self::LinkState,
        Self::Timestamps,
        Self::HexView,
        Self::ScrollBack,
        Self::Filter,
        Self::ClearBuffer,
        Self::Export,
        Self::FileTransfer,
        Self::TransferProtocol,
        Self::FlashFirmware,
        Self::About,
        #[cfg(feature = "rest")]
        Self::RestServer,
    ];
}

/// What the graphical window offers, and the text that proves it still does.
///
/// The second element is a token that has to occur in `monitor.rs`. It ties
/// the claim to the implementation: deleting the control without touching this
/// table fails the test rather than quietly widening the gap.
#[cfg(feature = "monitor")]
pub const GUI: &[(Capability, &str)] = &[
    (Capability::SendLine, "\"Send\""),
    (Capability::InputHistory, "history_idx"),
    (Capability::LineEnding, "\"CRLF\""),
    (Capability::Break, "\"Break\""),
    (Capability::ControlLines, "\"DTR\""),
    (Capability::Macros, "\"Bootloader\""),
    (Capability::PortSettings, "show_settings_dialog"),
    (Capability::ToggleConnection, "⏏ Disconnect"),
    (Capability::LinkState, "fn link_status"),
    (Capability::Timestamps, "\"Time\""),
    (Capability::HexView, "\"Hex\""),
    (Capability::ScrollBack, "\"Auto-follow\""),
    (Capability::Filter, "\"Filter:\""),
    (Capability::ClearBuffer, "\"Clear\""),
    (Capability::Export, "\"Export ▾\""),
    (Capability::FileTransfer, "\"Transfer ▾\""),
    (Capability::TransferProtocol, "FileTransferProtocol::ALL"),
    (Capability::FlashFirmware, "\"Flash ▾\""),
    (Capability::About, "show_about_dialog"),
    #[cfg(feature = "rest")]
    (Capability::RestServer, "fn render_rest_dialog"),
];

/// What the terminal interface offers, with the same kind of proof.
#[cfg(feature = "tui")]
pub const TUI: &[(Capability, &str)] = &[
    (Capability::SendLine, "handle_enter"),
    (Capability::Break, "send_break"),
    (Capability::PortSettings, "InputMode::Configure"),
    (Capability::LinkState, "fn link_label"),
    (Capability::Timestamps, "show_timestamps"),
    (Capability::HexView, "hex_view"),
    (Capability::ScrollBack, "auto_follow"),
    (Capability::FileTransfer, "InputMode::SendFile"),
    (Capability::About, "InputMode::About"),
    (Capability::ClearBuffer, "\"Display cleared\""),
    (Capability::Filter, "InputMode::Filter"),
    (Capability::ControlLines, "enum ControlLine"),
    (Capability::Macros, "fn handle_macros_key"),
    (Capability::InputHistory, "fn history_back"),
    (Capability::LineEnding, "state.line_ending.suffix()"),
    (Capability::ToggleConnection, "fn toggle_connection"),
    (Capability::TransferProtocol, "fn transfer_title"),
    (Capability::Export, "fn run_export"),
    (Capability::FlashFirmware, "fn start_flash"),
    #[cfg(feature = "rest")]
    (Capability::RestServer, "InputMode::Rest"),
];

/// Capabilities the terminal interface does not have yet, and why.
///
/// This list is the ledger. A capability may sit here, or it may be in both
/// surfaces; it may not simply be absent. Adding something to the window
/// therefore fails the test until it is either built for the terminal too or
/// written down here on purpose.
///
/// **The list is empty.** It held ten entries on 6 September 2026 and was
/// worked off the same day. Anything added here from now on is debt and should
/// carry a reason worth reading, because the alternative is building it for
/// both surfaces.
///
/// One limit of the mechanism, so nobody mistakes it for more than it is: the
/// anchors are searched in the source text and cannot see `cfg` attributes.
/// The ledger therefore describes a full build. `FlashFirmware` needs the
/// `esp` feature in both surfaces, and a build without it has the capability
/// in neither.
#[cfg(all(feature = "monitor", feature = "tui"))]
pub const KNOWN_GAPS: &[(Capability, &str)] = &[];

#[cfg(all(test, feature = "monitor", feature = "tui"))]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const MONITOR_SOURCE: &str = include_str!("monitor.rs");
    const TUI_SOURCE: &str = include_str!("tui.rs");

    fn named(entries: &[(Capability, &str)]) -> BTreeSet<Capability> {
        entries.iter().map(|(capability, _)| *capability).collect()
    }

    #[test]
    fn every_claim_is_backed_by_something_in_the_source() {
        for (capability, token) in GUI {
            assert!(
                MONITOR_SOURCE.contains(token),
                "the window claims {capability:?} but monitor.rs has no {token}"
            );
        }
        for (capability, token) in TUI {
            assert!(
                TUI_SOURCE.contains(token),
                "the terminal claims {capability:?} but tui.rs has no {token}"
            );
        }
    }

    #[test]
    fn no_capability_is_claimed_twice() {
        assert_eq!(named(GUI).len(), GUI.len(), "the window lists a duplicate");
        assert_eq!(
            named(TUI).len(),
            TUI.len(),
            "the terminal lists a duplicate"
        );
        assert_eq!(
            named(KNOWN_GAPS).len(),
            KNOWN_GAPS.len(),
            "the ledger lists a duplicate"
        );
    }

    #[test]
    fn the_window_covers_every_capability() {
        // The window is the fuller surface today, so anything missing here is
        // a name in the enum that nothing implements.
        let missing: Vec<_> = Capability::ALL
            .iter()
            .filter(|capability| !named(GUI).contains(capability))
            .collect();
        assert!(missing.is_empty(), "no surface implements {missing:?}");
    }

    #[test]
    fn the_difference_between_the_surfaces_is_exactly_the_ledger() {
        let gaps: BTreeSet<Capability> = named(GUI).difference(&named(TUI)).copied().collect();
        let recorded = named(KNOWN_GAPS);

        let unrecorded: Vec<_> = gaps.difference(&recorded).collect();
        assert!(
            unrecorded.is_empty(),
            "the window gained {unrecorded:?} without the terminal. \
             Build it there, or record it in KNOWN_GAPS with a reason."
        );

        let closed: Vec<_> = recorded.difference(&gaps).collect();
        assert!(
            closed.is_empty(),
            "the terminal has {closed:?} now, so remove those from KNOWN_GAPS."
        );
    }

    #[test]
    fn the_terminal_does_everything_the_window_does() {
        // The plain statement, so a reader of this file does not have to
        // derive it from an empty ledger.
        assert_eq!(
            named(GUI),
            named(TUI),
            "the two surfaces have drifted apart again"
        );
        assert!(
            KNOWN_GAPS.is_empty(),
            "the ledger has entries but the surfaces agree, so remove them"
        );
    }

    #[test]
    fn a_gap_is_never_a_capability_the_terminal_already_has() {
        for (capability, _) in KNOWN_GAPS {
            assert!(
                !named(TUI).contains(capability),
                "{capability:?} is recorded as missing but the terminal lists it"
            );
        }
    }
}
