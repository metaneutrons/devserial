// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! ESP tooling through the `espflash` library.
//!
//! The library speaks to the chip over a serial port of its own, opened here
//! with `serialport`; devserial's ports are `serial2`. The two do not share a
//! handle, which is why the callers release the port before an operation and
//! open it again afterwards.
//!
//! Everything blocks, so each operation runs on a thread of its own, and the
//! lines it reports travel over the channel the caller handed in.

use std::path::{Path, PathBuf};

use espflash::connection::{Connection, ResetAfterOperation, ResetBeforeOperation};
use espflash::flasher::{FlashData, FlashSettings, FlashSize, Flasher};
use espflash::image_format::Metadata;
use espflash::image_format::idf::IdfBootloaderFormat;
use espflash::target::ProgressCallbacks;
use serialport::{FlowControl, SerialPortType, UsbPortInfo};

/// The baud rate the chip's ROM bootloader starts at.
const CONNECT_BAUD: u32 = 115_200;

/// Which of the tool's two streams a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// One line of tool output, delivered while the tool is still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLine {
    pub stream: Stream,
    pub text: String,
}

/// Where live output goes while a long operation runs.
///
/// Flashing takes tens of seconds. A caller that only gets the output at the
/// end cannot tell a working flash from a hung one, so it can hand over a
/// channel and read the lines as they arrive. The full output still comes back
/// from the call itself, so a caller that does not care changes nothing.
pub type Progress = tokio::sync::mpsc::UnboundedSender<OutputLine>;

/// What an operation has said so far, and where it goes meanwhile.
struct Report {
    sink: Option<Progress>,
    lines: Vec<String>,
}

impl Report {
    const fn new(sink: Option<Progress>) -> Self {
        Self {
            sink,
            lines: Vec::new(),
        }
    }

    fn line(&mut self, text: impl Into<String>) {
        let text = text.into();
        if let Some(sink) = &self.sink {
            // A closed receiver means the caller stopped listening. The
            // operation carries on and the full output still reaches the
            // return value.
            let _ = sink.send(OutputLine {
                stream: Stream::Stdout,
                text: text.clone(),
            });
        }
        self.lines.push(text);
    }

    fn into_output(self) -> String {
        self.lines.join("\n")
    }
}

/// Reports the progress of a write as a line at every tenth of it.
struct LineProgress<'a> {
    report: &'a mut Report,
    total: usize,
    reached: usize,
}

impl<'a> LineProgress<'a> {
    const fn new(report: &'a mut Report) -> Self {
        Self {
            report,
            total: 0,
            reached: 0,
        }
    }
}

impl ProgressCallbacks for LineProgress<'_> {
    fn init(&mut self, addr: u32, total: usize) {
        self.total = total;
        self.reached = 0;
        self.report
            .line(format!("Writing {total} bytes at {addr:#x}"));
    }

    fn update(&mut self, current: usize) {
        let tenth = tenth_of(current, self.total);
        if tenth > self.reached {
            self.reached = tenth;
            self.report.line(format!("  {}%", tenth * 10));
        }
    }

    fn verifying(&mut self) {
        self.report.line("Verifying");
    }

    fn finish(&mut self, skipped: bool) {
        self.report.line(if skipped {
            "Unchanged, skipped"
        } else {
            "Written"
        });
    }
}

/// How many tenths of `total` `current` is, from 0 to 10.
fn tenth_of(current: usize, total: usize) -> usize {
    if total == 0 {
        return 10;
    }
    (current.min(total).saturating_mul(10) / total).min(10)
}

/// Run a blocking operation on a thread of its own.
async fn blocking<F>(operation: F) -> Result<String, String>
where
    F: FnOnce() -> Result<String, String> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| format!("the ESP operation ended without an answer: {error}"))?
}

/// Open `port` and connect to the chip behind it.
fn connect(port: &str, baud: Option<u32>) -> Result<Flasher, String> {
    let serial = serialport::new(port, CONNECT_BAUD)
        .flow_control(FlowControl::None)
        .open_native()
        .map_err(|error| format!("could not open {port}: {error}"))?;
    let connection = Connection::new(
        serial,
        usb_info(port),
        ResetAfterOperation::HardReset,
        ResetBeforeOperation::DefaultReset,
        baud.unwrap_or(CONNECT_BAUD),
    );
    // Use the flasher stub, verify what was written, and skip regions that are
    // already as they should be: the defaults of the `espflash` command.
    Flasher::connect(connection, true, true, true, None, baud)
        .map_err(|error| format!("could not connect to a chip on {port}: {error}"))
}

/// The USB identity of `port`, which picks the reset sequence for the chip.
/// A port the system does not list as USB gets zeros, as the `espflash`
/// command does.
fn usb_info(port: &str) -> UsbPortInfo {
    let found = serialport::available_ports()
        .ok()
        .into_iter()
        .flatten()
        .find(|candidate| candidate.port_name == port);
    match found.map(|candidate| candidate.port_type) {
        Some(SerialPortType::UsbPort(info)) => info,
        _ => UsbPortInfo {
            vid: 0,
            pid: 0,
            serial_number: None,
            manufacturer: None,
            product: None,
        },
    }
}

/// Describe the chip, as the `board-info` command does.
fn describe(flasher: &mut Flasher, report: &mut Report) -> Result<(), String> {
    let info = flasher
        .device_info()
        .map_err(|error| format!("could not read the board's information: {error}"))?;
    let revision = info.revision.map_or_else(String::new, |(major, minor)| {
        format!(" (revision v{major}.{minor})")
    });
    report.line(format!("Chip type:         {}{revision}", info.chip));
    report.line(format!("Crystal frequency: {}", info.crystal_frequency));
    report.line(format!("Flash size:        {}", info.flash_size));
    report.line(format!("Features:          {}", info.features.join(", ")));
    if let Some(mac) = info.mac_address {
        report.line(format!("MAC address:       {mac}"));
    }
    Ok(())
}

/// Flash firmware to an ESP device.
///
/// # Errors
/// Returns error if the firmware cannot be read, the chip cannot be reached
/// or the write fails.
pub async fn flash(port: &str, firmware_path: &str, baud: Option<u32>) -> Result<String, String> {
    flash_with_progress(port, firmware_path, baud, None).await
}

/// Flash firmware to an ESP device, reporting each line as it is written.
///
/// # Errors
/// Returns error if the firmware cannot be read, the chip cannot be reached
/// or the write fails.
pub async fn flash_with_progress(
    port: &str,
    firmware_path: &str,
    baud: Option<u32>,
    progress: Option<&Progress>,
) -> Result<String, String> {
    let (port, firmware, sink) = (
        port.to_owned(),
        PathBuf::from(firmware_path),
        progress.cloned(),
    );
    blocking(move || flash_blocking(&port, &firmware, baud, Report::new(sink))).await
}

fn flash_blocking(
    port: &str,
    firmware: &Path,
    baud: Option<u32>,
    mut report: Report,
) -> Result<String, String> {
    // Read the image first: a missing file should not cost a reset of the chip.
    let elf = std::fs::read(firmware)
        .map_err(|error| format!("could not read {}: {error}", firmware.display()))?;
    let mut flasher = connect(port, baud)?;
    describe(&mut flasher, &mut report)?;

    let chip = flasher.chip();
    let metadata = Metadata::from_bytes(Some(&elf));
    if let Some(built_for) = metadata.chip_name()
        && !built_for.eq_ignore_ascii_case(&chip.to_string())
    {
        return Err(format!(
            "this firmware is built for {built_for}, the chip on {port} is {chip}"
        ));
    }
    if !flasher.secure_download_mode() {
        let wanted = metadata.min_chip_revision().unwrap_or(0);
        if wanted > 0 {
            flasher
                .verify_minimum_revision(wanted)
                .map_err(|error| error.to_string())?;
        }
    }

    let xtal = chip
        .xtal_frequency(flasher.connection())
        .map_err(|error| format!("could not read the crystal frequency: {error}"))?;
    let size = flasher
        .flash_detect()
        .ok()
        .flatten()
        .unwrap_or_else(FlashSize::default);
    flasher.set_flash_size(size);
    let flash_data = FlashData::new(
        FlashSettings::new(None, Some(size), None),
        0,
        None,
        chip,
        xtal,
    );
    let image = IdfBootloaderFormat::new(&elf, &flash_data, None, None, None, None)
        .map_err(|error| format!("could not build the flash image: {error}"))?;

    report.line(format!("Flashing {}", firmware.display()));
    flasher
        .load_image_to_flash(&mut LineProgress::new(&mut report), image.into())
        .map_err(|error| format!("flashing failed: {error}"))?;
    report.line("Flashing has completed");
    Ok(report.into_output())
}

/// Get board and chip information.
///
/// # Errors
/// Returns error if the chip cannot be reached.
pub async fn board_info(port: &str) -> Result<String, String> {
    let port = port.to_owned();
    blocking(move || {
        let mut report = Report::new(None);
        let mut flasher = connect(&port, None)?;
        describe(&mut flasher, &mut report)?;
        Ok(report.into_output())
    })
    .await
}

/// Erase the entire flash.
///
/// # Errors
/// Returns error if the chip cannot be reached or the erase fails.
pub async fn erase_flash(port: &str) -> Result<String, String> {
    erase_flash_with_progress(port, None).await
}

/// Erase the entire flash, reporting each line as it is written.
///
/// # Errors
/// Returns error if the chip cannot be reached or the erase fails.
pub async fn erase_flash_with_progress(
    port: &str,
    progress: Option<&Progress>,
) -> Result<String, String> {
    let (port, sink) = (port.to_owned(), progress.cloned());
    blocking(move || {
        let mut report = Report::new(sink);
        let mut flasher = connect(&port, None)?;
        describe(&mut flasher, &mut report)?;
        report.line("Erasing the flash");
        flasher
            .erase_flash()
            .map_err(|error| format!("erasing failed: {error}"))?;
        report.line("Flash has been erased");
        Ok(report.into_output())
    })
    .await
}

/// Write a binary file to a specific flash address.
///
/// # Errors
/// Returns error if the address or the file is not usable, the chip cannot be
/// reached or the write fails.
pub async fn write_bin(port: &str, file_path: &str, address: &str) -> Result<String, String> {
    let address = parse_address(address)?;
    let (port, file) = (port.to_owned(), PathBuf::from(file_path));
    blocking(move || {
        let data = std::fs::read(&file)
            .map_err(|error| format!("could not read {}: {error}", file.display()))?;
        let mut report = Report::new(None);
        let mut flasher = connect(&port, None)?;
        describe(&mut flasher, &mut report)?;
        flasher
            .write_bin_to_flash(address, &data, &mut LineProgress::new(&mut report))
            .map_err(|error| format!("writing failed: {error}"))?;
        report.line("Binary successfully written to flash");
        Ok(report.into_output())
    })
    .await
}

/// A flash address as the tool takes it: decimal, or hexadecimal with `0x`.
fn parse_address(text: &str) -> Result<u32, String> {
    let text = text.trim();
    let parsed = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .map_or_else(|| text.parse::<u32>(), |hex| u32::from_str_radix(hex, 16));
    parsed.map_err(|_| format!("`{text}` is not a flash address; use 0x10000 or 65536"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_read_in_decimal_and_hexadecimal() {
        assert_eq!(parse_address("0x10000"), Ok(0x10000));
        assert_eq!(parse_address(" 0X1000 "), Ok(0x1000));
        assert_eq!(parse_address("65536"), Ok(65536));
        // Counter-probes: text, a sign and a value past 32 bits are refused.
        assert!(parse_address("flash").is_err());
        assert!(parse_address("-1").is_err());
        assert!(parse_address("0x100000000").is_err());
    }

    #[test]
    fn progress_counts_in_tenths_and_never_past_the_end() {
        assert_eq!(tenth_of(0, 100), 0);
        assert_eq!(tenth_of(9, 100), 0);
        assert_eq!(tenth_of(10, 100), 1);
        assert_eq!(tenth_of(100, 100), 10);
        // More than the total, or nothing to write: still 10, not an overflow.
        assert_eq!(tenth_of(500, 100), 10);
        assert_eq!(tenth_of(0, 0), 10);
    }

    #[test]
    fn a_write_reports_a_line_at_each_tenth() {
        let (sink, mut lines) = tokio::sync::mpsc::unbounded_channel();
        let mut report = Report::new(Some(sink));
        {
            let mut progress = LineProgress::new(&mut report);
            progress.init(0x10000, 1000);
            for current in (0..=1000).step_by(50) {
                progress.update(current);
            }
            progress.verifying();
            progress.finish(false);
        }
        let mut seen = Vec::new();
        while let Ok(line) = lines.try_recv() {
            seen.push(line.text);
        }
        assert_eq!(seen.first().unwrap(), "Writing 1000 bytes at 0x10000");
        assert_eq!(seen.iter().filter(|line| line.ends_with('%')).count(), 10);
        assert!(seen.contains(&"  100%".to_string()));
        assert_eq!(seen.last().unwrap(), "Written");
        // The same lines are in the output the call returns.
        assert_eq!(report.into_output().lines().count(), seen.len());
    }

    #[tokio::test]
    async fn a_missing_firmware_file_is_named_before_the_port_is_touched() {
        let error = flash(
            "/dev/nonexistent_port_xyz",
            "/nonexistent/firmware.elf",
            None,
        )
        .await
        .unwrap_err();
        assert!(error.contains("/nonexistent/firmware.elf"), "{error}");
    }

    #[tokio::test]
    async fn a_missing_port_is_named() {
        let error = board_info("/dev/nonexistent_port_xyz").await.unwrap_err();
        assert!(error.contains("/dev/nonexistent_port_xyz"), "{error}");
        let error = erase_flash("/dev/nonexistent_port_xyz").await.unwrap_err();
        assert!(error.contains("/dev/nonexistent_port_xyz"), "{error}");
    }
}
