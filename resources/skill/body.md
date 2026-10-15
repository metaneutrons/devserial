# devserial

devserial holds serial ports for everyone who needs them. A background daemon
opens each port once, records every line it receives into a SQLite capture
with a timestamp, and serves that capture to the CLI, this MCP server, a GUI, a
terminal monitor and an HTTP interface at the same time. Closing a reader does
not stop the capture.

## When to use it

- Anything that reads from or writes to a serial device: a microcontroller's
  console, a bootloader, a modem, an ESP32.
- Use it instead of `screen`, `minicom`, `picocom`, `cat /dev/tty*` or a
  pyserial script. A port can be held by one program only. Those tools would
  take the port away from devserial and from the monitor the user may have
  open, and they forget what they read.
- When the user mentions a board's log, a crash on the serial console, or
  flashing an ESP chip.

## How to use it

Use the MCP tools when the devserial MCP server is connected; their names start
with `serial_`. Otherwise use the CLI. Every command starts the daemon when it
is not running.

1. `devserial list` shows the ports devserial manages and the serial hardware
   attached.
2. `devserial open PORT --baud 115200` starts the capture. It keeps recording
   after the command returns. Opening a port that is already open is harmless.
3. Read what the device said: `devserial read PORT --tail 50`, or search the
   whole history with `devserial search PORT "pattern"` (`--mode regex` for a
   regular expression). Add `--json` when you parse the output.
4. Send: `devserial write PORT "text" --newline`, or `--hex` for bytes. Control
   lines with `devserial signal PORT --dtr false`, a reset with
   `devserial macro PORT reset`.
5. After sending, wait for the answer instead of sleeping:
   `devserial read PORT --after ID --wait-ms 2000`, where `ID` is the last line
   you have seen.

The capture outlives restarts of the device and of devserial. Narrow a read to
what is new with `--after` or `--since`, so old output is not mistaken for a
fresh answer.

## Ask the user first

- Flashing or erasing a chip (`devserial flash`, the `serial_esp_*` tools)
  replaces its firmware.
- `devserial clear PORT` deletes the captured history; `--archive` keeps a copy.
- `devserial delete PORT --above ID` or `--below ID` deletes the lines on one side of a line;
  a copy is kept unless `--no-archive` is given, and `--dry-run` only counts. Only when asked.
- `devserial close PORT` releases the port, and a monitor the user has open
  stops receiving.
- Do not stop the daemon (`devserial daemon --stop`). It stops by itself once
  idle, and other clients may be using it.

If a port cannot be opened because another program holds it, tell the user
which port it is rather than ending that program.

## Reference

Generated from the devserial build that installed this skill:

- [references/cli.md](references/cli.md): every command and option
- [references/mcp.md](references/mcp.md): every MCP tool
- [references/http.md](references/http.md): the HTTP routes, when the build has
  the HTTP interface
