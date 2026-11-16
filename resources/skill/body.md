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

## ESP chips

`devserial flash PORT IMAGE` (`serial_esp_flash`) takes an ELF image only. A raw
or merged binary is written to a flash address with `serial_esp_write_bin`; an
image merged from bootloader, partition table and application starts at
address 0. `serial_esp_info` and `serial_esp_erase` reset the chip afterwards,
so it runs its application again.

`flash` writes the bootloader, the partition table and the application, and by
default they are the ones built into `espflash`, not the ones of the board's
project. A board with a layout of its own needs it named, or the flash replaces
its partition table and its bootloader:

- `--partition-table FILE`, `--bootloader FILE`, `--partition-table-offset
  ADDRESS` and `--target-app-partition NAME` (`ota_1`, say), or the fields
  `partition_table`, `bootloader`, `partition_table_offset` and
  `target_app_partition` in MCP and HTTP.
- `--flash-size`, `--flash-mode` and `--flash-frequency`, or `flash_size`,
  `flash_mode` and `flash_frequency`, for what is written into the image
  headers. Left out, the size is what the chip reports.

The output of an ESP operation starts with `Reset before connect:` and names
the reset it chose from the USB identity; `--before` (`reset` in MCP and HTTP)
changes it. A chip that does not answer is put into download mode by hand,
usually by holding BOOT and pressing RESET, and then reached with `no-reset`.

`devserial read-flash PORT FILE` (`serial_esp_read_flash`) reads the flash into
a new file: all of it, or a range with `--address` and `--length` (`address` and
`length` in MCP and HTTP, decimal or `0x` hexadecimal). Use it for the backup
before a flash or an erase. Before a flash replaces firmware that someone wants
to keep, ask what is on the board, and read the whole range that will be
written, right before, because a board that is also updated by other means can
have changed since an earlier backup. The file is written by the daemon, so
give an absolute path in a folder that exists; the CLI makes a relative path
absolute. An existing file is never overwritten, so a second backup needs a
new name, and a read that fails leaves no file. Each block is checked against
the chip's checksum. The chip is not changed, but it restarts afterwards, like
after `info`. Over a UART bridge the default 115200 baud takes close to an hour
for 32 MiB; pass a higher `--baud` (`baud`) there. After the read, say what was
backed up and where the file is, and keep it until the flash is known to work.

## Ask the user first

- Flashing or erasing a chip (`devserial flash`, `serial_esp_flash`,
  `serial_esp_write_bin`, `serial_esp_erase`) replaces its firmware.
- Reading a flash (`devserial read-flash`, `serial_esp_read_flash`) changes
  nothing on the chip, but the chip restarts and its application is interrupted,
  so ask first when it is running something that matters.
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
