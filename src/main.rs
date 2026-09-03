// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! `DevSerial` binary entrypoint.
//!
//! The library reports failures as errors; the process exit code is decided
//! here and nowhere else.

use std::process::ExitCode;

fn main() -> ExitCode {
    match devserial::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("devserial: {error}");
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}
