// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! `DevSerial` binary entrypoint.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    devserial::cli::run()
}
