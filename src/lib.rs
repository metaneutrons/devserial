// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

/// Copyright line shown wherever the program names itself.
///
/// The year lives here rather than at each site that prints it. `resources/`
/// `macos.m` carries its own copy for the native About panel, which cannot
/// read a Rust constant.
pub const COPYRIGHT: &str = "\u{a9} 2026 Fabian Schmieder";

pub mod assets;
pub mod cli;
pub mod config;
pub mod engine;
#[cfg(feature = "esp")]
pub mod esp;
pub mod export;
#[cfg(feature = "monitor")]
pub mod gui_ipc;
pub mod hex;
pub mod ipc;
pub mod modem;
#[cfg(feature = "monitor")]
pub mod monitor;
pub mod paths;
pub mod platform;
pub mod port_manager;
pub mod protocol;
pub mod reader;
pub mod serial_params;
pub mod server;
pub mod standalone;
pub mod state;
pub mod storage;
pub mod transport;
#[cfg(feature = "tui")]
pub mod tui;

#[cfg(any(test, feature = "testutil"))]
pub mod testutil;
