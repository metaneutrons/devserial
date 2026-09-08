# Initiative plan: REST interface

Epic: pending, created after this plan is reviewed
Decision state: accepted. The design was reviewed and every open question
answered on 8 and 9 September 2026; the decisions are recorded under
[Design and decisions](#design-and-decisions).

## Outcome and boundaries

A caller reaches every operation devserial can carry out over HTTP on
`127.0.0.1:9600`, with the same behaviour the CLI and the MCP server already
give, and switches the server on from the command line, the terminal monitor or
the window without editing a file.

Measured baseline, devserial 0.1.14:

| | Now |
|---|---|
| Transports over the engine | 3: CLI over IPC, the IPC daemon itself, MCP over stdio |
| HTTP server | none. No `axum`, `hyper`, `warp` or `actix` in the manifest or the lockfile |
| Operations in the vocabulary | 23 `RequestPayload` variants, 19 `ResponsePayload` variants |
| Exposed over MCP | 20 tools |
| Dependencies | 490 crates in `Cargo.lock` |
| Port ownership | two paths: the daemon holds ports opened by the CLI and MCP, a standalone monitor holds its own |

In scope: the HTTP transport and its 22 routes, the control surface in four
places, the daemon-side switch and its failure reporting, and the two changes
the decisions pull in with them (every monitor attaching to the daemon, and the
export format). Out of scope: TLS, authentication beyond a bearer token for
non-loopback binds, remote access as a supported configuration, a client
library, and any web UI.

## Design and decisions

The full design, with the routes, the failure states, the wire format and the
reasoning behind each decision, is the reviewed proposal at
<https://claude.ai/code/artifact/eac531b3-f631-4340-a36e-573049bf86e5>. This
section records what was decided and why, so the plan stands on its own where
it constrains delivery.

**The server lives in the daemon.** `engine.rs` is where an operation is carried
out, and a `CommandEngine` exists only in the daemon, the MCP server and the
in-process path the CLI starts. REST is a transport over that engine and
contains no operation logic, which is what keeps the transports behaving
identically. A second server hosted by a standalone monitor was rejected: it
would double the listener and give two answers to `GET /v1/ports`.

**Every port becomes a daemon port.** A serial port can be opened once, so a
port a standalone window held could not also be served. `devserial monitor PORT`
and `devserial tui PORT` therefore ensure the daemon, have it open the line and
attach to the capture, the way `serial_monitor_open` already does for the MCP
server. Considered and rejected: disabling the switch in standalone windows, and
serving only daemon ports while explaining the gap. Both leave two kinds of port
for the life of the tool. The cost is accepted: `devserial monitor` starts a
daemon where it previously started none, and its first launch is slower by
however long that takes.

**Port 9600, bound to `127.0.0.1`.** The number is the one a serial developer
recalls without looking it up. `/etc/services` assigns 9600 to
`micromuse-ncpw`; the collision risk is known and accepted, and the failure it
can produce is a first-class case rather than a crash. 11520 is the documented
fallback.

**No token on loopback.** A request to `127.0.0.1` is served as it stands; a
bind to any other address requires a token that was set deliberately and refuses
to start without one. This is a step down from the existing IPC endpoint, a Unix
socket at mode `0600` that refuses another user, and that is accepted. Two
mitigations become mandatory in consequence, because a browser can reach
`127.0.0.1` from any page the user has open: every state-changing route requires
`Content-Type: application/json`, which a form POST cannot set and a
cross-origin `fetch` cannot send without a preflight the server refuses; and the
`Host` header must be `127.0.0.1:9600` or `localhost:9600`, which defeats DNS
rebinding.

**Server-sent events, not WebSocket.** Following the capture and watching a
flash are both one-directional, and SSE is a plain `GET` that resumes from
`Last-Event-ID` mapped onto the line id, works from `curl`, and adds no second
protocol. Writes are their own `POST`, so the client never needs the return
channel.

**One timestamp field at full precision.** `timestamp` in RFC 3339 with nine
fractional digits, `2026-09-07T21:26:54.123456789Z`, readable and exact at once.
`timestamp_ns` accompanies it as a **string**: measured, a nanosecond epoch in
2026 is 1788816414123456789, which is 198.6 times above JavaScript's
`Number.MAX_SAFE_INTEGER`, where a double's step is 256 ns, so a JSON number is
silently rounded. That is irrelevant against a UART bit at 8.7 µs but breaks
handing the value back as a filter.

**The line id is `id` everywhere.** The Rust type calls it `id`, the `jsonl`
export calls it `line`, and the cursor is `next_after_id`. One spelling for all
transports, which renames the export's field.

**`RestServer` joins the capability register.** `surface.rs` names every
capability once and a test fails when the window and the terminal differ. Its
definition widens from what a person does with an open port to include controls
a surface offers over the daemon; the ledger of accepted gaps stays empty.

**Dependency cost, measured** by adding `axum` 0.8 and `tower-http` 0.7 to the
manifest and running `cargo metadata`: 19 new crates, 490 to 509. All MIT and
already on the `cargo-deny` allowlist. Nothing adds a TLS stack, because there
is no TLS. `axum` sits on the tokio runtime the daemon already runs, so the
listener is a task beside the socket listener and not a second reactor.

Unresolved: whether M1 and M2 ship in a release of their own ahead of the REST
work. Both change behaviour a user can observe, and shipping them separately
keeps the export break out of a feature's release notes. Decide before M3 opens.

## Delivery and acceptance

Six milestones in order; each depends on the one before it. Stable IDs name
requirements and do not track progress.

### M1: every monitor attaches to the daemon

Execution: pending
Dependencies: none

- M1-A1: `devserial monitor PORT` and `devserial tui PORT` open the port through
  the daemon, starting it if it is not running, and display the capture without
  holding the line. Verified by an integration test that opens a port through
  each entry point and asserts the daemon reports it in `ListPorts`.
- M1-A2: A monitor that exits leaves the port open on the daemon and the capture
  growing. Verified by a test that starts a monitor, ends it, and reads new lines
  afterwards.
- M1-A3: The parity test in `surface.rs` still passes, with no new entry in
  `KNOWN_GAPS`.
- M1-A4: The second action path in `standalone.rs` is either removed or reduced
  to what still has a caller, and the module header no longer claims that no
  daemon is involved where that is no longer true.
- M1-A5: The README paragraph on who holds the port, and the architecture
  paragraph that describes `standalone.rs` mirroring `engine.rs`, describe the
  single path.
- M1-A6: Clippy, tests and `cargo doc` pass on the four feature sets the CI
  matrix covers.

### M2: one export format at full precision

Execution: pending
Dependencies: M1 is not required; M2 may run in parallel or first.

- M2-A1: `export::format_timestamp` renders nine fractional digits. Verified by a
  unit test on a timestamp whose sub-millisecond digits are non-zero.
- M2-A2: The `csv` header and the `jsonl` object name the line id `id`, and
  `jsonl` writes `timestamp_ns` as a string. Verified by tests on the rendered
  output of both formats.
- M2-A3: Every place that consumed the old field name or the millisecond form is
  updated, established by a search for the old names rather than by inspection.
- M2-A4: The change is stated as a breaking change to the export format in the
  pull request subject, so it reaches the release notes as one.

### M3: the server and its switch

Execution: pending
Dependencies: M1, M2

- M3-A1: `[rest]` in the configuration file with `enabled`, `bind`, `port` and an
  optional `token`, defaulting to disabled on `127.0.0.1:9600`. Verified by
  configuration parsing tests including the absent-section default.
- M3-A2: `RestStatus`, `RestEnable { bind, port }` and `RestDisable` in
  `protocol.rs`, carried out in `engine.rs`, with the bind performed inside the
  request so the response reports the outcome.
- M3-A3: `AddrInUse`, `PermissionDenied` and `AddrNotAvailable` are reported as
  distinct, readable reasons naming the port or address. Verified by tests that
  bind a listener first, and that request a privileged port and an address no
  interface holds.
- M3-A4: A failed autostart from the configuration file leaves the daemon
  running and the reason retrievable. Verified by a test that occupies the
  configured port before the daemon starts.
- M3-A5: `devserial rest`, `--enable`, `--disable`, `--port`, `--bind`,
  `--token-file` and `--rotate-token` behave as documented, with the state
  readable in the exit code and on stdout.
- M3-A6: F7 in the terminal monitor and a window in the GUI show the state, the
  bind address, the port and the failure reason, and switch the server on and
  off. Both read the daemon's state rather than a local copy.
- M3-A7: `RestServer` is in `surface.rs` with an anchor in each surface, the
  register's definition is widened in its module documentation, and `KNOWN_GAPS`
  stays empty.
- M3-A8: `/v1/health` and `/v1/version` answer. No other route exists yet.
- M3-A9: The `rest` feature is out of `default` and in `full`, and the existing
  tests in `tests/features.rs` that `full` covers every product feature and never
  carries the test scaffolding both still pass.

### M4: reading over HTTP

Execution: pending
Dependencies: M3

- M4-A1: `GET /v1/ports`, `PUT` and `DELETE /v1/ports/{port}`,
  `GET /v1/ports/{port}`, `/stats`, `/lines`, `/search`, `POST /export` and
  `DELETE /lines` answer, with the port percent-encoded in the path.
- M4-A2: Every field of `ReadWindow` is reachable as a query parameter, and
  `since`, `from` and `to` accept RFC 3339, parsed by the same code path the MCP
  server uses.
- M4-A3: A line on the wire carries `id`, `timestamp` at nine digits,
  `timestamp_ns` as a string and `payload`, rendered through `export.rs` rather
  than a second renderer. Verified by a test comparing a response body against
  the `jsonl` export of the same lines.
- M4-A4: A test walks every `RequestPayload` variant and fails when one has
  neither a route nor a recorded reason for not having one. `Shutdown` is a
  recorded exception. Counter-probed: removing a route fails the test.
- M4-A5: Errors are `application/problem+json` with a stable `type`.
- M4-A6: A request without `Content-Type: application/json` on a state-changing
  route is refused, and a request whose `Host` is neither `127.0.0.1:9600` nor
  `localhost:9600` is refused. Verified by tests for both.

### M5: streaming

Execution: pending
Dependencies: M4

- M5-A1: `GET /v1/ports/{port}/lines/stream` emits one event per line with the
  line id as the event id, and resumes from `Last-Event-ID`. Verified by a test
  that disconnects and reconnects across a write.
- M5-A2: A client that disconnects does not leave the daemon holding work.
  Verified by a test asserting the task ends.
- M5-A3: `POST /v1/ports/{port}/esp/flash` answers `202` with a job id and
  streams the tool's output line by line on the same channel, using the reporting
  `esp.rs` already produces.

### M6: driving the device, and the specification

Execution: pending
Dependencies: M5

- M6-A1: `write`, `break`, `signals`, `macros/{name}` and `transfers` answer and
  carry out the same operation as the equivalent CLI command. Verified against a
  loopback or mock port.
- M6-A2: The four ESP routes answer behind the `esp` feature, and are absent
  without it.
- M6-A3: `GET /v1/openapi.json` is generated from the route mapping, not
  hand-written, and a test fails when a route is missing from it.
- M6-A4: The README documents the interface, the default port, the token rule
  and the two header requirements.

Initiative completion: M1 to M6 accepted, the README current, and a release
carrying the interface published. Publication is in scope because the export
break in M2 has to reach users through release notes.

## Migration, risks and verification cost

**Adoption is staged by the milestones.** M1 and M2 change observable behaviour
without REST existing, which is why they come first and can ship on their own.
M3 leaves the server disabled by default, so a user who never enables it sees no
change. M4 to M6 add routes to a server that is already switched off.

**Rollback.** M3 to M6 are removable by disabling the `rest` feature, and the
default keeps the listener off, so a fault reaches only someone who opted in. M1
has no feature gate and is the one milestone whose rollback is a revert; that is
the reason for its own acceptance criteria on the monitor entry points. M2
changes an output format, and rolling it back would break anyone who adapted in
the meantime, so it is announced rather than reversible.

**Risks.** Two are worth naming. Port 9600 is registered, so a collision is
possible; M3-A3 and M3-A4 are what make it survivable rather than a defect. And
the loopback decision leaves any local process able to drive the hardware, which
is accepted; M4-A6 is what keeps a web page in the user's own browser from
becoming one of those processes, and it is a required criterion rather than a
nicety.

**Verification cost.** Focused tests during iteration on the native target. The
four-feature-set matrix across Linux, macOS and Windows runs per pull request
as it does today, which is the eleven checks the repository already pays for; no
new qualification gate is introduced. The 19 added crates are a measured figure,
not a forecast; compile-time impact was not measured and is not claimed.

## Decision changes

None yet. A change to a criterion or to the scope is recorded here with its
date and the pull request that made it, and a milestone claiming acceptance
links the exact revision of this file it was accepted against.
