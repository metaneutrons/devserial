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

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use espflash::connection::{Connection, ResetAfterOperation, ResetBeforeOperation};
use espflash::flasher::{FlashData, FlashSettings, FlashSize, Flasher};
use espflash::image_format::Metadata;
use espflash::image_format::idf::IdfBootloaderFormat;
use espflash::target::ProgressCallbacks;
use serialport::{FlowControl, SerialPortInfo, SerialPortType, UsbPortInfo};

use crate::protocol::EspResetBefore;

/// The baud rate the chip's ROM bootloader starts at.
const CONNECT_BAUD: u32 = 115_200;

/// The USB product id of the native USB Serial/JTAG peripheral (VID `303a`).
const USB_SERIAL_JTAG_PID: u16 = 0x1001;

/// The first bytes of every ELF file.
const ELF_MAGIC: &[u8] = b"\x7fELF";

/// How long a device that restarted is given to show up again.
const REAPPEAR_TIMEOUT: Duration = Duration::from_secs(8);

/// How often the list of ports is read while waiting for it.
const REAPPEAR_POLL: Duration = Duration::from_millis(250);

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

/// What the reset choice comes to for this USB identity, in words.
///
/// This restates how `espflash` 4.6 picks its sequence
/// (`construct_reset_strategy_sequence`): a native USB Serial/JTAG device, or a
/// forced `usb-reset`, gets the USB Serial/JTAG sequence; anything else gets the
/// UART bridge sequences on DTR and RTS. The library does not say which it
/// used, so read `reset.rs` again when `espflash` is updated.
fn describe_reset(reset: EspResetBefore, usb: &UsbPortInfo) -> String {
    let identity = if usb.vid == 0 && usb.pid == 0 {
        "no USB identity".to_string()
    } else {
        let serial = usb
            .serial_number
            .as_deref()
            .map_or_else(String::new, |serial| format!(", serial {serial}"));
        format!("USB {:04x}:{:04x}{serial}", usb.vid, usb.pid)
    };
    let how = match reset {
        EspResetBefore::NoResetNoSync => {
            "no reset and no synchronization, the loader is assumed to be running and in sync"
        }
        EspResetBefore::NoReset => "no reset, the ROM loader is expected to be running already",
        EspResetBefore::UsbReset => "USB Serial/JTAG reset, as chosen",
        EspResetBefore::Auto if usb.pid == USB_SERIAL_JTAG_PID => {
            "USB Serial/JTAG reset, chosen from the USB identity"
        }
        EspResetBefore::Auto => "reset through the UART bridge on DTR and RTS",
    };
    format!("{reset}, {how} ({identity})")
}

/// Open `port` and connect to the chip behind it, saying which reset it uses.
fn connect(
    port: &str,
    baud: Option<u32>,
    reset: EspResetBefore,
    report: &mut Report,
) -> Result<Flasher, String> {
    let serial = serialport::new(port, CONNECT_BAUD)
        .flow_control(FlowControl::None)
        .open_native()
        .map_err(|error| format!("could not open {port}: {error}"))?;
    let usb = usb_info(port);
    let plan = describe_reset(reset, &usb);
    report.line(format!("Reset before connect: {plan}"));
    let connection = Connection::new(
        serial,
        usb,
        ResetAfterOperation::HardReset,
        match reset {
            EspResetBefore::Auto => ResetBeforeOperation::DefaultReset,
            EspResetBefore::UsbReset => ResetBeforeOperation::UsbReset,
            EspResetBefore::NoReset => ResetBeforeOperation::NoReset,
            EspResetBefore::NoResetNoSync => ResetBeforeOperation::NoResetNoSync,
        },
        baud.unwrap_or(CONNECT_BAUD),
    );
    // Use the flasher stub, verify what was written, and skip regions that are
    // already as they should be: the defaults of the `espflash` command.
    Flasher::connect(connection, true, true, true, None, baud).map_err(|error| {
        format!(
            "could not connect to a chip on {port} (reset before connect: {plan}): {error}. \
             Nothing was erased or written. To use a loader that is already in download mode, \
             choose the reset `no-reset`. A native USB chip is put into download mode by hand \
             first, typically by holding BOOT and pressing RESET."
        )
    })
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

/// Refuse a file that is not an ELF image before the chip is touched.
///
/// `flash` builds its image from an ELF. Handed a raw binary it used to reset
/// the chip, read its information, and only then fail with a message about the
/// ELF, which is a reset for nothing.
fn ensure_elf(firmware: &Path, image: &[u8]) -> Result<(), String> {
    if image.starts_with(ELF_MAGIC) {
        return Ok(());
    }
    Err(format!(
        "{} is not an ELF image, and `flash` takes only ELF images. A raw or merged binary is \
         written to a flash address with `serial_esp_write_bin` or \
         `POST /v1/ports/{{port}}/esp/write-bin`; an image merged from bootloader, partition \
         table and application starts at address 0. The chip was not touched.",
        firmware.display()
    ))
}

/// What identifies a USB serial device again after it has restarted.
///
/// The path of a native USB device can change when it re-enumerates, and the
/// system may give it another one. The vendor, product and serial number do not
/// change. Without a serial number the identity is not unique, so there is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbIdentity {
    vid: u16,
    pid: u16,
    serial: String,
}

impl UsbIdentity {
    fn of(info: &SerialPortInfo) -> Option<Self> {
        let SerialPortType::UsbPort(usb) = &info.port_type else {
            return None;
        };
        let serial = usb
            .serial_number
            .as_deref()
            .filter(|serial| !serial.is_empty())?;
        Some(Self {
            vid: usb.vid,
            pid: usb.pid,
            serial: serial.to_owned(),
        })
    }
}

/// What is known of a port before an operation that may restart the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watch {
    /// The system listed the port under this very path. A path it never lists,
    /// a symbolic link or a virtual port, is not waited for: it would not
    /// show up in the list afterwards either.
    listed: bool,
    /// The identity of the device, if it is USB with a serial number.
    identity: Option<UsbIdentity>,
}

impl Watch {
    fn of(listed: &[SerialPortInfo], port: &str) -> Self {
        let found = listed.iter().find(|candidate| candidate.port_name == port);
        Self {
            listed: found.is_some(),
            identity: found.and_then(UsbIdentity::of),
        }
    }
}

/// Note what the system lists for `port` now, to find the device again later.
pub async fn watch(port: &str) -> Watch {
    Watch::of(&listed_ports().await, port)
}

/// The serial ports the system lists, read off the async threads.
async fn listed_ports() -> Vec<SerialPortInfo> {
    tokio::task::spawn_blocking(|| serialport::available_ports().unwrap_or_default())
        .await
        .unwrap_or_default()
}

/// Where a device is after it restarted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Located {
    /// The port is listed under its old path.
    Here,
    /// The old path is gone and the same device is listed under this one.
    MovedTo(String),
    /// The old path is gone and more than one device carries the identity.
    Ambiguous,
    /// The old path is gone and nothing carries the identity.
    Missing,
}

/// The part of a path that stays when only its number changes: `/dev/ttyACM`
/// for `/dev/ttyACM0`, `/dev/cu.usbmodem` for `/dev/cu.usbmodem1101`, `COM`
/// for `COM5`. macOS lists both the `cu` and the `tty` node of a device, and
/// only the one of the same family is the same port.
fn family(path: &str) -> &str {
    path.trim_end_matches(|character: char| character.is_ascii_digit())
}

/// Find the device that was on `port`, among the ports listed now.
fn locate(listed: &[SerialPortInfo], port: &str, identity: Option<&UsbIdentity>) -> Located {
    if listed.iter().any(|candidate| candidate.port_name == port) {
        return Located::Here;
    }
    let Some(identity) = identity else {
        return Located::Missing;
    };
    let mut found = listed
        .iter()
        .filter(|candidate| family(&candidate.port_name) == family(port))
        .filter(|candidate| UsbIdentity::of(candidate).as_ref() == Some(identity))
        .map(|candidate| candidate.port_name.clone());
    match (found.next(), found.next()) {
        (Some(path), None) => Located::MovedTo(path),
        (Some(_), Some(_)) => Located::Ambiguous,
        _ => Located::Missing,
    }
}

/// Wait until the device that was on `port` is listed again, under its old path
/// or under another one, or until `timeout` has passed. A port the system did
/// not list before is not waited for.
///
/// A chip that was reset re-enumerates, which takes some time and can leave the
/// device on a different path. Reopening at once, on the old path, then fails
/// or opens something else.
pub async fn wait_until_listed<L, F>(
    mut list: L,
    port: &str,
    watch: &Watch,
    poll: Duration,
    timeout: Duration,
) -> Located
where
    L: FnMut() -> F,
    F: Future<Output = Vec<SerialPortInfo>>,
{
    if !watch.listed {
        return Located::Here;
    }
    let started = Instant::now();
    loop {
        let found = locate(&list().await, port, watch.identity.as_ref());
        if matches!(found, Located::Here | Located::MovedTo(_)) || started.elapsed() >= timeout {
            return found;
        }
        tokio::time::sleep(poll).await;
    }
}

/// [`wait_until_listed`] against the ports the system lists.
pub async fn wait_for_port(port: &str, watch: &Watch) -> Located {
    wait_until_listed(listed_ports, port, watch, REAPPEAR_POLL, REAPPEAR_TIMEOUT).await
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
pub async fn flash(
    port: &str,
    firmware_path: &str,
    baud: Option<u32>,
    reset: EspResetBefore,
) -> Result<String, String> {
    flash_with_progress(port, firmware_path, baud, reset, None).await
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
    reset: EspResetBefore,
    progress: Option<&Progress>,
) -> Result<String, String> {
    let (port, firmware, sink) = (
        port.to_owned(),
        PathBuf::from(firmware_path),
        progress.cloned(),
    );
    blocking(move || flash_blocking(&port, &firmware, baud, reset, Report::new(sink))).await
}

fn flash_blocking(
    port: &str,
    firmware: &Path,
    baud: Option<u32>,
    reset: EspResetBefore,
    mut report: Report,
) -> Result<String, String> {
    // Read and check the image first: a missing file, or one that is not an
    // ELF, should not cost a reset of the chip.
    let elf = std::fs::read(firmware)
        .map_err(|error| format!("could not read {}: {error}", firmware.display()))?;
    ensure_elf(firmware, &elf)?;
    let mut flasher = connect(port, baud, reset, &mut report)?;
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

/// Let the chip leave the loader and run its application again.
///
/// The `espflash` library resets after a write on its own, but not after
/// `info` or `erase`: its command line does that itself. Without it the chip
/// stays in the ROM loader after an `info` and its application never runs
/// again until something resets it. A failure is noted, not returned, because
/// the answer the caller asked for is already in hand.
fn restart(flasher: &mut Flasher, report: &mut Report) {
    let chip = flasher.chip();
    // `HardReset`, which is what `connect` asks for, does not look at the stub.
    match flasher.connection().reset_after(true, chip) {
        Ok(()) => report.line("Reset the chip, so that it runs its application again"),
        Err(error) => report.line(format!(
            "The chip could not be reset afterwards ({error}) and may still be in the loader"
        )),
    }
}

/// Get board and chip information.
///
/// # Errors
/// Returns error if the chip cannot be reached.
pub async fn board_info(port: &str, reset: EspResetBefore) -> Result<String, String> {
    let port = port.to_owned();
    blocking(move || {
        let mut report = Report::new(None);
        let mut flasher = connect(&port, None, reset, &mut report)?;
        describe(&mut flasher, &mut report)?;
        restart(&mut flasher, &mut report);
        Ok(report.into_output())
    })
    .await
}

/// Erase the entire flash.
///
/// # Errors
/// Returns error if the chip cannot be reached or the erase fails.
pub async fn erase_flash(port: &str, reset: EspResetBefore) -> Result<String, String> {
    erase_flash_with_progress(port, reset, None).await
}

/// Erase the entire flash, reporting each line as it is written.
///
/// # Errors
/// Returns error if the chip cannot be reached or the erase fails.
pub async fn erase_flash_with_progress(
    port: &str,
    reset: EspResetBefore,
    progress: Option<&Progress>,
) -> Result<String, String> {
    let (port, sink) = (port.to_owned(), progress.cloned());
    blocking(move || {
        let mut report = Report::new(sink);
        let mut flasher = connect(&port, None, reset, &mut report)?;
        describe(&mut flasher, &mut report)?;
        report.line("Erasing the flash");
        flasher
            .erase_flash()
            .map_err(|error| format!("erasing failed: {error}"))?;
        report.line("Flash has been erased");
        restart(&mut flasher, &mut report);
        Ok(report.into_output())
    })
    .await
}

/// Write a binary file to a specific flash address.
///
/// # Errors
/// Returns error if the address or the file is not usable, the chip cannot be
/// reached or the write fails.
pub async fn write_bin(
    port: &str,
    file_path: &str,
    address: &str,
    reset: EspResetBefore,
) -> Result<String, String> {
    let address = parse_address(address)?;
    let (port, file) = (port.to_owned(), PathBuf::from(file_path));
    blocking(move || {
        let data = std::fs::read(&file)
            .map_err(|error| format!("could not read {}: {error}", file.display()))?;
        let mut report = Report::new(None);
        let mut flasher = connect(&port, None, reset, &mut report)?;
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
            EspResetBefore::Auto,
        )
        .await
        .unwrap_err();
        assert!(error.contains("/nonexistent/firmware.elf"), "{error}");
    }

    #[tokio::test]
    async fn a_missing_port_is_named() {
        let error = board_info("/dev/nonexistent_port_xyz", EspResetBefore::Auto)
            .await
            .unwrap_err();
        assert!(error.contains("/dev/nonexistent_port_xyz"), "{error}");
        let error = erase_flash("/dev/nonexistent_port_xyz", EspResetBefore::Auto)
            .await
            .unwrap_err();
        assert!(error.contains("/dev/nonexistent_port_xyz"), "{error}");
    }

    fn usb(name: &str, vid: u16, pid: u16, serial: Option<&str>) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.to_string(),
            port_type: SerialPortType::UsbPort(UsbPortInfo {
                vid,
                pid,
                serial_number: serial.map(str::to_string),
                manufacturer: None,
                product: None,
            }),
        }
    }

    fn identity(serial: &str) -> UsbIdentity {
        UsbIdentity {
            vid: 0x303a,
            pid: USB_SERIAL_JTAG_PID,
            serial: serial.to_string(),
        }
    }

    fn watching(serial: &str) -> Watch {
        Watch {
            listed: true,
            identity: Some(identity(serial)),
        }
    }

    #[test]
    fn only_an_elf_image_is_taken_by_flash() {
        let path = Path::new("merged.bin");
        assert!(ensure_elf(path, b"\x7fELF\x02\x01\x01").is_ok());
        let error = ensure_elf(path, &[0xff; 64]).unwrap_err();
        assert!(error.contains("merged.bin is not an ELF image"), "{error}");
        assert!(error.contains("serial_esp_write_bin"), "{error}");
        assert!(error.contains("The chip was not touched."), "{error}");
        assert!(ensure_elf(path, b"").is_err());
    }

    #[tokio::test]
    async fn a_file_that_is_not_an_elf_is_refused_before_the_port_is_opened() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("merged.bin");
        std::fs::write(&file, [0xff_u8; 128]).unwrap();
        let error = flash(
            "/dev/nonexistent_port_xyz",
            file.to_str().unwrap(),
            None,
            EspResetBefore::Auto,
        )
        .await
        .unwrap_err();
        // Opening the port would have failed first, so this shows that the
        // image is checked before anything is done with the port.
        assert!(error.contains("is not an ELF image"), "{error}");
        assert!(!error.contains("could not open"), "{error}");
    }

    #[test]
    fn the_reset_choice_is_described_for_the_identity_it_meets() {
        let native = UsbPortInfo {
            vid: 0x303a,
            pid: USB_SERIAL_JTAG_PID,
            serial_number: Some("AA:BB".to_string()),
            manufacturer: None,
            product: None,
        };
        let bridge = UsbPortInfo {
            vid: 0x10c4,
            pid: 0xea60,
            serial_number: None,
            manufacturer: None,
            product: None,
        };
        let unlisted = usb("x", 0, 0, None);
        let SerialPortType::UsbPort(unlisted) = unlisted.port_type else {
            unreachable!()
        };

        let auto_native = describe_reset(EspResetBefore::Auto, &native);
        assert!(
            auto_native.contains("USB Serial/JTAG reset, chosen"),
            "{auto_native}"
        );
        assert!(
            auto_native.contains("303a:1001, serial AA:BB"),
            "{auto_native}"
        );
        let auto_bridge = describe_reset(EspResetBefore::Auto, &bridge);
        assert!(auto_bridge.contains("UART bridge"), "{auto_bridge}");
        assert!(auto_bridge.contains("10c4:ea60"), "{auto_bridge}");
        // A forced usb-reset is what it says, whatever the identity.
        let forced = describe_reset(EspResetBefore::UsbReset, &bridge);
        assert!(
            forced.contains("USB Serial/JTAG reset, as chosen"),
            "{forced}"
        );
        // The manual workflow names what it does not do.
        let manual = describe_reset(EspResetBefore::NoReset, &native);
        assert!(manual.contains("no reset"), "{manual}");
        let unsynced = describe_reset(EspResetBefore::NoResetNoSync, &native);
        assert!(unsynced.contains("no synchronization"), "{unsynced}");
        assert!(describe_reset(EspResetBefore::Auto, &unlisted).contains("no USB identity"));
    }

    #[test]
    fn a_device_is_found_again_under_its_old_path() {
        let listed = [usb("/dev/cu.usbmodem1101", 0x303a, 0x1001, Some("AA"))];
        assert_eq!(
            locate(&listed, "/dev/cu.usbmodem1101", Some(&identity("AA"))),
            Located::Here
        );
        // Listed, so it stays where it is, whatever else carries the identity.
        let both = [
            usb("/dev/cu.usbmodem1101", 0x303a, 0x1001, Some("BB")),
            usb("/dev/cu.usbmodem11201", 0x303a, 0x1001, Some("AA")),
        ];
        assert_eq!(
            locate(&both, "/dev/cu.usbmodem1101", Some(&identity("AA"))),
            Located::Here
        );
    }

    #[test]
    fn a_device_that_came_back_on_another_path_is_found_by_its_identity() {
        let listed = [
            usb("/dev/cu.usbmodem11201", 0x303a, 0x1001, Some("AA")),
            // The tty node of the same device is not the same port.
            usb("/dev/tty.usbmodem11201", 0x303a, 0x1001, Some("AA")),
            usb("/dev/cu.usbmodem2101", 0x10c4, 0xea60, Some("ZZ")),
        ];
        assert_eq!(
            locate(&listed, "/dev/cu.usbmodem1101", Some(&identity("AA"))),
            Located::MovedTo("/dev/cu.usbmodem11201".to_string())
        );
        let linux = [usb("/dev/ttyACM1", 0x303a, 0x1001, Some("AA"))];
        assert_eq!(
            locate(&linux, "/dev/ttyACM0", Some(&identity("AA"))),
            Located::MovedTo("/dev/ttyACM1".to_string())
        );
        let windows = [usb("COM7", 0x303a, 0x1001, Some("AA"))];
        assert_eq!(
            locate(&windows, "COM5", Some(&identity("AA"))),
            Located::MovedTo("COM7".to_string())
        );
    }

    #[test]
    fn a_device_is_not_guessed() {
        let other_device = [usb("/dev/cu.usbmodem11201", 0x303a, 0x1001, Some("BB"))];
        assert_eq!(
            locate(&other_device, "/dev/cu.usbmodem1101", Some(&identity("AA"))),
            Located::Missing
        );
        // Without an identity there is nothing to look for.
        let same_model = [usb("/dev/cu.usbmodem11201", 0x303a, 0x1001, Some("AA"))];
        assert_eq!(
            locate(&same_model, "/dev/cu.usbmodem1101", None),
            Located::Missing
        );
        // Two nodes of the same family with the identity: no choice is made.
        let twice = [
            usb("/dev/cu.usbmodem11201", 0x303a, 0x1001, Some("AA")),
            usb("/dev/cu.usbmodem11301", 0x303a, 0x1001, Some("AA")),
        ];
        assert_eq!(
            locate(&twice, "/dev/cu.usbmodem1101", Some(&identity("AA"))),
            Located::Ambiguous
        );
        // A port without a serial number has no identity at all.
        assert_eq!(UsbIdentity::of(&usb("p", 1, 2, None)), None);
        assert_eq!(UsbIdentity::of(&usb("p", 1, 2, Some(""))), None);
    }

    #[tokio::test]
    async fn the_wait_ends_when_the_device_is_listed_again() {
        let mut reads = 0_u32;
        let found = wait_until_listed(
            || {
                reads += 1;
                let listing = match reads {
                    1 | 2 => vec![],
                    _ => vec![usb("/dev/cu.usbmodem11201", 0x303a, 0x1001, Some("AA"))],
                };
                std::future::ready(listing)
            },
            "/dev/cu.usbmodem1101",
            &watching("AA"),
            Duration::from_millis(1),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(found, Located::MovedTo("/dev/cu.usbmodem11201".to_string()));
        assert_eq!(reads, 3);
    }

    #[tokio::test]
    async fn the_wait_is_over_at_once_when_the_port_is_there() {
        let started = Instant::now();
        let found = wait_until_listed(
            || std::future::ready(vec![usb("COM5", 0x303a, 0x1001, Some("AA"))]),
            "COM5",
            &watching("AA"),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(found, Located::Here);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn the_wait_gives_up_when_the_device_never_comes_back() {
        let found = wait_until_listed(
            || std::future::ready(Vec::new()),
            "COM5",
            &watching("AA"),
            Duration::from_millis(1),
            Duration::from_millis(30),
        )
        .await;
        assert_eq!(found, Located::Missing);
    }

    #[tokio::test]
    async fn a_port_the_system_never_listed_is_not_waited_for() {
        // A symbolic link or a virtual port is not in the list under its own
        // name, before or after, so waiting for it would only cost the timeout.
        let listed = [usb("/dev/ttyACM0", 0x303a, 0x1001, Some("AA"))];
        let watch = Watch::of(&listed, "/dev/serial/by-id/usb-Espressif-AA");
        assert!(!watch.listed);
        let started = Instant::now();
        let found = wait_until_listed(
            || std::future::ready(Vec::new()),
            "/dev/serial/by-id/usb-Espressif-AA",
            &watch,
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(found, Located::Here);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_listed_port_is_noted_with_its_identity() {
        let listed = [
            usb("/dev/cu.usbmodem1101", 0x303a, 0x1001, Some("AA")),
            usb("/dev/cu.usbmodem2101", 0x10c4, 0xea60, None),
        ];
        let with = Watch::of(&listed, "/dev/cu.usbmodem1101");
        assert!(with.listed);
        assert_eq!(with.identity, Some(identity("AA")));
        // Listed, but without a serial number there is no identity to look for.
        let without = Watch::of(&listed, "/dev/cu.usbmodem2101");
        assert!(without.listed);
        assert_eq!(without.identity, None);
    }
}
