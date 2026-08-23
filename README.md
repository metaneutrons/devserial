# devserial

[![CI](https://github.com/metaneutrons/devserial-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/metaneutrons/devserial-mcp/actions/workflows/ci.yml)
[![Release](https://github.com/metaneutrons/devserial-mcp/actions/workflows/release.yml/badge.svg)](https://github.com/metaneutrons/devserial-mcp/actions/workflows/release.yml)
[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)

Universal serial hardware bridge, background daemon, and CLI engine for LLMs, developers, and autonomous agents with SQLite-backed persistent line buffering.

---

## Key Highlights

- **Unified Single Binary** — Use `devserial` as an MCP server, a persistent background daemon, a fast CLI tool for terminal/shell scripting, or a live GUI/TUI monitor.
- **Persistent SQLite Buffering** — All serial streams are ingested and indexed in WAL-mode SQLite databases with nanosecond timestamps. Reconnects and restarts never lose history.
- **Background Daemon & Transparent Auto-Spawn** — CLI commands (`devserial read`, `devserial write`, etc.) talk to the daemon over native Unix Domain Sockets (< 100µs latency). If the daemon is not running, CLI commands automatically launch it in the background.
- **Non-blocking Flashing & Live Monitoring** — Flash firmware with `espflash` while holding open buffers, seamlessly reopening the serial port and capturing post-flash boot logs.
- **Fast Grep & Regex Search** — Query gigabyte-sized serial history instantly by substring, regex, or time range.
- **Hardware Control & Macros** — Full control over DTR/RTS pin states, reset routines, and custom macro sequences.

---

## Installation

```bash
cargo install --path . --all-features
```

Or download pre-built binaries from [Releases](https://github.com/metaneutrons/devserial-mcp/releases) (macOS, Linux, Windows).

---

## Usage

### 1. CLI Commands (LLM-friendly & Scriptable)

All CLI commands communicate with the background daemon and will **automatically start the daemon** if it is not already running.

#### Port Management
```bash
# List managed ports and detected hardware ports
devserial list
devserial list --json

# Open a serial port
devserial open /dev/ttyUSB0 --baud 115200 --parity none --data-bits 8 --stop-bits 1

# Close a port
devserial close /dev/ttyUSB0
```

#### Reading & Streaming Data
```bash
# Read the last 50 lines
devserial read /dev/ttyUSB0 --tail 50

# Read lines with timestamps
devserial read /dev/ttyUSB0 --tail 20 --timestamps

# Read lines incrementally from a specific line ID
devserial read /dev/ttyUSB0 --from 1050 --limit 100

# Live stream newly arriving lines (like `tail -f`)
devserial read /dev/ttyUSB0 --follow

# Output lines as JSON
devserial read /dev/ttyUSB0 --tail 10 --json
```

#### Writing Data, Break Signals & File Transfers
```bash
# Send ASCII string with automatic \r\n newline
devserial write /dev/ttyUSB0 "help" --newline

# Send raw hex bytes (e.g. 0xAA 0xBB)
devserial write /dev/ttyUSB0 "0xAABB" --hex

# Pipe data from stdin
echo "AT+GMR" | devserial write /dev/ttyUSB0

# Send RS-232 serial BREAK pulse (TX low)
devserial break /dev/ttyUSB0 --duration-ms 250

# Send file via ZMODEM, YMODEM, or XMODEM
devserial send /dev/ttyUSB0 firmware.bin --protocol zmodem
devserial send /dev/ttyUSB0 update.bin --protocol ymodem
devserial send /dev/ttyUSB0 boot.bin --protocol xmodem-1k

# Receive file via ZMODEM, YMODEM, or XMODEM
devserial recv /dev/ttyUSB0 ./downloads --protocol zmodem

# Toggle DTR/RTS hardware control lines
devserial signal /dev/ttyUSB0 --dtr true --rts false

# Execute predefined or configured macro sequence
devserial macro /dev/ttyUSB0 reset
```

#### Search, Export & Stats
```bash
# Search buffer with substring or regular expression
devserial search /dev/ttyUSB0 "Guru Meditation"
devserial search /dev/ttyUSB0 "ERROR.*timeout" --regex

# Export buffered lines to a file (txt, csv, or jsonl)
devserial export /dev/ttyUSB0 dump.jsonl --format jsonl
devserial export /dev/ttyUSB0 dump.csv --format csv --start 1000 --end 2000

# View buffer and port statistics
devserial stats /dev/ttyUSB0

# Clear buffer (with optional timestamped SQLite archive)
devserial clear /dev/ttyUSB0 --archive
```

#### Flashing Firmware (ESP Devices)
```bash
# Flash firmware and seamlessly switch to live boot log monitoring
devserial flash /dev/ttyUSB0 build/app.bin --baud 921600 --monitor
```

---

### 2. Background Daemon Service

Manage the daemon directly:

```bash
# Check daemon status
devserial daemon --status

# Start daemon in foreground
devserial daemon

# Stop a running daemon
devserial daemon --stop
```

---

### 3. MCP Server Mode

When invoked without subcommands in a non-interactive pipe, or via `devserial mcp`, `devserial` runs as a Model Context Protocol (MCP) server over standard input/output (stdio).

#### Claude Desktop Setup
Add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "serial": {
      "command": "devserial",
      "args": ["mcp"]
    }
  }
}
```

#### MCP Tools Reference

| Tool | Description |
|------|-------------|
| `serial_open` | Open a serial port with custom baud, parity, data bits, stop bits |
| `serial_close` | Close an active serial port |
| `serial_read` | Read buffered lines with pagination, tailing, and incremental `after_line` |
| `serial_write` | Send UTF-8 text or hex byte strings to the port |
| `serial_break` | Send an RS-232 serial BREAK pulse (TX low duration) |
| `serial_send_file` | Send file over serial via ZMODEM, YMODEM, or XMODEM |
| `serial_receive_file` | Receive file over serial via ZMODEM, YMODEM, or XMODEM |
| `serial_search` | Search buffered history via regex, substring, and time bounds |
| `serial_signal` | Set DTR and RTS pin states |
| `serial_macro` | Execute automated macro sequences (e.g. `reset`, `enter_bootloader`) |
| `serial_status` | Query connection state, line counts, bytes, and timestamps |
| `serial_list` | List open managed ports and available system hardware |
| `serial_export` | Export buffer to `txt`, `csv`, or `jsonl` |
| `serial_clear` | Clear port buffer with optional archive database creation |
| `serial_esp_flash` | Flash firmware ELF/BIN to ESP devices |
| `serial_esp_info` | Query connected ESP chip model, MAC address, and features |
| `serial_esp_erase` | Erase entire ESP flash memory |
| `serial_monitor_open` | Open native GUI monitor window |
| `serial_monitor_close` | Close active monitor window |

---

### 4. Interactive Monitors (GUI / TUI)

- **GUI Monitor (`egui`)**: Standalone window with virtualized scrolling, search filter, signal toggles, `Break` button, and `Transfer ▾` dialog.
- **TUI Monitor (`ratatui`)**: Terminal dashboard with real-time streaming, status bar, and hotkeys (`F4`/`Ctrl+B` for Break, `Ctrl+S` for Send File, `Ctrl+R` for Receive File).

```bash
# Standalone native GUI monitor (egui)
devserial monitor /dev/ttyUSB0 --baud 115200

# Terminal UI monitor (ratatui)
devserial tui /dev/ttyUSB0 --baud 115200
```

---

## Configuration

`devserial` automatically looks for `./devserial.toml` or `~/.config/devserial/config.toml`:

```toml
[global]
data_dir = "./data"
archive_dir = "./data/archive"
flush_interval_ms = 100
flush_batch_size = 1000
log_level = "info"

[ports."/dev/ttyUSB0"]
baudrate = 115200
data_bits = 8
parity = "none"
stop_bits = 1
flow_control = "none"
auto_reconnect = true
max_buffer_lines = 500000

[macros.reset_esp32]
description = "Hardware reset ESP32 via DTR toggle"
steps = [
    { action = "dtr", value = false },
    { action = "delay", ms = 100 },
    { action = "dtr", value = true },
]
```

---

## Cargo Features

| Feature | Default | Description |
|---------|:-------:|-------------|
| `esp` | ✅ | ESP flash, info, and erase tooling via `espflash` |
| `monitor` | ❌ | Native GUI desktop window (`egui` / `eframe`) |
| `tui` | ❌ | Terminal dashboard (`ratatui` / `crossterm`) |

---

## Development & Verification

Requires Rust 1.85+ (Edition 2024).

```bash
# Run unit & integration tests
cargo test --all-targets --all-features

# Run strict enterprise clippy lints
cargo clippy --all-targets --all-features -- -D warnings

# Format check
cargo fmt --all -- --check
```

---

## License

GPL-3.0-only — see [LICENSE](LICENSE).
