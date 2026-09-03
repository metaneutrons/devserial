<p align="center">
  <img src="resources/icon-512.png" width="128" height="128" alt="devserial icon" />
</p>

<h1 align="center">devserial</h1>

<p align="center">
  <strong>One binary that turns a serial port into a queryable, always-on log: for AI agents over MCP, for scripts over a CLI, and for humans in a GUI or terminal monitor.</strong>
</p>

<p align="center">
  <a href="https://github.com/metaneutrons/devserial/actions/workflows/ci.yml?query=branch%3Amain"><img src="https://img.shields.io/github/actions/workflow/status/metaneutrons/devserial/ci.yml?branch=main&amp;event=push&amp;label=ci&amp;logo=github" alt="CI status on main" /></a>
  <a href="https://github.com/metaneutrons/devserial/releases/latest"><img src="https://img.shields.io/github/v/release/metaneutrons/devserial?label=release&amp;color=blue&amp;logo=github" alt="Latest release" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0--or--later-blue" alt="License: GPL-3.0-or-later" /></a>
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey" alt="Platforms: macOS, Linux, Windows" />
  <a href="https://modelcontextprotocol.io"><img src="https://img.shields.io/badge/MCP-server-6f4ee8" alt="Model Context Protocol server" /></a>
</p>

---

## What it is

A serial terminal forgets everything the moment you close it, and it can only be read by the person sitting in front of it. `devserial` captures the port into a SQLite database instead, then hands that capture to whoever needs it.

A background daemon owns the open ports and writes every line with a nanosecond timestamp. Four front ends read from the same capture and drive the same hardware.

| Front end | For | Started with |
|-----------|-----|--------------|
| MCP server | AI agents over stdio | `devserial mcp`, or automatically when stdin is a pipe |
| CLI | scripts and shells | `devserial read`, `devserial write`, … |
| GUI monitor | interactive work on the desktop | `devserial`, or `devserial monitor PORT` |
| TUI monitor | interactive work over SSH | `devserial tui PORT` |

All four go through one execution core, so a command behaves identically no matter which one issued it. Unplugging a device does not lose the log, and reconnecting resumes into the same buffer.

---

## Quick start

```bash
# What is connected?
devserial list

# Start capturing. The daemon starts itself.
devserial open /dev/ttyUSB0 --baud 115200

# Watch it live, or read the last 50 lines
devserial read /dev/ttyUSB0 --follow
devserial read /dev/ttyUSB0 --tail 50

# Ask a question of the whole history
devserial search /dev/ttyUSB0 "Guru Meditation"
```

The port stays open and keeps recording after the command returns. Reboot the device, flash it, close your laptop lid: the buffer keeps growing, and it survives a daemon restart.

---

## Installation

### Homebrew (macOS)

```bash
brew install metaneutrons/tap/devserial
```

### Debian and Ubuntu

```bash
sudo curl -fsSL https://deb.metaneutrons.cc/metaneutrons-archive-keyring.pgp \
  -o /usr/share/keyrings/metaneutrons-archive-keyring.pgp
sudo tee /etc/apt/sources.list.d/metaneutrons.sources >/dev/null <<'SOURCES'
Types: deb
URIs: https://deb.metaneutrons.cc/devserial
Suites: rolling
Components: main
Signed-By: /usr/share/keyrings/metaneutrons-archive-keyring.pgp
SOURCES
sudo apt update && sudo apt install devserial
```

The repository is not devserial's own. devserial builds the `.deb`, attests it
and attaches it to its GitHub release; the archive at `deb.metaneutrons.cc`
fetches it from there, verifies the attestation against this repository's
workflow and signs the repository indices. devserial holds neither a signing key
nor write access to the archive, so a compromised release workflow could not
produce a validly signed index.

The packages are built on Debian 12, so they install on Debian 12 and newer and
on Ubuntu 22.04 and newer, for `amd64` and `arm64`. The GUI libraries are
listed as `Recommends`: they are pulled in by default and can be left out with
`--no-install-recommends` on a headless machine, where the CLI, the daemon, the
MCP server and the TUI work without them.

A single `.deb` also hangs on every GitHub release if you prefer not to add a
repository.

### Arch Linux (AUR)

```bash
yay -S devserial-bin   # installs the released binary
yay -S devserial       # builds from the tagged sources
```

Both recipes are generated at release time from one script, the binary one with
checksums measured from the published archives. Both are built, installed and
run in CI before they are pushed.

### Pre-built binaries

Download an archive for your platform from [GitHub Releases](https://github.com/metaneutrons/devserial/releases). Every published binary is built with all features, so no download is missing a subcommand. Each release carries a `SHA256SUMS` file:

```bash
sha256sum --check --ignore-missing SHA256SUMS
```

A checksum only proves the file is intact, not who built it. Every payload also
carries a keyless [Sigstore](https://www.sigstore.dev) bundle and a GitHub build
attestation, both bound to this repository's release workflow and to the exact
tag. Verifying either one tells you the archive came from that workflow and not
from someone who merely recomputed a checksum:

```bash
# Provenance, against GitHub's attestation store
gh attestation verify devserial-<version>-<target>.tar.gz --repo metaneutrons/devserial

# Or the Sigstore bundle, without gh
cosign verify-blob \
  --bundle devserial-<version>-<target>.tar.gz.sigstore.json \
  --certificate-identity-regexp '^https://github.com/metaneutrons/devserial/\.github/workflows/release\.yml@refs/tags/devserial-v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  devserial-<version>-<target>.tar.gz
```

Each payload additionally carries an SPDX software bill of materials as
`<payload>.spdx.json`.

| Platform | Targets |
|----------|---------|
| macOS | `aarch64-apple-darwin`, `x86_64-apple-darwin` |
| Linux glibc | `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu` |
| Linux musl | `aarch64-unknown-linux-musl`, `x86_64-unknown-linux-musl` |
| Windows | `aarch64-pc-windows-msvc`, `x86_64-pc-windows-msvc` |
| Debian package | `devserial_<version>_amd64.deb`, `devserial_<version>_arm64.deb` |

The binaries are not code-signed or notarized. On macOS, install through Homebrew or clear the quarantine flag after downloading:

```bash
xattr -d com.apple.quarantine devserial
```

The musl builds are statically linked. That is what makes them portable, and it also means they cannot load the X11 or Wayland client libraries at run time, so use a glibc build if you want the GUI window on Linux. Everything else works.

### From source

```bash
cargo install --path . --all-features
```

The toolchain is pinned in `rust-toolchain.toml` and rustup installs that version on its own, so there is nothing to choose. There is no supported-MSRV promise: devserial is not published to crates.io, so no consumer needs a documented floor, and the pinned toolchain is the only one it is built and tested against. Building the GUI on Linux needs `libxcb-render0-dev`, `libxcb-shape0-dev`, `libxcb-xfixes0-dev` and `libxkbcommon-dev`.

---

## The GUI monitor

```bash
# Opens the port connection manager
devserial          # from a terminal
devserial gui      # explicitly, for a desktop launcher

# Or go straight to a port
devserial monitor /dev/cu.usbmodem1101 --baud 115200 --parity none --data-bits 8 --stop-bits 1
```

Every device is offered once. On macOS a port exists twice in `/dev`, as a callout node (`cu.`) and a dial-in node (`tty.`); the callout node is the correct one for outgoing use, so it is the one you get.

**Windows and sessions.** All sessions live in one process with one Dock icon. A second `devserial monitor` invocation opens a window in the running instance instead of starting a second application. Ports already shown are marked and cannot be opened twice.

**Live controls.** Reset and Bootloader macros, a BREAK pulse, DTR and RTS toggles, timestamp and hex views, pause and auto-follow, and a filter bar that takes a substring or, written as `/pattern/`, a regular expression.

**Runtime reconfiguration.** Baud rate, framing and flow control can be changed while the session is open, without losing the buffer. The change is recorded as a marker line in the capture.

**Transfers and export.** File transfer over ZMODEM, YMODEM and XMODEM from a dialog, and export of the whole database, the visible buffer or a line range to TXT, CSV or JSONL, either to a file or to the clipboard.

**macOS integration.** Native menu bar, About panel, Dock icon, and the system text editing shortcuts. Menu commands:

| Shortcut | Action |
|----------|--------|
| `Cmd+N` | New window |
| `Cmd+O` | Open port |
| `Cmd+K` | Connect or disconnect |
| `Cmd+Shift+P` | Port settings |
| `Cmd+E` | Export buffer |
| `Cmd+W` | Close window |
| `Cmd+Q` | Quit |

The buffer view is virtualized and keeps the most recent 100 000 lines on screen. The full history stays in the database and is reachable through search and export.

---

## The TUI monitor

For a headless machine or an SSH session:

```bash
devserial tui /dev/ttyUSB0 --baud 115200
```

| Key | Action |
|-----|--------|
| `F1` | About |
| `F2` or `Ctrl+P` | Port settings: baud, data bits, parity, stop bits, flow control |
| `F4` or `Ctrl+B` | Send a BREAK pulse |
| `Ctrl+S` | Send a file over ZMODEM |
| `Ctrl+R` | Receive a file over ZMODEM |
| `Ctrl+T` | Show or hide timestamps |
| `Ctrl+H` | Switch between text and hex dump |
| `↑` `↓` `PgUp` `PgDn` `End` | Scroll, `End` returns to auto-follow |
| `Enter` | Send the typed line with CR LF |
| `Esc` | Leave the current mode |
| `Ctrl+C` or `q` | Exit |

The status bar shows the active view. Timestamps and the hex dump work the same way as in the GUI and produce the same output for the same bytes.

The terminal is restored even if the program is killed or panics.

---

## The CLI

Every command talks to the background daemon and starts it if it is not running. Failures go to stderr and set a non-zero exit code, so commands compose in scripts.

| Command | Purpose |
|---------|---------|
| `list` | Managed ports and detected hardware |
| `open` | Open a port, or reconfigure an open one |
| `close` | Close a managed port |
| `read` | Read from the capture buffer |
| `write` | Send text or hex bytes |
| `break` | Send an RS-232 BREAK pulse |
| `send` / `recv` | File transfer over XMODEM, YMODEM or ZMODEM |
| `signal` | Set DTR and RTS |
| `macro` | Run a built-in or configured macro |
| `search` | Query the capture |
| `export` | Write the capture to a file |
| `clear` | Empty the buffer, optionally archiving it first |
| `stats` | Connection state and buffer statistics |
| `flash` | Flash an ESP device through `espflash` |
| `daemon` | Run, inspect or stop the daemon |
| `monitor` / `tui` | Interactive monitors |
| `mcp` | Run as an MCP server |
| `about` | Version, author, license, repository |

Two options are global. `--socket` selects the IPC endpoint, `--config` a configuration file. Both are validated before anything else happens.

### Ports

```bash
devserial list
devserial list --json

devserial open /dev/ttyUSB0 --baud 115200 --data-bits 8 --parity none --stop-bits 1 --flow-control none
devserial close /dev/ttyUSB0
```

`open` is idempotent. Applying it to an already open port reconfigures that port instead of failing. Invalid values are rejected by the parser, so a typo in `--parity` can never be silently downgraded to no parity.

### Reading

```bash
devserial read /dev/ttyUSB0 --tail 50               # the last 50 lines
devserial read /dev/ttyUSB0 --from 1050 --limit 100 # from a line ID
devserial read /dev/ttyUSB0 --from -20              # negative counts from the end
devserial read /dev/ttyUSB0 --after 1200            # everything after a known ID
devserial read /dev/ttyUSB0 --since 2026-01-15T10:00:00Z
devserial read /dev/ttyUSB0 --follow                # stream, like tail -f
devserial read /dev/ttyUSB0 --tail 20 --timestamps
devserial read /dev/ttyUSB0 --tail 10 --json
```

`--after` is the option to build a polling loop on: pass the highest ID you have seen and you get exactly what is new. Add `--wait-ms 2000` to have the daemon hold the request open until data arrives instead of returning empty.

### Writing, signals and transfers

```bash
devserial write /dev/ttyUSB0 "help" --newline     # appends CR LF
devserial write /dev/ttyUSB0 "0xAABB" --hex       # raw bytes
echo "AT+GMR" | devserial write /dev/ttyUSB0      # from standard input

devserial break /dev/ttyUSB0 --duration-ms 250
devserial signal /dev/ttyUSB0 --dtr true --rts false
devserial macro /dev/ttyUSB0 reset

devserial send /dev/ttyUSB0 firmware.bin --protocol zmodem
devserial send /dev/ttyUSB0 boot.bin --protocol xmodem-1k
devserial recv /dev/ttyUSB0 --output ./downloads --protocol zmodem
```

Built-in macros are `reset`, `enter_bootloader` and `break`. A macro of the same name in the configuration file replaces the built-in one.

### Searching and exporting

```bash
devserial search /dev/ttyUSB0 "Guru Meditation"                  # substring, the default
devserial search /dev/ttyUSB0 "boot ok"          --mode exact    # whole line
devserial search /dev/ttyUSB0 "ERROR.*timeout"   --mode regex
devserial search /dev/ttyUSB0 "reset" --start 2026-01-15T10:00:00Z --end 2026-01-15T12:00:00Z

devserial export /dev/ttyUSB0 dump.txt   --format txt
devserial export /dev/ttyUSB0 dump.csv   --format csv --start 1000 --end 2000
devserial export /dev/ttyUSB0 dump.jsonl --format jsonl

devserial stats /dev/ttyUSB0
devserial clear /dev/ttyUSB0 --archive
```

A filtering search reports when it stopped before the end of the buffer, so an empty result never silently means "did not look far enough".

Export writes one format, whichever front end asks for it. CSV carries the header `line,timestamp,timestamp_ns,payload` and quotes every payload; JSONL emits one object per line with the same four fields.

### The daemon

```bash
devserial daemon --status   # is it running, and on which endpoint
devserial daemon            # run it in the foreground
devserial daemon --stop     # stop it
```

The daemon owns the open ports, the SQLite writers and the IPC endpoint. It restores the ports that were open when it last stopped.

---

## MCP server

`devserial` implements the Model Context Protocol over stdio. Invoked with a piped stdin it runs as an MCP server automatically; `devserial mcp` forces it.

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

The configuration file lives at `~/Library/Application Support/Claude/claude_desktop_config.json` on macOS and `%APPDATA%\Claude\claude_desktop_config.json` on Windows.

| Tool | What it does |
|------|--------------|
| `serial_list` | System serial ports and managed connections |
| `serial_open` | Open a port, or reconfigure an open one |
| `serial_close` | Close a managed port |
| `serial_status` | Connection state, line and byte counts, last activity |
| `serial_read` | Read the capture, with `after_line` for incremental polling and `wait_ms` to avoid empty polls |
| `serial_search` | Substring, exact or regular expression query, with optional time bounds |
| `serial_export` | Write the capture to `txt`, `csv` or `jsonl` |
| `serial_clear` | Empty the buffer, optionally archiving it first |
| `serial_write` | Send UTF-8 text or `0x`-prefixed hex bytes |
| `serial_signal` | Set DTR and RTS |
| `serial_break` | Send a BREAK pulse |
| `serial_macro` | Run a built-in or configured macro |
| `serial_send_file` | Send a file over ZMODEM, YMODEM or XMODEM |
| `serial_receive_file` | Receive a file over the same protocols |
| `serial_monitor_open` | Open a GUI window for a port the model is watching |
| `serial_monitor_close` | Close that window |
| `serial_esp_flash` | Flash firmware through `espflash` |
| `serial_esp_info` | Chip type, flash size, MAC address |
| `serial_esp_erase` | Erase the entire flash |
| `serial_esp_write_bin` | Write a raw binary to a flash address |

Each port is also exposed as an MCP resource at `serial://PORT/status`.

The ESP tools release the port for the duration of the operation and reopen it afterwards, so a flash can be followed by reading the boot log without losing the first lines.

---

## Configuration

The first of these that exists is used:

1. the file given with `--config`
2. the file named by `DEVSERIAL_CONFIG`
3. `./devserial.toml`
4. `~/.config/devserial/config.toml`, on Windows `%APPDATA%\devserial\config.toml`

An explicitly named file is validated straight away, so a typo is reported by the command you ran rather than by a background process. Unknown keys and out-of-range values are errors, never silent fallbacks.

```toml
[global]
# Capture databases and the port state database.
# Default: ~/Library/Application Support/devserial (macOS),
#          $XDG_STATE_HOME/devserial or ~/.local/state/devserial (Linux),
#          %LOCALAPPDATA%\devserial (Windows)
data_dir = "/Users/me/Library/Application Support/devserial"

# Where `clear --archive` and the GUI archive action put their snapshots.
archive_dir = "/Users/me/Library/Application Support/devserial/archive"

# A partially filled batch of captured lines is written after this long.
flush_interval_ms = 100

# This many buffered lines force an immediate write.
flush_batch_size = 1000

# trace, debug, info, warn, error, off. RUST_LOG overrides it.
log_level = "info"

[ports."/dev/ttyUSB0"]
baudrate = 115200
data_bits = 8              # 5, 6, 7 or 8
parity = "none"            # none, odd, even
stop_bits = 1              # 1 or 2
flow_control = "none"      # none, software, hardware
delimiter = 10             # byte that ends a line, 10 is LF
auto_reconnect = true
reconnect_interval_ms = 1000
reconnect_max_backoff_ms = 30000
max_buffer_lines = 500000  # 0 keeps everything

[macros.reset_esp32]
description = "Hardware reset an ESP32 through DTR"
steps = [
    { action = "dtr", value = false },
    { action = "delay", ms = 100 },
    { action = "dtr", value = true },
]
```

A `[ports."…"]` section supplies the defaults for that port. Values passed on the command line or through an MCP call override them.

Macro steps are `write` (UTF-8, or hex with a `0x` prefix), `dtr`, `rts` and `delay`.

Three environment variables override the defaults without a configuration file. They exist so that sandboxes and test runs never touch production state.

| Variable | Overrides |
|----------|-----------|
| `DEVSERIAL_DATA_DIR` | the data directory |
| `DEVSERIAL_SOCKET` | the IPC endpoint |
| `DEVSERIAL_CONFIG` | the configuration file |

---

## Files and endpoints

| What | Where |
|------|-------|
| Capture database, one per port | `<data_dir>/<port with / \ : replaced by _>.db` |
| Open-port state, restored on restart | `<data_dir>/config.db` |
| Archives from `clear --archive` | `<archive_dir>/<port>_<timestamp>.db` |
| IPC endpoint, macOS | `~/Library/Application Support/devserial/devserial.sock` |
| IPC endpoint, Linux | `$XDG_RUNTIME_DIR/devserial/devserial.sock`, otherwise under `~/.local/state` |
| IPC endpoint, Windows | `\\.\pipe\devserial` |

The databases are ordinary SQLite files in WAL mode. Nothing stops you from querying them directly while devserial is running.

The socket directory is created with owner-only permissions, the socket itself is `0600`, and connections from another user are refused. On Windows the named pipe rejects remote clients.

---

## Platform notes

**Linux permissions.** Serial devices belong to a group, `dialout` on Debian and Ubuntu, `uucp` on Arch. Without membership you get "Permission denied" on every port:

```bash
sudo usermod -aG dialout "$USER"   # log out and back in afterwards
```

**macOS device nodes.** Use the `cu.` node. The `tty.` node of the same device blocks on open until carrier detect. `devserial list` and the GUI already offer only the `cu.` node.

**Windows.** Ports are named `COM3` and so on. The daemon uses a named pipe rather than a socket file; `--socket` accepts either a pipe name or an ordinary path, which is then mapped into the pipe namespace.

---

## Troubleshooting

**"Resource busy" or "Device or resource busy".** Something else holds the port. Another `devserial` window, a serial monitor in an IDE, or `screen`. `devserial list` shows what devserial itself has open.

**The daemon does not start.** Run it in the foreground to see why:

```bash
devserial daemon
```

**Too much or too little logging.** `RUST_LOG=debug devserial daemon`, or `log_level` in the configuration file.

**Starting over.** `devserial daemon --stop`, then delete the database for the port from the data directory. `devserial clear PORT` empties a buffer without touching anything else.

---

## Cargo features

| Feature | Default | Contents |
|---------|:-------:|----------|
| `esp` | yes | Flashing, chip info and flash erase through `espflash` |
| `monitor` | no | The desktop GUI (`egui` and `eframe`) |
| `tui` | no | The terminal monitor (`ratatui` and `crossterm`) |
| `testutil` | no | Mock serial ports and generators, for tests |

Released binaries are built with `--all-features`. Building with fewer features removes the corresponding subcommands and MCP tools; the crate compiles and passes its tests with any combination, including `--no-default-features`.

---

## Development

```bash
# Tests
cargo test --all-targets --all-features

# Lints, with pedantic and nursery denied
cargo clippy --all-targets --all-features -- -D warnings

# Formatting
cargo fmt --all -- --check

# Documentation, warnings denied
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps

# Every shipped feature combination has to compile
cargo check --no-default-features
cargo check --no-default-features --features esp,tui
```

Continuous integration runs lints, tests and documentation on Linux, macOS and Windows across four feature sets, plus a job that checks the declared minimum supported Rust version.

Architecture in one paragraph: `engine.rs` is the only place operations are carried out. `protocol.rs` holds the vocabulary of requests and responses. The CLI, the IPC daemon and the MCP server are transports over that vocabulary and contain no operation logic, which is what keeps their behaviour identical. `port_manager.rs` owns the open ports, `storage.rs` the SQLite layer, `transport.rs` the platform-specific IPC.

---

## License

GPL-3.0-or-later. Copyright (C) 2026 Fabian Schmieder. See [LICENSE](LICENSE) for the
text of version 3.

Releases 0.1.1 through 0.1.4 were distributed under GPL-3.0-only. That does not
change retroactively: whoever received one of those versions keeps those terms
for it. The `or later` option applies from the next release onwards.

The GUI embeds five typefaces under OFL-1.1, UFL-1.0 and MIT. Their notices and
licence texts travel with every binary, in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) and [licenses/](licenses).
