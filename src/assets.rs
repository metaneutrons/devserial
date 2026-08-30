// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Embedded binary assets.
//!
//! One definition per asset. `resources/icon.png` is the 1024 pixel master and
//! stays out of the binary; `resources/icon-512.png` is the derived size that
//! is embedded, which keeps roughly half a megabyte out of every build.

/// Application icon, used for the window, the dock and the about panel.
#[cfg(feature = "monitor")]
pub const ICON_PNG: &[u8] = include_bytes!("../resources/icon-512.png");

/// Builds without a GUI embed no icon.
#[cfg(not(feature = "monitor"))]
pub const ICON_PNG: &[u8] = &[];
