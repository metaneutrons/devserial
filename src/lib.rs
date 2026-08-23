// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

pub mod cli;
pub mod config;
pub mod engine;
#[cfg(feature = "esp")]
pub mod esp;
pub mod ipc;
pub mod modem;
#[cfg(feature = "monitor")]
pub mod monitor;
pub mod port_manager;
pub mod protocol;
pub mod reader;
pub mod server;
pub mod standalone;
pub mod state;
pub mod storage;
#[cfg(feature = "tui")]
pub mod tui;

#[cfg(any(test, feature = "testutil"))]
pub mod testutil;
