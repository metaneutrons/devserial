// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

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
