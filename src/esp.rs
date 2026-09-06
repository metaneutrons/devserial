// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! ESP tooling via the `espflash` subprocess.

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::OnceCell;

/// Name of the tool we shell out to.
const ESPFLASH: &str = "espflash";

/// Cached availability of the espflash binary.
static AVAILABLE: OnceCell<bool> = OnceCell::const_new();

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

/// Whether espflash is available in `PATH`.
///
/// The probe runs a subprocess, so it is asynchronous and cached. Calling it
/// from an async context used to block the runtime thread.
pub async fn is_available() -> bool {
    *AVAILABLE
        .get_or_init(|| async {
            match Command::new(ESPFLASH)
                .arg("--version")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await
            {
                Ok(output) if output.status.success() => {
                    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    tracing::info!(version = %version, "espflash detected");
                    true
                }
                Ok(_) => false,
                Err(e) => {
                    tracing::debug!(error = %e, "espflash not available");
                    false
                }
            }
        })
        .await
}

/// Flash firmware to an ESP device.
///
/// # Errors
/// Returns error if espflash fails or is not available.
pub async fn flash(port: &str, firmware_path: &str, baud: Option<u32>) -> Result<String, String> {
    flash_with_progress(port, firmware_path, baud, None).await
}

/// Flash firmware to an ESP device, reporting each line as it is written.
///
/// # Errors
/// Returns error if espflash fails or is not available.
pub async fn flash_with_progress(
    port: &str,
    firmware_path: &str,
    baud: Option<u32>,
    progress: Option<&Progress>,
) -> Result<String, String> {
    let mut cmd = Command::new(ESPFLASH);
    cmd.arg("flash").arg("--port").arg(port);
    if let Some(baud) = baud {
        cmd.arg("--baud").arg(baud.to_string());
    }
    // `--` keeps a path that begins with a dash from being read as an option.
    cmd.arg("--").arg(firmware_path);
    run(cmd, progress).await
}

/// Get board and chip information.
///
/// # Errors
/// Returns error if espflash fails or is not available.
pub async fn board_info(port: &str) -> Result<String, String> {
    let mut cmd = Command::new(ESPFLASH);
    cmd.arg("board-info").arg("--port").arg(port);
    run(cmd, None).await
}

/// Erase the entire flash.
///
/// # Errors
/// Returns error if espflash fails or is not available.
pub async fn erase_flash(port: &str) -> Result<String, String> {
    erase_flash_with_progress(port, None).await
}

/// Erase the entire flash, reporting each line as it is written.
///
/// # Errors
/// Returns error if espflash fails or is not available.
pub async fn erase_flash_with_progress(
    port: &str,
    progress: Option<&Progress>,
) -> Result<String, String> {
    let mut cmd = Command::new(ESPFLASH);
    cmd.arg("erase-flash").arg("--port").arg(port);
    run(cmd, progress).await
}

/// Write a binary file to a specific flash address.
///
/// # Errors
/// Returns error if espflash fails or is not available.
pub async fn write_bin(port: &str, file_path: &str, address: &str) -> Result<String, String> {
    let mut cmd = Command::new(ESPFLASH);
    cmd.arg("write-bin")
        .arg("--port")
        .arg(port)
        .arg("--")
        .arg(address)
        .arg(file_path);
    run(cmd, None).await
}

async fn run(mut cmd: Command, progress: Option<&Progress>) -> Result<String, String> {
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to run {ESPFLASH}: {e}"))?;

    // Both pipes are drained at the same time. Reading one to the end first
    // would block the tool as soon as it filled the other one.
    let out = child.stdout.take();
    let err = child.stderr.take();
    let (stdout, stderr) = tokio::join!(
        drain(out, Stream::Stdout, progress),
        drain(err, Stream::Stderr, progress),
    );

    let status = child
        .wait()
        .await
        .map_err(|e| format!("failed to wait for {ESPFLASH}: {e}"))?;

    if status.success() {
        Ok(join_streams(&stdout, &stderr))
    } else {
        let mut message = join_streams(&stderr, &stdout);
        if message.trim().is_empty() {
            message = format!("{ESPFLASH} exited with status {status}");
        }
        Err(message)
    }
}

/// Read one pipe to its end, forwarding lines and returning the raw text.
///
/// The returned string is what the pipe wrote, byte for byte as the previous
/// implementation returned it. The forwarded lines are a second, lossier view
/// of the same bytes and never replace it.
async fn drain<R>(reader: Option<R>, stream: Stream, progress: Option<&Progress>) -> String
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return String::new();
    };
    let mut raw: Vec<u8> = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if let Some(sink) = progress {
                    split_lines(&chunk[..n], &mut pending, |text| {
                        // A closed receiver means the caller stopped
                        // listening. The operation carries on and its full
                        // output still reaches the return value.
                        let _ = sink.send(OutputLine { stream, text });
                    });
                }
            }
        }
    }

    // A tool that ends without a final terminator still wrote a line.
    if let Some(sink) = progress
        && let Some(text) = take_line(&mut pending)
    {
        let _ = sink.send(OutputLine { stream, text });
    }
    String::from_utf8_lossy(&raw).into_owned()
}

/// Hand every complete line in `chunk` to `line`, keeping the tail in `pending`.
///
/// A line ends at a newline **or** a carriage return, because espflash draws
/// its progress bar by returning to the start of the line. Ending only at the
/// newline would show nothing at all until the bar was finished.
///
/// This is the whole of what the streaming does, and it is the same on every
/// platform, which is why it is a function of its own rather than a loop
/// inside a subprocess.
fn split_lines(chunk: &[u8], pending: &mut Vec<u8>, mut line: impl FnMut(String)) {
    for byte in chunk {
        if *byte == b'\n' || *byte == b'\r' {
            if let Some(text) = take_line(pending) {
                line(text);
            }
        } else {
            pending.push(*byte);
        }
    }
}

/// The pending bytes as one line, or `None` when there is nothing to report.
///
/// `\r\n` would otherwise report an empty line after the carriage return, and
/// a blank line in a progress bar is noise rather than information.
fn take_line(pending: &mut Vec<u8>) -> Option<String> {
    let text = String::from_utf8_lossy(pending).trim_end().to_string();
    pending.clear();
    (!text.is_empty()).then_some(text)
}

fn join_streams(primary: &str, secondary: &str) -> String {
    match (primary.trim().is_empty(), secondary.trim().is_empty()) {
        (true, _) => secondary.to_string(),
        (false, true) => primary.to_string(),
        (false, false) => format!("{primary}\n{secondary}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn availability_is_cached() {
        let first = is_available().await;
        let second = is_available().await;
        assert_eq!(first, second);
    }

    /// Collect what `split_lines` reports for a sequence of chunks.
    fn lines_of(chunks: &[&[u8]]) -> (Vec<String>, Vec<u8>) {
        let mut pending = Vec::new();
        let mut lines = Vec::new();
        for chunk in chunks {
            split_lines(chunk, &mut pending, |text| lines.push(text));
        }
        (lines, pending)
    }

    #[test]
    fn a_newline_ends_a_line() {
        let (lines, rest) = lines_of(&[b"one\ntwo\n"]);
        assert_eq!(lines, ["one", "two"]);
        assert!(rest.is_empty());
    }

    #[test]
    fn a_carriage_return_ends_a_line_too() {
        // This is how espflash reports progress.
        let (lines, _) = lines_of(&[b"10%\r55%\r100%\r"]);
        assert_eq!(lines, ["10%", "55%", "100%"]);
    }

    #[test]
    fn a_windows_line_ending_reports_one_line() {
        let (lines, _) = lines_of(&[b"one\r\ntwo\r\n"]);
        assert_eq!(lines, ["one", "two"]);
    }

    #[test]
    fn blank_lines_are_not_reported() {
        let (lines, _) = lines_of(&[b"\n\n\r\n   \n"]);
        assert!(lines.is_empty(), "got: {lines:?}");
    }

    #[test]
    fn a_line_split_across_chunks_arrives_whole() {
        // 4096 bytes at a time means a line is regularly cut in half.
        let (lines, rest) = lines_of(&[b"beg", b"in", b"ning\nnext"]);
        assert_eq!(lines, ["beginning"]);
        assert_eq!(rest, b"next");
    }

    #[test]
    fn an_unterminated_tail_stays_pending() {
        let (lines, mut rest) = lines_of(&[b"done\nhalf"]);
        assert_eq!(lines, ["done"]);
        assert_eq!(take_line(&mut rest), Some("half".to_string()));
    }

    #[test]
    fn invalid_utf8_is_replaced_rather_than_dropped() {
        let (lines, _) = lines_of(&[&[b'a', 0xff, b'b', b'\n']]);
        assert_eq!(lines, ["a\u{fffd}b"]);
    }

    /// A subprocess that writes exactly what the test asks for.
    ///
    /// espflash is not installed everywhere, and the point of these tests is
    /// `run` itself rather than the tool it happens to start. The fixture
    /// needs a POSIX shell, so these tests are Unix-only; what they exercise
    /// beyond the platform-independent tests above is the subprocess
    /// plumbing, not the line splitting.
    #[cfg(unix)]
    fn shell(script: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    #[cfg(unix)]
    fn channel() -> (Progress, tokio::sync::mpsc::UnboundedReceiver<OutputLine>) {
        tokio::sync::mpsc::unbounded_channel()
    }

    #[cfg(unix)]
    fn drained(rx: &mut tokio::sync::mpsc::UnboundedReceiver<OutputLine>) -> Vec<OutputLine> {
        let mut lines = Vec::new();
        while let Ok(line) = rx.try_recv() {
            lines.push(line);
        }
        lines
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn every_line_arrives_and_the_whole_output_still_comes_back() {
        let (tx, mut rx) = channel();
        let out = run(shell("printf 'one\\ntwo\\n'"), Some(&tx))
            .await
            .expect("the script succeeds");
        assert_eq!(out, "one\ntwo\n");
        let lines = drained(&mut rx);
        assert_eq!(
            lines,
            vec![
                OutputLine {
                    stream: Stream::Stdout,
                    text: "one".into()
                },
                OutputLine {
                    stream: Stream::Stdout,
                    text: "two".into()
                },
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_carriage_return_ends_a_line_but_survives_in_the_output() {
        // espflash draws its progress bar this way. Every step has to be
        // visible, and the returned text has to stay what the tool wrote.
        let (tx, mut rx) = channel();
        let out = run(shell("printf '10%%\\r55%%\\rdone\\n'"), Some(&tx))
            .await
            .expect("the script succeeds");
        assert_eq!(out, "10%\r55%\rdone\n");
        let texts: Vec<String> = drained(&mut rx).into_iter().map(|l| l.text).collect();
        assert_eq!(texts, vec!["10%", "55%", "done"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_two_streams_stay_apart() {
        let (tx, mut rx) = channel();
        let message = run(
            shell("printf 'note\\n'; printf 'boom\\n' >&2; exit 3"),
            Some(&tx),
        )
        .await
        .expect_err("the script fails");
        // On failure stderr leads, because that is what explains the failure.
        assert_eq!(message, "boom\n\nnote\n");
        let mut lines = drained(&mut rx);
        lines.sort_by_key(|l| l.text.clone());
        assert_eq!(
            lines,
            vec![
                OutputLine {
                    stream: Stream::Stderr,
                    text: "boom".into()
                },
                OutputLine {
                    stream: Stream::Stdout,
                    text: "note".into()
                },
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_silent_failure_still_names_the_status() {
        let (tx, _rx) = channel();
        let message = run(shell("exit 7"), Some(&tx))
            .await
            .expect_err("the script fails");
        assert!(message.contains("exit status: 7"), "got: {message}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_caller_that_stops_listening_does_not_stop_the_run() {
        let (tx, rx) = channel();
        drop(rx);
        let out = run(shell("printf 'still here\\n'"), Some(&tx))
            .await
            .expect("the script succeeds");
        assert_eq!(out, "still here\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_larger_than_one_read_is_complete() {
        // One read is 4096 bytes. A tool that writes more must not lose any of
        // it, and no line may be cut in half at the boundary.
        let (tx, mut rx) = channel();
        let out = run(
            shell("for i in $(seq 1 900); do echo line$i; done"),
            Some(&tx),
        )
        .await
        .expect("the script succeeds");
        assert!(out.len() > 4096, "the test needs more than one read");
        let lines = drained(&mut rx);
        assert_eq!(lines.len(), 900);
        assert_eq!(lines[0].text, "line1");
        assert_eq!(lines[899].text, "line900");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn without_a_channel_the_output_is_unchanged() {
        let out = run(shell("printf 'quiet\\n'"), None)
            .await
            .expect("the script succeeds");
        assert_eq!(out, "quiet\n");
    }

    #[test]
    fn stream_joining_prefers_the_primary_stream() {
        assert_eq!(join_streams("out", ""), "out");
        assert_eq!(join_streams("", "err"), "err");
        assert_eq!(join_streams("out", "err"), "out\nerr");
    }

    #[tokio::test]
    async fn flash_reports_failure_for_a_missing_device() {
        if !is_available().await {
            return; // espflash is not installed on this machine
        }
        let result = flash(
            "/dev/nonexistent_port_xyz",
            "/nonexistent/firmware.elf",
            None,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn board_info_reports_failure_for_a_missing_device() {
        if !is_available().await {
            return;
        }
        assert!(board_info("/dev/nonexistent_port_xyz").await.is_err());
    }
}
