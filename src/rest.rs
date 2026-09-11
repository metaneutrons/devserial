// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! The HTTP interface, and the switch that turns it on.
//!
//! A fourth transport over [`crate::engine::CommandEngine`], beside the CLI,
//! the IPC daemon and the MCP server, and like them it carries no operation
//! logic of its own. Every request variant the daemon knows either has a route
//! here or a recorded reason for not having one, and a test at the bottom of
//! this file fails when neither is true.
//!
//! Three things here are decisions rather than mechanics, and each is written
//! down where it is made: the bind happens inside the request that asked for
//! it, so the answer says what happened; a request to loopback needs no token
//! while a bind to anything else refuses to start without one; and flashing,
//! the one operation too slow to answer inside a request, is handed to a job
//! whose output is a stream.

// A handler's error is an `axum::Response`, which is 128 bytes, and clippy
// calls that a large `Err`. Boxing it would mean every handler wrapping and
// unwrapping to satisfy a size lint, and the type is the one axum's own
// `IntoResponse` is built around. Allowed here rather than worked around.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use crate::config::RestConfig;
use crate::protocol::RestState;

/// Every route the interface serves, as method and path.
///
/// The list is the interface's own account of itself: `/v1/version` hands it
/// out, and the ratchet at the bottom of this file checks it against the
/// request vocabulary, so a request variant with no route and no recorded
/// reason fails the build rather than going unnoticed.
pub const ROUTES: &[&str] = &[
    "GET /v1/health",
    "GET /v1/version",
    "GET /v1/ports",
    "PUT /v1/ports/{port}",
    "DELETE /v1/ports/{port}",
    "GET /v1/ports/{port}",
    "GET /v1/ports/{port}/stats",
    "GET /v1/ports/{port}/lines",
    "GET /v1/ports/{port}/lines/stream",
    "DELETE /v1/ports/{port}/lines",
    "GET /v1/ports/{port}/search",
    "POST /v1/ports/{port}/export",
    "POST /v1/ports/{port}/write",
    "POST /v1/ports/{port}/break",
    "POST /v1/ports/{port}/signals",
    "POST /v1/ports/{port}/macros/{name}",
    "POST /v1/ports/{port}/transfers",
    #[cfg(feature = "esp")]
    "GET /v1/ports/{port}/esp",
    #[cfg(feature = "esp")]
    "POST /v1/ports/{port}/esp/flash",
    #[cfg(feature = "esp")]
    "POST /v1/ports/{port}/esp/erase",
    #[cfg(feature = "esp")]
    "POST /v1/ports/{port}/esp/write-bin",
    #[cfg(feature = "esp")]
    "GET /v1/jobs/{job}/stream",
    "GET /v1/openapi.json",
];

/// How long a stream parks on the engine when nothing is arriving.
///
/// The engine answers as soon as a line lands, so this is the cost of an idle
/// port rather than a delay on a busy one.
const STREAM_WAIT_MS: u64 = 5_000;

/// Why a listener could not be bound, in words a person can act on.
///
/// The three kinds are separated because the reader does something different
/// about each. Everything else is reported as it came, rather than being
/// flattened into one message that says nothing.
#[must_use]
pub fn bind_failure(addr: std::net::SocketAddr, error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::AddrInUse => format!(
            "port {} is in use. Another program is listening there.",
            addr.port()
        ),
        std::io::ErrorKind::PermissionDenied => format!(
            "port {} needs privileges this process does not have. Use a port above 1024.",
            addr.port()
        ),
        std::io::ErrorKind::AddrNotAvailable => format!(
            "no interface on this machine has the address {}.",
            addr.ip()
        ),
        _ => format!("could not bind {addr}: {error}"),
    }
}

/// A listener that is accepting connections, and the way to end it.
struct Running {
    shutdown: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

/// The HTTP interface of one daemon.
///
/// Holds at most one listener. Asking for a second while one runs replaces it,
/// because the alternative is two answers to the same question depending on
/// which port a caller reached.
pub struct RestServer {
    state: RestState,
    token: Option<String>,
    running: Option<Running>,
}

/// Whether a caller has to present a bearer token.
///
/// A non-loopback bind always needs one, and a token configured for loopback
/// is honoured rather than ignored: the middleware enforces whatever token the
/// router was built with, so a state that said otherwise would be a lie about
/// the running listener.
fn token_required(config: &RestConfig) -> bool {
    config.token.is_some() || !config.binds_loopback()
}

impl RestServer {
    /// A server that is not listening, described by the configuration.
    #[must_use]
    pub fn new(config: &RestConfig) -> Self {
        Self {
            state: RestState {
                listening: false,
                bind: config.bind.clone(),
                port: config.port,
                reason: None,
                token_required: token_required(config),
            },
            token: config.token.clone(),
            running: None,
        }
    }

    /// What the daemon reports about the interface.
    #[must_use]
    pub fn state(&self) -> RestState {
        self.state.clone()
    }

    /// Start listening, reporting the outcome of the bind.
    ///
    /// The bind is synchronous and happens here rather than inside a spawned
    /// task, which is the whole point: a caller that asked for a port learns
    /// in the same answer whether it got it. Starting in the background and
    /// hoping would report success for a listener that never came up.
    ///
    /// # Errors
    /// Returns a readable reason if the address is not an address, if a
    /// non-loopback bind carries no token, or if the bind itself fails.
    pub fn enable(
        &mut self,
        config: &RestConfig,
        engine: &crate::engine::CommandEngine,
    ) -> Result<RestState, String> {
        let addr = config.socket_addr()?;

        // The bind address is the whole security boundary once loopback needs
        // no token, so a bind that leaves the machine has to be deliberate.
        if !config.binds_loopback() && config.token.is_none() {
            return Err(format!(
                "binding {} exposes the device to the network, so it needs a token. \
                 Set one with --token-file, or bind 127.0.0.1.",
                config.bind
            ));
        }

        let listener = std::net::TcpListener::bind(addr).map_err(|e| {
            let reason = bind_failure(addr, &e);
            self.state = RestState {
                listening: false,
                bind: config.bind.clone(),
                port: config.port,
                reason: Some(reason.clone()),
                token_required: token_required(config),
            };
            reason
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("could not prepare the listener: {e}"))?;

        // The port the operating system actually gave out, not the one that was
        // asked for. They differ whenever 0 was asked for, and a state that
        // reported 0 would send every caller to a port nothing listens on.
        let bound = listener
            .local_addr()
            .map_err(|e| format!("could not read the bound address: {e}"))?;

        // A listener already running is replaced rather than left beside the
        // new one.
        self.stop_running();

        let listener = tokio::net::TcpListener::from_std(listener)
            .map_err(|e| format!("could not hand the listener to the runtime: {e}"))?;

        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        let token = config.token.clone();
        // The router holds a clone of the engine, and the engine holds this
        // server, so a listening daemon carries a reference cycle. It is
        // deliberate and bounded: stopping the listener aborts the task and
        // drops the router with it, and a daemon that never stops ends with
        // the process.
        let router = router(&Guards {
            token: token.clone(),
            port: bound.port(),
            engine: engine.clone(),
        });
        let task = tokio::spawn(async move {
            let served = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    drop(shutdown_rx.await);
                })
                .await;
            if let Err(e) = served {
                tracing::warn!(error = %e, "the HTTP interface stopped");
            }
        });

        self.token = token;
        self.running = Some(Running { shutdown, task });
        self.state = RestState {
            listening: true,
            bind: config.bind.clone(),
            port: bound.port(),
            reason: None,
            token_required: token_required(config),
        };
        Ok(self.state.clone())
    }

    /// Stop listening. A server that is not listening is left as it is.
    pub fn disable(&mut self) -> RestState {
        self.stop_running();
        self.state.listening = false;
        self.state.reason = None;
        self.state.clone()
    }

    fn stop_running(&mut self) {
        if let Some(running) = self.running.take() {
            // The receiver ending is what the graceful shutdown waits for, so
            // dropping the sender is enough; the send is the polite form and
            // its result says only whether anyone was still listening.
            let _ = running.shutdown.send(());
            running.task.abort();
        }
    }
}

impl Drop for RestServer {
    fn drop(&mut self) {
        self.stop_running();
    }
}

/// What every request is checked against, and what carries it out.
#[derive(Clone)]
struct Guards {
    token: Option<String>,
    port: u16,
    engine: crate::engine::CommandEngine,
}

/// What a handler reaches: the engine, and the jobs running on it.
///
/// Two things rather than one because the flash route hands its work to a job
/// and the job stream reads it back. Everything else takes the engine alone,
/// which is why this is a substate rather than a new extractor everywhere.
#[derive(Clone)]
struct Api {
    engine: crate::engine::CommandEngine,
    port: u16,
    #[cfg(feature = "esp")]
    jobs: Jobs,
}

impl Api {
    fn new(engine: crate::engine::CommandEngine, port: u16) -> Self {
        Self {
            engine,
            port,
            #[cfg(feature = "esp")]
            jobs: Jobs::default(),
        }
    }
}

impl axum::extract::FromRef<Api> for crate::engine::CommandEngine {
    fn from_ref(api: &Api) -> Self {
        api.engine.clone()
    }
}

#[cfg(feature = "esp")]
impl axum::extract::FromRef<Api> for Jobs {
    fn from_ref(api: &Api) -> Self {
        Self::clone(&api.jobs)
    }
}

/// The router the interface serves.
///
/// `/v1/health` sits outside the guards: a caller has to be able to ask
/// whether anything is there before being told it is not allowed in.
/// Everything else passes the host check and, when one is configured, the
/// token check.
fn router(guards: &Guards) -> axum::Router {
    use axum::routing::{get, post};

    let open = axum::Router::new().route("/v1/health", get(health));

    let guarded = axum::Router::new()
        .route("/v1/version", get(version))
        .route("/v1/ports", get(list_ports))
        .route(
            "/v1/ports/{port}",
            get(port_status).put(open_port).delete(close_port),
        )
        .route("/v1/ports/{port}/stats", get(port_stats))
        .route(
            "/v1/ports/{port}/lines",
            get(read_lines).delete(clear_lines),
        )
        .route("/v1/ports/{port}/lines/stream", get(stream_lines))
        .route("/v1/ports/{port}/search", get(search))
        .route("/v1/ports/{port}/export", post(export))
        .route("/v1/ports/{port}/write", post(write_data))
        .route("/v1/ports/{port}/break", post(send_break))
        .route("/v1/ports/{port}/signals", post(set_signals))
        .route("/v1/ports/{port}/macros/{name}", post(run_macro))
        .route("/v1/ports/{port}/transfers", post(transfer))
        .route("/v1/openapi.json", get(openapi));

    #[cfg(feature = "esp")]
    let guarded = guarded
        .route("/v1/ports/{port}/esp", get(esp_info))
        .route("/v1/ports/{port}/esp/flash", post(esp_flash))
        .route("/v1/ports/{port}/esp/erase", post(esp_erase))
        .route("/v1/ports/{port}/esp/write-bin", post(esp_write_bin))
        .route("/v1/jobs/{job}/stream", get(job_stream));

    let guarded = guarded
        .with_state(Api::new(guards.engine.clone(), guards.port))
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(guards.clone()),
            require_token,
        ));

    open.merge(guarded)
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(guards.clone()),
            require_local_host,
        ))
        .layer(axum::middleware::from_fn(require_json_body))
}

/// Liveness, and the one route that never needs a token.
///
/// A caller has to be able to ask whether anything is there before it can be
/// told that it is not allowed in.
async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "status": "ok" }))
}

/// What this build is and what it can do.
async fn version() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "name": "devserial",
        "version": env!("CARGO_PKG_VERSION"),
        "api": "v1",
        "features": features(),
        "routes": ROUTES,
    }))
}

/// Features compiled into this build, so a caller need not guess.
fn features() -> Vec<&'static str> {
    let mut features = vec!["rest"];
    if cfg!(feature = "esp") {
        features.push("esp");
    }
    if cfg!(feature = "monitor") {
        features.push("monitor");
    }
    if cfg!(feature = "tui") {
        features.push("tui");
    }
    features
}

// ------------------------------------------------------------------ routes
//
// Every handler does the same three things: turn the request into a
// `RequestPayload`, hand it to the engine, and turn the answer into JSON. None
// of them carries operation logic, which is what keeps the four transports
// behaving identically.

type Engine = axum::extract::State<crate::engine::CommandEngine>;
type Reply = Result<axum::Json<serde_json::Value>, axum::response::Response>;

/// Run one request through the engine, or turn its failure into a problem.
async fn run(
    engine: &crate::engine::CommandEngine,
    payload: crate::protocol::RequestPayload,
) -> Result<crate::protocol::ResponsePayload, axum::response::Response> {
    engine.execute(payload).await.map_err(|e| {
        // The engine's own error text is what the CLI prints, so a caller here
        // reads the same sentence rather than a second wording of it.
        problem(
            axum::http::StatusCode::BAD_REQUEST,
            "operation-failed",
            &e.to_string(),
        )
    })
}

/// The answer shape a handler did not expect, which is a fault in this file.
fn unexpected() -> axum::response::Response {
    problem(
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "unexpected-answer",
        "the engine answered with something this route does not serve",
    )
}

/// Managed ports, and optionally what the operating system reports.
async fn list_ports(
    axum::extract::State(engine): Engine,
    axum::extract::Query(query): axum::extract::Query<PortsQuery>,
) -> Reply {
    use crate::protocol::{RequestPayload, ResponsePayload};

    let ResponsePayload::PortList(managed) = run(&engine, RequestPayload::ListPorts).await? else {
        return Err(unexpected());
    };

    let mut body = serde_json::json!({ "ports": managed });
    if query.hardware.unwrap_or(false) {
        let ResponsePayload::HardwarePorts(hardware) =
            run(&engine, RequestPayload::ListHardware).await?
        else {
            return Err(unexpected());
        };
        body["hardware"] = serde_json::json!(hardware);
    }
    Ok(axum::Json(body))
}

/// Open a port, or reconfigure one that is already open.
///
/// `PUT` because it is idempotent in the way the CLI's `open` already is: the
/// same body twice leaves the same port in the same state.
async fn open_port(
    axum::extract::State(engine): Engine,
    axum::extract::Path(port): axum::extract::Path<String>,
    axum::Json(settings): axum::Json<crate::protocol::PortSettings>,
) -> Reply {
    use crate::protocol::{RequestPayload, ResponsePayload};

    let ResponsePayload::PortOpened {
        name,
        config_summary,
    } = run(
        &engine,
        RequestPayload::OpenPort {
            name: port,
            settings,
        },
    )
    .await?
    else {
        return Err(unexpected());
    };
    Ok(axum::Json(
        serde_json::json!({ "port": name, "settings": config_summary }),
    ))
}

/// Close a managed port.
async fn close_port(
    axum::extract::State(engine): Engine,
    axum::extract::Path(port): axum::extract::Path<String>,
) -> Reply {
    use crate::protocol::{RequestPayload, ResponsePayload};

    let ResponsePayload::PortClosed { name } =
        run(&engine, RequestPayload::ClosePort { name: port }).await?
    else {
        return Err(unexpected());
    };
    Ok(axum::Json(serde_json::json!({ "port": name })))
}

/// Link state and buffer statistics of one port.
async fn port_status(
    axum::extract::State(engine): Engine,
    axum::extract::Path(port): axum::extract::Path<String>,
) -> Reply {
    use crate::protocol::{RequestPayload, ResponsePayload};

    let ResponsePayload::Status { name, state, stats } =
        run(&engine, RequestPayload::GetStatus { port }).await?
    else {
        return Err(unexpected());
    };
    Ok(axum::Json(serde_json::json!({
        "port": name,
        "state": state,
        "stats": stats,
    })))
}

/// Buffer statistics of one port.
async fn port_stats(
    axum::extract::State(engine): Engine,
    axum::extract::Path(port): axum::extract::Path<String>,
) -> Reply {
    use crate::protocol::{RequestPayload, ResponsePayload};

    let ResponsePayload::Stats(stats) = run(&engine, RequestPayload::GetStats { port }).await?
    else {
        return Err(unexpected());
    };
    Ok(axum::Json(serde_json::to_value(stats).unwrap_or_default()))
}

/// A page of captured lines.
async fn read_lines(
    axum::extract::State(engine): Engine,
    axum::extract::Path(port): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<LinesQuery>,
) -> Reply {
    use crate::protocol::{RequestPayload, ResponsePayload};

    let window = query
        .window()
        .map_err(|e| problem(axum::http::StatusCode::BAD_REQUEST, "invalid-parameter", &e))?;

    let ResponsePayload::Lines(page) =
        run(&engine, RequestPayload::ReadLines { port, window }).await?
    else {
        return Err(unexpected());
    };

    Ok(axum::Json(serde_json::json!({
        "total_lines": page.total_lines,
        "next_after_id": page.next_after_id,
        "lines": page
            .lines
            .iter()
            .map(crate::export::line_object)
            .collect::<Vec<_>>(),
    })))
}

/// Discard the buffer, optionally archiving it first.
async fn clear_lines(
    axum::extract::State(engine): Engine,
    axum::extract::Path(port): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<ClearQuery>,
) -> Reply {
    use crate::protocol::{RequestPayload, ResponsePayload};

    let ResponsePayload::ClearSuccess {
        lines_cleared,
        archive_path,
    } = run(
        &engine,
        RequestPayload::Clear {
            port,
            archive_current: query.archive,
        },
    )
    .await?
    else {
        return Err(unexpected());
    };
    Ok(axum::Json(serde_json::json!({
        "lines_cleared": lines_cleared,
        "archive": archive_path,
    })))
}

/// Search the capture.
async fn search(
    axum::extract::State(engine): Engine,
    axum::extract::Path(port): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<SearchQuery>,
) -> Reply {
    use crate::protocol::ResponsePayload;

    let payload = query
        .payload(port)
        .map_err(|e| problem(axum::http::StatusCode::BAD_REQUEST, "invalid-parameter", &e))?;

    let ResponsePayload::SearchResults(outcome) = run(&engine, payload).await? else {
        return Err(unexpected());
    };
    Ok(axum::Json(serde_json::json!({
        "truncated": outcome.truncated,
        "results": outcome
            .results
            .iter()
            .map(crate::export::line_object)
            .collect::<Vec<_>>(),
    })))
}

/// Write the capture to a file the daemon can reach.
async fn export(
    axum::extract::State(engine): Engine,
    axum::extract::Path(port): axum::extract::Path<String>,
    axum::Json(body): axum::Json<ExportBody>,
) -> Reply {
    use crate::protocol::{RequestPayload, ResponsePayload};

    let ResponsePayload::ExportSuccess {
        lines_exported,
        path,
        file_format,
    } = run(
        &engine,
        RequestPayload::Export {
            port,
            output_path: body.path,
            file_format: body.format,
            start_line: body.start_line,
            end_line: body.end_line,
        },
    )
    .await?
    else {
        return Err(unexpected());
    };
    Ok(axum::Json(serde_json::json!({
        "lines_exported": lines_exported,
        "path": path,
        "format": file_format,
    })))
}

/// The capture as it grows, one event per line.
///
/// Server-sent events rather than a WebSocket: the channel only ever runs one
/// way, a plain `GET` reconnects on its own, and `Last-Event-ID` resumes from
/// the line the client last saw. Writing is its own request, so the return
/// channel a WebSocket would add has nothing to carry.
///
/// The loop asks the engine with `wait_ms`, so an idle port costs one parked
/// request rather than a spin. When the client goes away the response future is
/// dropped, the loop ends with it, and the engine is left holding nothing.
async fn stream_lines(
    axum::extract::State(engine): Engine,
    axum::extract::Path(port): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<LinesQuery>,
    headers: axum::http::HeaderMap,
) -> Result<
    axum::response::Sse<
        impl futures_core::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    axum::response::Response,
> {
    // `Last-Event-ID` is the resumption point a reconnecting browser sends by
    // itself. An explicit `after` wins, so a caller that is not a browser can
    // say where to start.
    let resume = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok());

    let window = query
        .window()
        .map_err(|e| problem(axum::http::StatusCode::BAD_REQUEST, "invalid-parameter", &e))?;
    let mut after = window.after_id.or(resume).unwrap_or(0);
    let wait_ms = window.wait_ms.or(Some(STREAM_WAIT_MS));

    let stream = async_stream::stream! {
        loop {
            let payload = crate::protocol::RequestPayload::ReadLines {
                port: port.clone(),
                window: crate::protocol::ReadWindow {
                    after_id: Some(after),
                    wait_ms,
                    ..crate::protocol::ReadWindow::default()
                },
            };
            match engine.execute(payload).await {
                Ok(crate::protocol::ResponsePayload::Lines(page)) => {
                    for line in &page.lines {
                        after = line.id;
                        yield Ok(axum::response::sse::Event::default()
                            .id(line.id.to_string())
                            .event("line")
                            .data(crate::export::line_object(line).to_string()));
                    }
                }
                // A port that went away ends the stream rather than looping on
                // an error the client cannot act on.
                Ok(_) | Err(_) => break,
            }
        }
    };

    Ok(axum::response::Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}

// -------------------------------------------------------- driving the device
//
// The five routes that change what the device sees. Each is the operation the
// CLI already carries out, reached through the engine rather than repeated
// here, so the two transports cannot drift apart.

/// Write bytes to the port.
async fn write_data(
    axum::extract::State(engine): Engine,
    axum::extract::Path(port): axum::extract::Path<String>,
    axum::Json(body): axum::Json<WriteBody>,
) -> Reply {
    use crate::protocol::{RequestPayload, ResponsePayload};

    let ResponsePayload::WriteSuccess { bytes_written } = run(
        &engine,
        RequestPayload::WriteData {
            port,
            data: body.data,
            is_hex: body.hex,
        },
    )
    .await?
    else {
        return Err(unexpected());
    };
    Ok(axum::Json(
        serde_json::json!({ "bytes_written": bytes_written }),
    ))
}

/// Hold the line in the break condition.
async fn send_break(
    axum::extract::State(engine): Engine,
    axum::extract::Path(port): axum::extract::Path<String>,
    axum::Json(body): axum::Json<BreakBody>,
) -> Reply {
    use crate::protocol::{RequestPayload, ResponsePayload};

    let ResponsePayload::BreakSuccess { duration_ms } = run(
        &engine,
        RequestPayload::SendBreak {
            port,
            duration_ms: body.duration_ms,
        },
    )
    .await?
    else {
        return Err(unexpected());
    };
    Ok(axum::Json(
        serde_json::json!({ "duration_ms": duration_ms }),
    ))
}

/// Set DTR, RTS or both.
async fn set_signals(
    axum::extract::State(engine): Engine,
    axum::extract::Path(port): axum::extract::Path<String>,
    axum::Json(body): axum::Json<SignalsBody>,
) -> Reply {
    use crate::protocol::{RequestPayload, ResponsePayload};

    let ResponsePayload::SignalSuccess { applied } = run(
        &engine,
        RequestPayload::SetSignal {
            port,
            dtr: body.dtr,
            rts: body.rts,
        },
    )
    .await?
    else {
        return Err(unexpected());
    };
    Ok(axum::Json(serde_json::json!({ "applied": applied })))
}

/// Run a macro from the configuration.
///
/// The macro is named in the path rather than in a body, because it is the
/// thing being run and not a parameter of it.
async fn run_macro(
    axum::extract::State(engine): Engine,
    axum::extract::Path((port, name)): axum::extract::Path<(String, String)>,
) -> Reply {
    use crate::protocol::{RequestPayload, ResponsePayload};

    let ResponsePayload::MacroSuccess { executed_steps } = run(
        &engine,
        RequestPayload::ExecuteMacro {
            port,
            macro_name: name,
        },
    )
    .await?
    else {
        return Err(unexpected());
    };
    Ok(axum::Json(
        serde_json::json!({ "executed": executed_steps }),
    ))
}

/// Send or receive a file with a modem protocol.
///
/// One route for both directions: the transfer is the same operation with the
/// file going the other way, and a caller that has to pick a direction anyway
/// is better served by one name than by two.
async fn transfer(
    axum::extract::State(engine): Engine,
    axum::extract::Path(port): axum::extract::Path<String>,
    axum::Json(body): axum::Json<TransferBody>,
) -> Reply {
    use crate::protocol::{RequestPayload, ResponsePayload};

    let payload = match body {
        TransferBody::Send { path, protocol } => RequestPayload::SendFile {
            port,
            file_path: path,
            protocol,
        },
        TransferBody::Receive {
            directory,
            protocol,
        } => RequestPayload::ReceiveFile {
            port,
            output_dir: directory,
            protocol,
        },
    };

    let ResponsePayload::TransferSuccess {
        bytes_transferred,
        file_name,
        protocol,
        path,
    } = run(&engine, payload).await?
    else {
        return Err(unexpected());
    };
    Ok(axum::Json(serde_json::json!({
        "bytes_transferred": bytes_transferred,
        "file_name": file_name,
        "protocol": protocol,
        "path": path,
    })))
}

// ------------------------------------------------------------------ the esp
//
// Present only with the `esp` feature, and absent from the route list without
// it, so a caller reading `/v1/version` learns what this build can do rather
// than discovering it at a 404.

/// What the attached board says about itself.
#[cfg(feature = "esp")]
async fn esp_info(
    axum::extract::State(engine): Engine,
    axum::extract::Path(port): axum::extract::Path<String>,
) -> Reply {
    esp_answer(run(&engine, crate::protocol::RequestPayload::EspInfo { port }).await?)
}

/// Erase the flash.
#[cfg(feature = "esp")]
async fn esp_erase(
    axum::extract::State(engine): Engine,
    axum::extract::Path(port): axum::extract::Path<String>,
) -> Reply {
    esp_answer(run(&engine, crate::protocol::RequestPayload::EspErase { port }).await?)
}

/// Write a raw binary to an address in flash.
#[cfg(feature = "esp")]
async fn esp_write_bin(
    axum::extract::State(engine): Engine,
    axum::extract::Path(port): axum::extract::Path<String>,
    axum::Json(body): axum::Json<EspWriteBinBody>,
) -> Reply {
    esp_answer(
        run(
            &engine,
            crate::protocol::RequestPayload::EspWriteBin {
                port,
                file_path: body.path,
                address: body.address,
            },
        )
        .await?,
    )
}

/// The tool's output, which is the same answer for all three of these.
#[cfg(feature = "esp")]
fn esp_answer(answer: crate::protocol::ResponsePayload) -> Reply {
    let crate::protocol::ResponsePayload::EspSuccess(output) = answer else {
        return Err(unexpected());
    };
    Ok(axum::Json(serde_json::json!({ "output": output })))
}

/// Start a flash, and answer with the job that is carrying it out.
///
/// `202` rather than `200`: flashing takes tens of seconds, and a request that
/// waited for it would be indistinguishable from one that hung. The answer
/// names the stream where the tool's output arrives line by line and where the
/// outcome is reported, so nothing has to be polled for.
#[cfg(feature = "esp")]
async fn esp_flash(
    axum::extract::State(engine): Engine,
    axum::extract::State(jobs): axum::extract::State<Jobs>,
    axum::extract::Path(port): axum::extract::Path<String>,
    axum::Json(body): axum::Json<EspFlashBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;

    let id = job_id();
    let job = Arc::new(Job::new());
    remember(&jobs, id.clone(), Arc::clone(&job));

    tokio::spawn(async move {
        let (progress, mut lines) =
            tokio::sync::mpsc::unbounded_channel::<crate::esp::OutputLine>();
        let collecting = Arc::clone(&job);
        let pump = tokio::spawn(async move {
            while let Some(line) = lines.recv().await {
                collecting.push(line.text);
            }
        });

        let outcome = engine
            .esp_flash_reporting(port, body.firmware, body.baud, progress)
            .await;

        // The pump ends when the call above drops its sender, so waiting for
        // it here is what puts every line in the log before the outcome does.
        drop(pump.await);
        job.finish(match outcome {
            Ok(crate::protocol::ResponsePayload::EspSuccess(output)) => Ok(output),
            Ok(_) => Err("the engine answered with something this route does not serve".to_owned()),
            Err(e) => Err(e.to_string()),
        });
    });

    (
        axum::http::StatusCode::ACCEPTED,
        axum::Json(serde_json::json!({
            "job": id,
            "stream": format!("/v1/jobs/{id}/stream"),
        })),
    )
        .into_response()
}

/// The output of a job, one event per line, ending with its outcome.
///
/// The mechanism the capture stream already uses: an id on every event, and
/// `Last-Event-ID` to resume from it. A client that reconnects after the job
/// finished still gets everything, because the log is kept.
#[cfg(feature = "esp")]
async fn job_stream(
    axum::extract::State(jobs): axum::extract::State<Jobs>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<JobQuery>,
    headers: axum::http::HeaderMap,
) -> Result<
    axum::response::Sse<
        impl futures_core::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    axum::response::Response,
> {
    let Some(job) = find(&jobs, &id) else {
        return Err(problem(
            axum::http::StatusCode::NOT_FOUND,
            "no-such-job",
            &format!("no job '{id}' is known"),
        ));
    };

    let resume = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    // An event id is the line's position counted from one, so the id last seen
    // is also the number of lines already delivered.
    let mut sent = query.after.or(resume).unwrap_or(0);

    let stream = async_stream::stream! {
        // Subscribed before the first read, so a line written between the read
        // and the wait bumps a revision this reader has not seen and it comes
        // straight back rather than waiting for the next one.
        let mut revisions = job.revision.subscribe();
        loop {
            let (lines, outcome) = job.since(sent);
            for text in lines {
                sent += 1;
                yield Ok(axum::response::sse::Event::default()
                    .id(sent.to_string())
                    .event("line")
                    .data(serde_json::json!({ "text": text }).to_string()));
            }
            if let Some(outcome) = outcome {
                let done = match outcome {
                    Ok(output) => serde_json::json!({ "ok": true, "output": output }),
                    Err(error) => serde_json::json!({ "ok": false, "error": error }),
                };
                yield Ok(axum::response::sse::Event::default()
                    .event("done")
                    .data(done.to_string()));
                break;
            }
            if revisions.changed().await.is_err() {
                break;
            }
        }
    };

    Ok(axum::response::Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}

// ------------------------------------------------------------------- jobs
//
// A job is one run of an external tool. It exists because flashing is the one
// operation too slow to answer inside a request, and it is deliberately not a
// general task system: there is one producer, the log is the whole state, and
// the registry dies with the listener.

/// How many finished jobs the registry keeps.
///
/// A finished job stays so a client that reconnects can still read the
/// outcome. Without a cap, a daemon that flashes all day would hold every run
/// it ever made.
#[cfg(feature = "esp")]
const JOBS_KEPT: usize = 16;

/// What a job has printed, and how it ended.
#[cfg(feature = "esp")]
#[derive(Default)]
struct JobLog {
    lines: Vec<String>,
    outcome: Option<Result<String, String>>,
}

/// One run of an external tool.
#[cfg(feature = "esp")]
struct Job {
    log: std::sync::Mutex<JobLog>,
    revision: tokio::sync::watch::Sender<u64>,
}

#[cfg(feature = "esp")]
impl Job {
    fn new() -> Self {
        Self {
            log: std::sync::Mutex::new(JobLog::default()),
            revision: tokio::sync::watch::channel(0).0,
        }
    }

    fn push(&self, text: String) {
        self.change(|log| log.lines.push(text));
    }

    fn finish(&self, outcome: Result<String, String>) {
        self.change(|log| log.outcome = Some(outcome));
    }

    fn change(&self, write: impl FnOnce(&mut JobLog)) {
        write(
            &mut self
                .log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        self.revision.send_modify(|revision| *revision += 1);
    }

    /// Everything past `sent`, and the outcome when there is one.
    ///
    /// Both come from one lock on purpose. The outcome is set after the last
    /// line, so a snapshot carrying it carries every line before it, and a
    /// reader can end the stream without wondering what it missed.
    fn since(&self, sent: usize) -> (Vec<String>, Option<Result<String, String>>) {
        let log = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            log.lines.get(sent..).unwrap_or_default().to_vec(),
            log.outcome.clone(),
        )
    }

    fn is_finished(&self) -> bool {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .outcome
            .is_some()
    }
}

/// The jobs this listener knows, oldest first.
#[cfg(feature = "esp")]
type Jobs = Arc<std::sync::Mutex<std::collections::VecDeque<(String, Arc<Job>)>>>;

/// Put a job in the registry, dropping old finished ones.
#[cfg(feature = "esp")]
fn remember(jobs: &Jobs, id: String, job: Arc<Job>) {
    let mut known = jobs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    known.push_back((id, job));
    // A running job is somebody's stream, so only finished ones are dropped.
    while known.len() > JOBS_KEPT && known.front().is_some_and(|(_, job)| job.is_finished()) {
        known.pop_front();
    }
    drop(known);
}

/// The job with this id, if the registry still has it.
#[cfg(feature = "esp")]
fn find(jobs: &Jobs, id: &str) -> Option<Arc<Job>> {
    jobs.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(known, _)| known == id)
        .map(|(_, job)| Arc::clone(job))
}

/// An id no other job in this process has had.
///
/// The clock and a counter: the counter keeps two jobs started in the same
/// nanosecond apart, and the clock keeps a job apart from one that had the
/// same counter before the listener was restarted.
#[cfg(feature = "esp")]
fn job_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let count = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let clock = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_nanos()).unwrap_or(0));
    format!("{clock:x}{count:x}")
}

// ---------------------------------------------------------- the description
//
// `/v1/openapi.json` is built from `ROUTES` and from the very structs the
// handlers deserialize. Nothing about a route is described in two places: the
// path and method come from the list, the parameters and bodies from the
// types, and the only sentence written by hand is the summary.

/// The interface as `OpenAPI`, for a reader or a generator.
fn openapi_document(port: u16) -> serde_json::Value {
    let mut paths = serde_json::Map::new();
    for route in ROUTES {
        let Some((method, path)) = route.split_once(' ') else {
            continue;
        };
        let (query, body) = shapes(route);
        let mut operation = serde_json::json!({
            "summary": describe(route),
            "operationId": operation_id(method, path),
            "parameters": parameters(path, query.as_ref()),
        });
        if let Some(body) = body {
            operation["requestBody"] = serde_json::json!({
                "required": true,
                "content": { "application/json": { "schema": body } },
            });
        }
        operation["responses"] = responses(route);

        paths
            .entry(path.to_owned())
            .or_insert_with(|| serde_json::json!({}))[method.to_lowercase()] = operation;
    }

    serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "devserial",
            "version": env!("CARGO_PKG_VERSION"),
            "summary": "The serial daemon over HTTP.",
            "description": "Every request carries Host: localhost:PORT or the \
                            loopback address on the bound port, and every body \
                            declares Content-Type: application/json. A bearer \
                            token is required unless the listener is on \
                            loopback without one configured.",
            "license": { "name": "GPL-3.0-or-later" },
        },
        "servers": [{ "url": format!("http://127.0.0.1:{port}") }],
        "components": {
            "securitySchemes": {
                "token": { "type": "http", "scheme": "bearer" },
            },
        },
        "paths": paths,
    })
}

/// The interface, described for a generator.
async fn openapi(
    axum::extract::State(api): axum::extract::State<Api>,
) -> axum::Json<serde_json::Value> {
    axum::Json(openapi_document(api.port))
}

/// What one route is for, in one sentence.
///
/// The only hand-written part of the description, and the test below fails
/// when a route has no sentence here.
fn describe(route: &str) -> &'static str {
    match route {
        "GET /v1/health" => "Whether the interface is answering.",
        "GET /v1/version" => "What this build is, and every route it serves.",
        "GET /v1/ports" => "The ports the daemon holds, and what the operating system reports.",
        "PUT /v1/ports/{port}" => "Open a port, or change the settings of one already open.",
        "DELETE /v1/ports/{port}" => "Close a port the daemon holds.",
        "GET /v1/ports/{port}" => "Link state and buffer statistics of one port.",
        "GET /v1/ports/{port}/stats" => "Buffer statistics of one port.",
        "GET /v1/ports/{port}/lines" => "A page of captured lines.",
        "GET /v1/ports/{port}/lines/stream" => "The capture as it grows, one event per line.",
        "DELETE /v1/ports/{port}/lines" => "Discard the buffer, optionally archiving it first.",
        "GET /v1/ports/{port}/search" => "Search the capture.",
        "POST /v1/ports/{port}/export" => "Write the capture to a file the daemon can reach.",
        "POST /v1/ports/{port}/write" => "Write bytes to the port, as text or as hex.",
        "POST /v1/ports/{port}/break" => "Hold the line in the break condition.",
        "POST /v1/ports/{port}/signals" => "Set DTR, RTS or both.",
        "POST /v1/ports/{port}/macros/{name}" => "Run a macro from the configuration.",
        "POST /v1/ports/{port}/transfers" => "Send or receive a file with a modem protocol.",
        "GET /v1/openapi.json" => "This description.",
        #[cfg(feature = "esp")]
        "GET /v1/ports/{port}/esp" => "What the attached board says about itself.",
        #[cfg(feature = "esp")]
        "POST /v1/ports/{port}/esp/flash" => "Start flashing firmware, and name the job doing it.",
        #[cfg(feature = "esp")]
        "POST /v1/ports/{port}/esp/erase" => "Erase the flash of the attached board.",
        #[cfg(feature = "esp")]
        "POST /v1/ports/{port}/esp/write-bin" => "Write a raw binary to an address in flash.",
        #[cfg(feature = "esp")]
        "GET /v1/jobs/{job}/stream" => "The output of a job, one event per line.",
        _ => "",
    }
}

/// The query parameters and the request body of one route, as schemas.
///
/// Derived from the structs the handlers deserialize, so a field that is
/// renamed or dropped changes the description with it.
fn shapes(route: &str) -> (Option<serde_json::Value>, Option<serde_json::Value>) {
    let query = |schema: serde_json::Value| (Some(schema), None);
    let body = |schema: serde_json::Value| (None, Some(schema));
    match route {
        "GET /v1/ports" => query(schema_of::<PortsQuery>()),
        "GET /v1/ports/{port}/lines" | "GET /v1/ports/{port}/lines/stream" => {
            query(schema_of::<LinesQuery>())
        }
        "DELETE /v1/ports/{port}/lines" => query(schema_of::<ClearQuery>()),
        "GET /v1/ports/{port}/search" => query(schema_of::<SearchQuery>()),
        "PUT /v1/ports/{port}" => body(schema_of::<crate::protocol::PortSettings>()),
        "POST /v1/ports/{port}/export" => body(schema_of::<ExportBody>()),
        "POST /v1/ports/{port}/write" => body(schema_of::<WriteBody>()),
        "POST /v1/ports/{port}/break" => body(schema_of::<BreakBody>()),
        "POST /v1/ports/{port}/signals" => body(schema_of::<SignalsBody>()),
        "POST /v1/ports/{port}/transfers" => body(schema_of::<TransferBody>()),
        #[cfg(feature = "esp")]
        "POST /v1/ports/{port}/esp/flash" => body(schema_of::<EspFlashBody>()),
        #[cfg(feature = "esp")]
        "POST /v1/ports/{port}/esp/write-bin" => body(schema_of::<EspWriteBinBody>()),
        #[cfg(feature = "esp")]
        "GET /v1/jobs/{job}/stream" => query(schema_of::<JobQuery>()),
        _ => (None, None),
    }
}

/// One type's JSON schema, as a value this document can carry.
fn schema_of<T: schemars::JsonSchema>() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_default()
}

/// The parameters of one route: the path placeholders, then the query.
fn parameters(path: &str, query: Option<&serde_json::Value>) -> Vec<serde_json::Value> {
    let mut parameters: Vec<serde_json::Value> = path
        .split('/')
        .filter_map(|segment| segment.strip_prefix('{')?.strip_suffix('}'))
        .map(|name| {
            serde_json::json!({
                "name": name,
                "in": "path",
                "required": true,
                "schema": { "type": "string" },
            })
        })
        .collect();

    let properties = query
        .and_then(|schema| schema.get("properties"))
        .and_then(serde_json::Value::as_object);
    for (name, schema) in properties.into_iter().flatten() {
        parameters.push(serde_json::json!({
            "name": name,
            "in": "query",
            "required": false,
            "schema": schema,
        }));
    }
    parameters
}

/// What a route answers with.
///
/// One success and one catch-all, because every failure this interface
/// produces is a problem document and listing them per route would say the
/// same thing twenty times.
fn responses(route: &str) -> serde_json::Value {
    let (code, description) = if route.ends_with("/esp/flash") {
        ("202", "the job was started")
    } else if route.ends_with("/stream") {
        ("200", "an event stream")
    } else {
        ("200", "the request was carried out")
    };
    serde_json::json!({
        code: { "description": description },
        "default": {
            "description": "a problem document (RFC 9457)",
            "content": { "application/problem+json": {} },
        },
    })
}

/// A name for one route, built from its method and path.
fn operation_id(method: &str, path: &str) -> String {
    let mut id = method.to_lowercase();
    // The leading empty segment and the version carry nothing that tells two
    // routes apart.
    for segment in path.split('/').skip(2) {
        let cleaned: String = segment
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect();
        for word in cleaned.split('_').filter(|word| !word.is_empty()) {
            id.push('_');
            id.push_str(word);
        }
    }
    id
}

// ------------------------------------------------------------- parameters

/// `GET /v1/ports`
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PortsQuery {
    hardware: Option<bool>,
}

/// `DELETE /v1/ports/{port}/lines`
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ClearQuery {
    archive: Option<bool>,
}

/// `POST /v1/ports/{port}/export`
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ExportBody {
    path: String,
    format: Option<crate::export::ExportFormat>,
    start_line: Option<i64>,
    end_line: Option<i64>,
}

/// `POST /v1/ports/{port}/write`
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct WriteBody {
    /// What to write. A `0x` prefix is read as hex without `hex` being set.
    data: String,
    /// Read `data` as hex even when it carries no prefix.
    #[serde(default)]
    hex: bool,
}

/// `POST /v1/ports/{port}/break`
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BreakBody {
    /// How long to hold the line, in milliseconds.
    duration_ms: Option<u64>,
}

/// `POST /v1/ports/{port}/signals`
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SignalsBody {
    /// Data terminal ready.
    dtr: Option<bool>,
    /// Request to send.
    rts: Option<bool>,
}

/// `POST /v1/ports/{port}/transfers`
///
/// One route for both directions, told apart by `direction`, because a
/// transfer is the same operation with the file going the other way.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "direction", rename_all = "lowercase")]
enum TransferBody {
    /// Send a file the daemon can read.
    Send {
        /// Path on the machine the daemon runs on.
        path: String,
        protocol: crate::modem::FileTransferProtocol,
    },
    /// Receive a file into a directory the daemon can write to.
    Receive {
        /// Directory on the machine the daemon runs on.
        directory: String,
        protocol: crate::modem::FileTransferProtocol,
    },
}

/// `POST /v1/ports/{port}/esp/flash`
#[cfg(feature = "esp")]
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EspFlashBody {
    /// Path to the firmware, on the machine the daemon runs on.
    firmware: String,
    /// Baud rate for the flash itself, not for the port afterwards.
    baud: Option<u32>,
}

/// `POST /v1/ports/{port}/esp/write-bin`
#[cfg(feature = "esp")]
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EspWriteBinBody {
    /// Path to the binary, on the machine the daemon runs on.
    path: String,
    /// Flash address, such as `0x1000`.
    address: String,
}

/// `GET /v1/jobs/{job}/stream`
#[cfg(feature = "esp")]
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct JobQuery {
    /// The last event id already seen, so the stream resumes after it.
    after: Option<usize>,
}

/// `GET /v1/ports/{port}/lines`
///
/// Every field of `ReadWindow` is reachable, so the HTTP caller can ask the
/// same questions the CLI and the MCP server can. `since` is RFC 3339 rather
/// than a nanosecond count, parsed by the same function the MCP server uses.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct LinesQuery {
    start: Option<i64>,
    after: Option<i64>,
    tail: Option<u32>,
    since: Option<String>,
    limit: Option<u32>,
    wait_ms: Option<u64>,
}

impl LinesQuery {
    fn window(self) -> Result<crate::protocol::ReadWindow, String> {
        Ok(crate::protocol::ReadWindow {
            start_id: self.start,
            after_id: self.after,
            tail: self.tail,
            since_ns: parse_time(self.since.as_deref())?,
            limit: self.limit,
            wait_ms: self.wait_ms,
        })
    }
}

/// `GET /v1/ports/{port}/search`
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SearchQuery {
    q: String,
    mode: Option<crate::protocol::SearchMode>,
    from: Option<String>,
    to: Option<String>,
    limit: Option<u32>,
}

impl SearchQuery {
    fn payload(self, port: String) -> Result<crate::protocol::RequestPayload, String> {
        Ok(crate::protocol::RequestPayload::Search {
            port,
            query: self.q,
            mode: self.mode.unwrap_or_default(),
            start_ns: parse_time(self.from.as_deref())?,
            end_ns: parse_time(self.to.as_deref())?,
            limit: self.limit,
        })
    }
}

/// An RFC 3339 instant as the store counts them.
///
/// The same shape the MCP server accepts, so the three transports take the
/// same input rather than each inventing a time format.
fn parse_time(value: Option<&str>) -> Result<Option<i64>, String> {
    value
        .map(|text| {
            chrono::DateTime::parse_from_rfc3339(text)
                .map(|dt| dt.timestamp_nanos_opt().unwrap_or(0))
                .map_err(|e| format!("invalid time '{text}': {e}"))
        })
        .transpose()
}

/// Refuse a request that carries no valid bearer token, when one is required.
///
/// No token is configured for a loopback bind, and then this passes everything
/// through. That is the decided trade and it is stated in the module header.
async fn require_token(
    axum::extract::State(guards): axum::extract::State<Arc<Guards>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let Some(expected) = guards.token.as_ref() else {
        return next.run(request).await;
    };

    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    if presented == Some(expected.as_str()) {
        return next.run(request).await;
    }

    problem(
        axum::http::StatusCode::UNAUTHORIZED,
        "unauthorized",
        "a bearer token is required for this interface",
    )
}

/// Refuse a body that does not declare itself as JSON.
///
/// The second half of the loopback decision, and the reason it is only on the
/// methods that carry a body: a cross-origin `fetch` with `PUT` or `DELETE`
/// needs a CORS preflight, which this server never grants, so those are
/// already out of a web page's reach. `POST` is a simple method and is not,
/// unless it declares a content type a form cannot set. `GET` changes nothing
/// and is covered by the host check alone.
async fn require_json_body(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let carries_body = matches!(
        *request.method(),
        axum::http::Method::POST | axum::http::Method::PUT | axum::http::Method::PATCH
    );
    if !carries_body {
        return next.run(request).await;
    }

    let declared = request
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();

    // A parameter such as `; charset=utf-8` is allowed; the type is what counts.
    if declared
        .split(';')
        .next()
        .map(str::trim)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("application/json"))
    {
        return next.run(request).await;
    }

    problem(
        axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "not-json",
        "a body has to declare Content-Type: application/json",
    )
}

/// Refuse a request that did not come from this machine's own loopback name.
///
/// This is the other half of the loopback decision, and it is not optional.
/// A page in the user's browser can reach `127.0.0.1`, and DNS rebinding lets
/// an attacker's hostname resolve there and carry their origin along. Checking
/// the `Host` header against the names this listener answers to is what stops
/// that, because the browser sends the name it dialled, not the address.
async fn require_local_host(
    axum::extract::State(guards): axum::extract::State<Arc<Guards>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();

    if host_is_local(host, guards.port) {
        return next.run(request).await;
    }

    problem(
        axum::http::StatusCode::MISDIRECTED_REQUEST,
        "host-not-local",
        "this interface answers only to localhost and 127.0.0.1",
    )
}

/// Whether a `Host` header names this listener on a loopback name.
///
/// A missing port means the default for the scheme, which is never ours, so a
/// header without one is refused rather than assumed.
fn host_is_local(host: &str, port: u16) -> bool {
    let (name, given_port) = match host.rsplit_once(':') {
        // An IPv6 literal carries colons of its own; the port is the part
        // after the last one only when the name is bracketed.
        Some((name, _)) if !name.ends_with(']') && name.contains(':') => (host, None),
        Some((name, tail)) => (name, tail.parse::<u16>().ok()),
        None => (host, None),
    };

    if given_port != Some(port) {
        return false;
    }

    matches!(
        name.trim_start_matches('[').trim_end_matches(']'),
        "localhost" | "127.0.0.1" | "::1"
    )
}

/// An error in the shape RFC 9457 describes.
///
/// A typed `type` a client can branch on and a `detail` a person can read,
/// rather than a bare string that has to be parsed.
fn problem(status: axum::http::StatusCode, kind: &str, detail: &str) -> axum::response::Response {
    use axum::response::IntoResponse as _;

    let body = serde_json::json!({
        "type": format!("https://devserial.dev/problem/{kind}"),
        "title": status.canonical_reason().unwrap_or("Error"),
        "status": status.as_u16(),
        "detail": detail,
    });
    let mut response = (status, axum::Json(body)).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/problem+json"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An engine with its own data directory, so a route test can actually
    /// reach one rather than asserting against a stub.
    fn test_engine() -> (crate::engine::CommandEngine, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut settings = crate::config::Config::default();
        settings.global.data_dir = dir.path().to_path_buf();
        settings.global.archive_dir = dir.path().join("archive");
        let engine = crate::engine::CommandEngine::new(
            crate::port_manager::PortManagerHandle::new(),
            Arc::new(std::sync::Mutex::new(
                crate::state::StateDb::open_memory().expect("state"),
            )),
            Arc::new(settings),
        );
        (engine, dir)
    }

    fn config(bind: &str, port: u16) -> RestConfig {
        RestConfig {
            enabled: false,
            bind: bind.to_string(),
            port,
            token: None,
        }
    }

    /// The three failures a person can act on are told apart.
    ///
    /// One message for all of them would leave the reader guessing which of
    /// the three questions to ask: is something else listening, do I need
    /// privileges, or is that not my address.
    #[test]
    fn the_three_bind_failures_read_differently() {
        let addr: std::net::SocketAddr = "127.0.0.1:9600".parse().unwrap();

        let in_use = bind_failure(addr, &std::io::Error::from(std::io::ErrorKind::AddrInUse));
        assert!(in_use.contains("9600"), "{in_use}");
        assert!(in_use.contains("in use"), "{in_use}");

        let denied = bind_failure(
            "127.0.0.1:443".parse().unwrap(),
            &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );
        assert!(
            denied.contains("443") && denied.contains("1024"),
            "{denied}"
        );

        let missing = bind_failure(
            "192.168.1.9:9600".parse().unwrap(),
            &std::io::Error::from(std::io::ErrorKind::AddrNotAvailable),
        );
        assert!(missing.contains("192.168.1.9"), "{missing}");

        // Anything else still says what happened rather than nothing.
        let other = bind_failure(addr, &std::io::Error::from(std::io::ErrorKind::Other));
        assert!(other.contains("127.0.0.1:9600"), "{other}");

        let all = [&in_use, &denied, &missing];
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b, "two failures read the same");
            }
        }
    }

    /// A bind that leaves the machine refuses to start without a token.
    // Needs a runtime: building an engine spawns the port manager's task.
    #[tokio::test]
    async fn a_non_loopback_bind_without_a_token_is_refused() {
        let mut server = RestServer::new(&config("0.0.0.0", 0));
        let refused = server
            .enable(&config("0.0.0.0", 0), &test_engine().0)
            .expect_err("0.0.0.0 without a token has to be refused");
        assert!(refused.contains("token"), "{refused}");
        assert!(!server.state().listening);
    }

    /// An address that is not an address is reported before anything binds.
    // Needs a runtime: building an engine spawns the port manager's task.
    #[tokio::test]
    async fn a_bind_that_is_not_an_address_is_reported() {
        let mut server = RestServer::new(&config("localhost", 0));
        let refused = server
            .enable(&config("localhost", 0), &test_engine().0)
            .expect_err("a name is not an address");
        assert!(refused.contains("localhost"), "{refused}");
    }

    /// A name is not loopback, however much it looks like one.
    ///
    /// Resolving it would let `localhost.attacker.example` through the check
    /// that decides whether a token is needed.
    #[test]
    fn only_an_address_counts_as_loopback() {
        assert!(config("127.0.0.1", 9600).binds_loopback());
        assert!(config("::1", 9600).binds_loopback());
        assert!(!config("localhost", 9600).binds_loopback());
        assert!(!config("0.0.0.0", 9600).binds_loopback());
        assert!(!config("192.168.1.9", 9600).binds_loopback());
    }

    /// The route each request variant is served by, or why it has none.
    ///
    /// This is the ratchet. The match has no catch-all, so a new variant in
    /// `RequestPayload` does not compile until it is named here, and the test
    /// below then checks that whatever it names is a route the router actually
    /// serves. A request that grew a transport in the vocabulary but not on the
    /// wire would otherwise be invisible.
    fn served_by(
        payload: &crate::protocol::RequestPayload,
    ) -> Result<&'static [&'static str], &'static str> {
        use crate::protocol::RequestPayload as R;
        match payload {
            R::Ping => Ok(&["GET /v1/health"]),
            R::ListPorts | R::ListHardware => Ok(&["GET /v1/ports"]),
            R::OpenPort { .. } | R::ReconfigurePort { .. } => Ok(&["PUT /v1/ports/{port}"]),
            R::ClosePort { .. } => Ok(&["DELETE /v1/ports/{port}"]),
            R::GetStatus { .. } => Ok(&["GET /v1/ports/{port}"]),
            R::GetStats { .. } => Ok(&["GET /v1/ports/{port}/stats"]),
            // Two routes, one request: a page and the same page as it grows.
            R::ReadLines { .. } => Ok(&[
                "GET /v1/ports/{port}/lines",
                "GET /v1/ports/{port}/lines/stream",
            ]),
            R::Clear { .. } => Ok(&["DELETE /v1/ports/{port}/lines"]),
            R::Search { .. } => Ok(&["GET /v1/ports/{port}/search"]),
            R::Export { .. } => Ok(&["POST /v1/ports/{port}/export"]),

            R::Shutdown => Err(
                "stopping the daemon over HTTP would let a caller remove the thing answering it",
            ),
            R::WriteData { .. } => Ok(&["POST /v1/ports/{port}/write"]),
            R::SendBreak { .. } => Ok(&["POST /v1/ports/{port}/break"]),
            R::SetSignal { .. } => Ok(&["POST /v1/ports/{port}/signals"]),
            R::ExecuteMacro { .. } => Ok(&["POST /v1/ports/{port}/macros/{name}"]),
            // One route for both, told apart by the direction in the body.
            R::SendFile { .. } | R::ReceiveFile { .. } => Ok(&["POST /v1/ports/{port}/transfers"]),
            #[cfg(feature = "esp")]
            R::EspInfo { .. } => Ok(&["GET /v1/ports/{port}/esp"]),
            // Two routes, one request: starting the flash and watching it.
            #[cfg(feature = "esp")]
            R::EspFlash { .. } => Ok(&[
                "POST /v1/ports/{port}/esp/flash",
                "GET /v1/jobs/{job}/stream",
            ]),
            #[cfg(feature = "esp")]
            R::EspErase { .. } => Ok(&["POST /v1/ports/{port}/esp/erase"]),
            #[cfg(feature = "esp")]
            R::EspWriteBin { .. } => Ok(&["POST /v1/ports/{port}/esp/write-bin"]),

            // Recorded exceptions, each with the reason it is one.
            R::RestStatus | R::RestEnable { .. } | R::RestDisable => {
                Err("the switch is reached through the daemon, not through the thing it switches")
            }
        }
    }

    /// One of every request variant, so the ratchet can walk them.
    fn every_request() -> Vec<crate::protocol::RequestPayload> {
        use crate::protocol::{ReadWindow, RequestPayload as R, SearchMode};
        let port = || "/dev/ttyUSB0".to_string();
        let mut all = vec![
            R::Ping,
            R::ListPorts,
            R::ListHardware,
            R::OpenPort {
                name: port(),
                settings: crate::protocol::PortSettings::default(),
            },
            R::ReconfigurePort {
                name: port(),
                settings: crate::protocol::PortSettings::default(),
            },
            R::ClosePort { name: port() },
            R::ReadLines {
                port: port(),
                window: ReadWindow::default(),
            },
            R::GetStatus { port: port() },
            R::WriteData {
                port: port(),
                data: "x".into(),
                is_hex: false,
            },
            R::SendBreak {
                port: port(),
                duration_ms: None,
            },
            R::SendFile {
                port: port(),
                file_path: "f".into(),
                protocol: crate::modem::FileTransferProtocol::Zmodem,
            },
            R::ReceiveFile {
                port: port(),
                output_dir: ".".into(),
                protocol: crate::modem::FileTransferProtocol::Zmodem,
            },
            R::SetSignal {
                port: port(),
                dtr: None,
                rts: None,
            },
            R::ExecuteMacro {
                port: port(),
                macro_name: "reset".into(),
            },
            R::Search {
                port: port(),
                query: "q".into(),
                mode: SearchMode::Substring,
                start_ns: None,
                end_ns: None,
                limit: None,
            },
            R::Export {
                port: port(),
                output_path: "o".into(),
                file_format: None,
                start_line: None,
                end_line: None,
            },
            R::Clear {
                port: port(),
                archive_current: None,
            },
            R::GetStats { port: port() },
            R::RestStatus,
            R::RestEnable {
                bind: None,
                port: None,
                token: None,
            },
            R::RestDisable,
            R::Shutdown,
        ];
        #[cfg(feature = "esp")]
        all.extend([
            R::EspFlash {
                port: port(),
                firmware_path: "f".into(),
                baud: None,
            },
            R::EspInfo { port: port() },
            R::EspErase { port: port() },
            R::EspWriteBin {
                port: port(),
                file_path: "f".into(),
                address: "0x0".into(),
            },
        ]);
        all
    }

    /// Every request variant has a route, or a reason it has none.
    ///
    /// Both halves matter. A variant claiming a route the router does not
    /// serve would be a promise nothing keeps, and a variant with neither
    /// would be an operation the interface silently cannot do.
    #[test]
    fn every_request_has_a_route_or_a_recorded_reason() {
        for payload in every_request() {
            match served_by(&payload) {
                Ok(routes) => {
                    assert!(!routes.is_empty(), "{payload:?} claims no route at all");
                    for route in routes {
                        assert!(
                            ROUTES.contains(route),
                            "{payload:?} claims {route}, which the router does not serve"
                        );
                    }
                }
                Err(reason) => assert!(
                    !reason.is_empty(),
                    "{payload:?} has neither a route nor a reason"
                ),
            }
        }
    }

    /// Every route the interface advertises is claimed by some request.
    ///
    /// The other direction: a route in the list that nothing maps to would be
    /// advertised by `/v1/version` and answer nothing useful.
    #[test]
    fn every_route_serves_a_request() {
        let claimed: std::collections::BTreeSet<&str> = every_request()
            .iter()
            .filter_map(|payload| served_by(payload).ok())
            .flat_map(<[&str]>::iter)
            .copied()
            .collect();
        // Two routes describe the build rather than carrying out a request,
        // so they are the ones with nothing to claim them.
        let about_itself = ["GET /v1/version", "GET /v1/openapi.json"];
        for route in ROUTES {
            if about_itself.contains(route) {
                continue;
            }
            assert!(
                claimed.contains(route),
                "{route} is advertised but no request maps to it"
            );
        }
    }

    /// A `Host` header from somewhere else is refused.
    #[test]
    fn only_a_loopback_host_on_our_port_passes() {
        assert!(host_is_local("127.0.0.1:9600", 9600));
        assert!(host_is_local("localhost:9600", 9600));
        assert!(host_is_local("[::1]:9600", 9600));

        // A rebound name, the attack this guard exists for.
        assert!(!host_is_local("localhost.attacker.example:9600", 9600));
        assert!(!host_is_local("evil.example:9600", 9600));
        // The right name on the wrong port is not this listener.
        assert!(!host_is_local("127.0.0.1:9601", 9600));
        // No port means the scheme default, which is never ours.
        assert!(!host_is_local("127.0.0.1", 9600));
        assert!(!host_is_local("", 9600));
    }

    /// One HTTP request, spoken by hand.
    ///
    /// No client crate for two GETs: a raw request is fewer moving parts than
    /// a dependency, and it proves the listener speaks HTTP rather than that a
    /// client library agrees with a server library.
    fn get(addr: std::net::SocketAddr, path: &str, token: Option<&str>) -> String {
        send(addr, "GET", path, token, None, Some(&addr.to_string()))
    }

    /// One HTTP request, spoken by hand.
    ///
    /// `host` overrides the `Host` header so the guard can be tested; `body`
    /// carries JSON and sets the content type unless the caller wants it
    /// missing, which is the other guard.
    fn send(
        addr: std::net::SocketAddr,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&str>,
        host: Option<&str>,
    ) -> String {
        use std::io::{Read as _, Write as _};

        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        let auth = token.map_or_else(String::new, |t| format!("Authorization: Bearer {t}\r\n"));
        let host = host.map_or_else(String::new, |h| format!("Host: {h}\r\n"));
        let body_headers = body.map_or_else(String::new, |b| {
            format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                b.len()
            )
        });
        let request = format!(
            "{method} {path} HTTP/1.1\r\n{host}{auth}{body_headers}Connection: close\r\n\r\n{}",
            body.unwrap_or_default()
        );
        stream.write_all(request.as_bytes()).expect("write");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("timeout");
        let mut response = String::new();
        drop(stream.read_to_string(&mut response));
        response
    }

    /// A body with no content type at all, for the guard that wants one.
    fn send_untyped(addr: std::net::SocketAddr, method: &str, path: &str, body: &str) -> String {
        use std::io::{Read as _, Write as _};

        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).expect("write");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("timeout");
        let mut response = String::new();
        drop(stream.read_to_string(&mut response));
        response
    }

    /// The JSON body of a response, for a test that reads it.
    fn body_of(response: &str) -> serde_json::Value {
        let body = response
            .split_once("\r\n\r\n")
            .map_or("", |(_, body)| body)
            .trim();
        serde_json::from_str(body).unwrap_or_else(|e| panic!("body is not JSON: {e}\n{response}"))
    }

    /// A listener on an ephemeral loopback port.
    ///
    /// The tests using this need a multi-threaded runtime. The request below
    /// blocks its thread, and on a current-thread runtime that thread is also
    /// the one driving the server, so the request waits for an answer nobody
    /// is left to write. It shows up as an empty response rather than as a
    /// deadlock, which is why it is worth a note.
    fn listening(
        token: Option<&str>,
    ) -> (
        RestServer,
        std::net::SocketAddr,
        crate::engine::CommandEngine,
        tempfile::TempDir,
    ) {
        let mut config = config("127.0.0.1", 0);
        config.token = token.map(str::to_owned);
        let (engine, dir) = test_engine();
        let mut server = RestServer::new(&config);
        let state = server.enable(&config, &engine).expect("binding loopback");

        // The port was ephemeral, so the address comes from the state.
        let addr = format!("{}:{}", state.bind, state.port)
            .parse()
            .expect("the state carries a usable address");
        (server, addr, engine, dir)
    }

    /// The listener answers both routes, and says what it is.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_listener_answers_health_and_version() {
        let (_server, addr, _engine, _dir) = listening(None);

        let health = get(addr, "/v1/health", None);
        assert!(health.starts_with("HTTP/1.1 200"), "{health}");
        assert!(health.contains("\"status\":\"ok\""), "{health}");

        let version = get(addr, "/v1/version", None);
        assert!(version.starts_with("HTTP/1.1 200"), "{version}");
        assert!(version.contains(env!("CARGO_PKG_VERSION")), "{version}");
        assert!(version.contains("\"rest\""), "{version}");

        // A path nothing serves is a 404 rather than something else.
        let absent = get(addr, "/v1/nothing-here", None);
        assert!(absent.starts_with("HTTP/1.1 404"), "{absent}");
    }

    /// A configured token is enforced, and health stays reachable without it.
    ///
    /// Health has to answer before a caller is told it is not allowed in,
    /// otherwise "is anything there" and "may I in" give the same answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_token_is_enforced_but_health_is_not_behind_it() {
        let (_server, addr, _engine, _dir) = listening(Some("s3cret"));

        let health = get(addr, "/v1/health", None);
        assert!(health.starts_with("HTTP/1.1 200"), "{health}");

        let refused = get(addr, "/v1/version", None);
        assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
        assert!(
            refused.contains("application/problem+json"),
            "an error is a problem document: {refused}"
        );

        let wrong = get(addr, "/v1/version", Some("wrong"));
        assert!(wrong.starts_with("HTTP/1.1 401"), "{wrong}");

        let allowed = get(addr, "/v1/version", Some("s3cret"));
        assert!(allowed.starts_with("HTTP/1.1 200"), "{allowed}");
    }

    /// Stopping ends the listener rather than leaving it accepting.
    #[tokio::test(flavor = "multi_thread")]
    async fn stopping_closes_the_socket() {
        let (mut server, addr, _engine, _dir) = listening(None);
        assert!(get(addr, "/v1/health", None).starts_with("HTTP/1.1 200"));

        let state = server.disable();
        assert!(!state.listening);

        // The abort is not instant, so this waits rather than asserting on the
        // first attempt and reporting a race as a defect.
        for _ in 0..50 {
            if std::net::TcpStream::connect(addr).is_err() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("the socket was still accepting a second after being stopped");
    }

    /// A token on loopback is reported as required.
    ///
    /// The middleware enforces whatever the router was built with, so a state
    /// that said otherwise would be a lie about the running listener.
    #[test]
    fn a_configured_token_is_reported_as_required() {
        let mut loopback = config("127.0.0.1", 0);
        assert!(!token_required(&loopback));
        loopback.token = Some("s3cret".to_string());
        assert!(token_required(&loopback));
        assert!(token_required(&config("0.0.0.0", 0)));
    }

    /// A listener with a mock port already open and some captured lines.
    async fn listening_with_lines() -> (
        RestServer,
        std::net::SocketAddr,
        crate::engine::CommandEngine,
        tempfile::TempDir,
        Arc<std::sync::Mutex<crate::storage::SqliteStorage>>,
    ) {
        let (server, addr, engine, dir) = listening(None);

        let (mock, _control) = crate::testutil::mock_serial::mock_serial(4096);
        let storage = Arc::new(std::sync::Mutex::new(
            crate::storage::SqliteStorage::open_memory().expect("storage"),
        ));
        engine
            .port_manager()
            .open(
                "mock_rest_port".into(),
                mock,
                crate::config::PortConfig::default(),
                Arc::clone(&storage),
            )
            .await
            .expect("open the mock port");

        storage
            .lock()
            .expect("storage")
            .insert_lines(&[
                (1_767_225_600_000_000_000, "boot ok"),
                (1_767_225_600_500_000_000, "value = 42"),
                (1_767_225_601_000_000_000, "Guru Meditation"),
            ])
            .expect("insert");

        (server, addr, engine, dir, storage)
    }

    /// The reading routes answer, and answer about the right port.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_reading_routes_answer() {
        let (_server, addr, _engine, _dir, _storage) = listening_with_lines().await;
        let port = "mock_rest_port";

        let ports = get(addr, "/v1/ports", None);
        assert!(ports.starts_with("HTTP/1.1 200"), "{ports}");
        assert!(ports.contains(port), "{ports}");

        let status = get(addr, &format!("/v1/ports/{port}"), None);
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
        assert_eq!(body_of(&status)["port"], port);

        let counted = get(addr, &format!("/v1/ports/{port}/stats"), None);
        assert!(counted.starts_with("HTTP/1.1 200"), "{counted}");
        assert_eq!(body_of(&counted)["total_lines"], 3);

        let lines = get(addr, &format!("/v1/ports/{port}/lines"), None);
        assert!(lines.starts_with("HTTP/1.1 200"), "{lines}");
        let body = body_of(&lines);
        assert_eq!(body["total_lines"], 3);
        assert_eq!(body["lines"].as_array().map(Vec::len), Some(3));

        let found = get(
            addr,
            &format!("/v1/ports/{port}/search?q=Guru&mode=substring"),
            None,
        );
        assert!(found.starts_with("HTTP/1.1 200"), "{found}");
        assert_eq!(body_of(&found)["results"].as_array().map(Vec::len), Some(1));

        // A port that is not managed is a refusal, not an empty answer.
        let absent = get(addr, "/v1/ports/nope/stats", None);
        assert!(absent.starts_with("HTTP/1.1 400"), "{absent}");
        assert!(absent.contains("application/problem+json"), "{absent}");
        assert!(
            body_of(&absent)["type"]
                .as_str()
                .is_some_and(|kind| kind.contains("operation-failed")),
            "a problem document names its type: {absent}"
        );
    }

    /// A line on the wire is the record the export writes.
    ///
    /// Compared against the `jsonl` export of the same lines rather than
    /// against a shape written out here, because the point is that there is one
    /// definition and both go through it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_line_on_the_wire_is_the_line_in_the_export() {
        let (_server, addr, _engine, _dir, storage) = listening_with_lines().await;

        let response = get(addr, "/v1/ports/mock_rest_port/lines", None);
        let wire = body_of(&response)["lines"].clone();

        let stored = storage
            .lock()
            .expect("storage")
            .read_lines(1, 100)
            .expect("read");
        let exported =
            crate::export::to_string(&stored, crate::export::ExportFormat::Jsonl).expect("export");
        let from_file: Vec<serde_json::Value> = exported
            .lines()
            .map(|line| serde_json::from_str(line).expect("jsonl line"))
            .collect();

        assert_eq!(
            wire,
            serde_json::Value::Array(from_file),
            "the wire and the export disagree about the same lines"
        );
    }

    /// Every field of the read window is reachable, and time is RFC 3339.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_read_window_field_is_reachable() {
        let (_server, addr, _engine, _dir, _storage) = listening_with_lines().await;
        let base = "/v1/ports/mock_rest_port/lines";

        // tail
        let tail = body_of(&get(addr, &format!("{base}?tail=2"), None));
        assert_eq!(tail["lines"].as_array().map(Vec::len), Some(2));

        // after
        let after = body_of(&get(addr, &format!("{base}?after=1"), None));
        assert_eq!(after["lines"].as_array().map(Vec::len), Some(2));

        // start and limit
        let start = body_of(&get(addr, &format!("{base}?start=2&limit=1"), None));
        assert_eq!(start["lines"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            start["lines"][0]["id"], 2,
            "the line id is named id since M2"
        );

        // since, as RFC 3339: the third line only.
        let since = body_of(&get(
            addr,
            &format!("{base}?since=2026-01-01T00%3A00%3A00.700Z"),
            None,
        ));
        assert_eq!(since["lines"].as_array().map(Vec::len), Some(1));

        // wait_ms is accepted rather than rejected as unknown.
        let waited = get(addr, &format!("{base}?after=3&wait_ms=1"), None);
        assert!(waited.starts_with("HTTP/1.1 200"), "{waited}");

        // A time that is not a time is refused, and says which value.
        let bad = get(addr, &format!("{base}?since=yesterday"), None);
        assert!(bad.starts_with("HTTP/1.1 400"), "{bad}");
        assert!(bad.contains("yesterday"), "{bad}");
    }

    /// The two guards refuse what they exist to refuse.
    ///
    /// Without them, a page in the user's own browser could reach the
    /// interface: `POST` is a simple method, and a rebound hostname resolves
    /// to loopback while carrying the attacker's origin.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_body_without_json_and_a_foreign_host_are_refused() {
        let (_server, addr, _engine, _dir, _storage) = listening_with_lines().await;
        let path = "/v1/ports/mock_rest_port/export";
        let body = r#"{"path":"/tmp/does-not-matter"}"#;

        let untyped = send_untyped(addr, "POST", path, body);
        assert!(untyped.starts_with("HTTP/1.1 415"), "{untyped}");
        assert!(untyped.contains("application/problem+json"), "{untyped}");
        assert!(
            body_of(&untyped)["type"]
                .as_str()
                .is_some_and(|kind| kind.contains("not-json")),
            "{untyped}"
        );

        let foreign = send(
            addr,
            "GET",
            "/v1/ports",
            None,
            None,
            Some("localhost.attacker.example:9600"),
        );
        assert!(foreign.starts_with("HTTP/1.1 421"), "{foreign}");
        assert!(
            body_of(&foreign)["type"]
                .as_str()
                .is_some_and(|kind| kind.contains("host-not-local")),
            "{foreign}"
        );

        // And the same request with the right host passes, so the refusal is
        // the header rather than the route.
        let allowed = send(
            addr,
            "GET",
            "/v1/ports",
            None,
            None,
            Some(&format!("localhost:{}", addr.port())),
        );
        assert!(allowed.starts_with("HTTP/1.1 200"), "{allowed}");
    }

    /// Opening and closing a port over HTTP reaches the daemon's port manager.
    #[tokio::test(flavor = "multi_thread")]
    async fn closing_a_port_over_http_closes_it_on_the_daemon() {
        let (_server, addr, engine, _dir, _storage) = listening_with_lines().await;

        let closed = send(
            addr,
            "DELETE",
            "/v1/ports/mock_rest_port",
            None,
            None,
            Some(&addr.to_string()),
        );
        assert!(closed.starts_with("HTTP/1.1 200"), "{closed}");

        assert!(
            engine.port_manager().list().await.is_empty(),
            "the daemon still holds the port the route said it closed"
        );
    }

    /// One POST with a JSON body, from the right host.
    fn post(addr: std::net::SocketAddr, path: &str, body: &str) -> String {
        send(
            addr,
            "POST",
            path,
            None,
            Some(body),
            Some(&addr.to_string()),
        )
    }

    /// The write route hands the engine what the body said.
    ///
    /// A port the daemon holds for a mock has no hardware handle, so nothing
    /// reaches a wire here and the hardware gate is where that is seen. What
    /// this pins down is the step before it: an invalid hex string fails while
    /// it is being decoded, which only happens when `hex` arrived as true, and
    /// the same string without it is taken as text and gets to the port.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_write_route_hands_the_engine_what_the_body_said() {
        let (_server, addr, _engine, _dir, _storage) = listening_with_lines().await;
        let path = "/v1/ports/mock_rest_port/write";

        let hex = post(addr, path, r#"{"data":"zz","hex":true}"#);
        assert!(hex.starts_with("HTTP/1.1 400"), "{hex}");
        assert!(hex.contains("invalid hex digit 'zz'"), "{hex}");

        let text = post(addr, path, r#"{"data":"zz"}"#);
        assert!(text.contains("no hardware handle"), "{text}");

        // A prefix says hex on its own, as it does everywhere else.
        let prefixed = post(addr, path, r#"{"data":"0xzz"}"#);
        assert!(prefixed.contains("invalid hex digit"), "{prefixed}");
    }

    /// Break, signals and macros are carried out by the engine.
    ///
    /// A mock port has no hardware handle, so the engine's own refusal is what
    /// comes back. That refusal is the evidence: it can only be produced by a
    /// request that reached the engine with the right port in it.
    #[tokio::test(flavor = "multi_thread")]
    async fn break_signals_and_macros_reach_the_engine() {
        let (_server, addr, _engine, _dir, _storage) = listening_with_lines().await;

        let broken = post(
            addr,
            "/v1/ports/mock_rest_port/break",
            r#"{"duration_ms":5}"#,
        );
        assert!(broken.starts_with("HTTP/1.1 400"), "{broken}");
        assert!(broken.contains("no hardware handle"), "{broken}");

        // Nothing to set is refused by the engine rather than passed on.
        let nothing = post(addr, "/v1/ports/mock_rest_port/signals", "{}");
        assert!(nothing.contains("at least one of dtr or rts"), "{nothing}");

        let signalled = post(addr, "/v1/ports/mock_rest_port/signals", r#"{"dtr":true}"#);
        assert!(signalled.contains("no hardware handle"), "{signalled}");

        // The name in the path is the macro that is looked up: an unknown one
        // is refused by name, a known one gets as far as the port.
        let unknown = post(addr, "/v1/ports/mock_rest_port/macros/not-a-macro", "{}");
        assert!(unknown.contains("unknown macro 'not-a-macro'"), "{unknown}");
        let known = post(addr, "/v1/ports/mock_rest_port/macros/reset", "{}");
        assert!(known.contains("no hardware handle"), "{known}");
    }

    /// A transfer reaches the engine in both directions.
    ///
    /// Against a port that does not exist, so the modem protocol never starts
    /// and the test measures the route rather than the timing of a handshake.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_transfer_reaches_the_engine_in_both_directions() {
        let (_server, addr, _engine, _dir, _storage) = listening_with_lines().await;
        let path = "/v1/ports/nope/transfers";

        let sending = post(
            addr,
            path,
            r#"{"direction":"send","path":"/tmp/does-not-matter.bin","protocol":"zmodem"}"#,
        );
        assert!(sending.starts_with("HTTP/1.1 400"), "{sending}");
        assert!(
            sending.contains("does-not-matter.bin"),
            "the refusal names the file from the body: {sending}"
        );

        let receiving = post(
            addr,
            path,
            r#"{"direction":"receive","directory":"/tmp","protocol":"ymodem"}"#,
        );
        assert!(receiving.starts_with("HTTP/1.1 400"), "{receiving}");
        assert!(receiving.contains("nope"), "{receiving}");

        // A direction the interface does not have is refused before anything
        // touches a port.
        let sideways = post(addr, path, r#"{"direction":"sideways","path":"/tmp/x"}"#);
        assert!(
            sideways.starts_with("HTTP/1.1 422") || sideways.starts_with("HTTP/1.1 400"),
            "{sideways}"
        );
        assert!(!sideways.contains("nope"), "{sideways}");
    }

    /// The ESP routes are there with the feature and absent without it.
    ///
    /// The answer with the feature is a refusal on this machine, because there
    /// is no board and usually no espflash either. What it is not is a 404,
    /// which is the whole difference this test measures.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_esp_routes_follow_the_feature() {
        let (_server, addr, _engine, _dir, _storage) = listening_with_lines().await;
        let answer = get(addr, "/v1/ports/mock_rest_port/esp", None);

        #[cfg(feature = "esp")]
        assert!(!answer.starts_with("HTTP/1.1 404"), "{answer}");
        #[cfg(not(feature = "esp"))]
        assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");

        let advertised = ROUTES.iter().any(|route| route.contains("/esp"));
        assert_eq!(advertised, cfg!(feature = "esp"));
    }

    /// The flash route answers with a job, and the job's stream carries it.
    ///
    /// The flash cannot succeed here: there is no board, and usually no
    /// espflash. That is what makes this a test of the job rather than of the
    /// tool. A client that only ever got the `202` would be left waiting, so
    /// the outcome has to arrive on the stream whatever it is.
    #[cfg(feature = "esp")]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_flash_route_answers_with_a_job_and_streams_its_outcome() {
        let (_server, addr, _engine, _dir, _storage) = listening_with_lines().await;

        let started = post(
            addr,
            "/v1/ports/mock_rest_port/esp/flash",
            r#"{"firmware":"/nonexistent/firmware.bin"}"#,
        );
        assert!(started.starts_with("HTTP/1.1 202"), "{started}");
        let body = body_of(&started);
        let job = body["job"]
            .as_str()
            .expect("the answer names a job")
            .to_owned();
        assert_eq!(body["stream"], format!("/v1/jobs/{job}/stream"));

        let seen = stream_until(
            addr,
            &format!("/v1/jobs/{job}/stream"),
            None,
            "event: done",
            1,
        );
        assert!(seen.starts_with("HTTP/1.1 200"), "{seen}");
        assert!(seen.contains("text/event-stream"), "{seen}");
        assert!(
            seen.contains(r#""ok":false"#),
            "the outcome has to reach the client: {seen}"
        );

        // A job nobody started is a refusal rather than a stream that never
        // says anything.
        let absent = get(addr, "/v1/jobs/nosuchjob/stream", None);
        assert!(absent.starts_with("HTTP/1.1 404"), "{absent}");
        assert!(absent.contains("no-such-job"), "{absent}");
    }

    /// Every route the interface advertises is actually mounted.
    ///
    /// The list is what `/v1/version` hands out and what the description is
    /// built from, so a path in it that the router does not carry would be a
    /// promise broken at a 404. The answers here are mostly refusals — an
    /// empty body is not an export, and nothing can be flashed on this
    /// machine — so what this looks at is whether anything is served at all.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_advertised_route_is_mounted() {
        let (_server, addr, _engine, _dir, _storage) = listening_with_lines().await;

        // One route answers 404 by right: a job that was never started. So one
        // is started, and the sweep asks about that.
        #[cfg(feature = "esp")]
        let flashing = {
            let started = post(
                addr,
                "/v1/ports/mock_rest_port/esp/flash",
                r#"{"firmware":"/nonexistent/firmware.bin"}"#,
            );
            body_of(&started)["job"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        };
        #[cfg(not(feature = "esp"))]
        let flashing = String::new();

        for route in ROUTES {
            let (method, template) = route.split_once(' ').expect("a method and a path");
            let path = template
                .replace("{port}", "mock_rest_port")
                .replace("{name}", "reset")
                .replace("{job}", &flashing);

            // A stream never ends, so it is read only until it has said what
            // it is.
            let answer = if template.ends_with("/stream") {
                stream_until(addr, &path, None, "HTTP/1.1", 1)
            } else if method == "GET" {
                get(addr, &path, None)
            } else {
                send(
                    addr,
                    method,
                    &path,
                    None,
                    Some("{}"),
                    Some(&addr.to_string()),
                )
            };

            assert!(
                answer.starts_with("HTTP/1.1 "),
                "{route} answered nothing at all: {answer}"
            );
            assert!(
                !answer.starts_with("HTTP/1.1 404"),
                "{route} is advertised but not served"
            );
        }
    }

    /// The description covers every route the interface serves, and no more.
    ///
    /// This is the second ratchet. A route added to the list without a
    /// sentence, or a description that drifts from the list, fails here rather
    /// than being published as a specification nobody checked.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_description_covers_every_route() {
        let (_server, addr, _engine, _dir, _storage) = listening_with_lines().await;

        let response = get(addr, "/v1/openapi.json", None);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let document = body_of(&response);
        assert_eq!(document["openapi"], "3.1.0");
        // The server it names is the one it is answering on.
        assert_eq!(
            document["servers"][0]["url"],
            format!("http://127.0.0.1:{}", addr.port())
        );

        let paths = document["paths"]
            .as_object()
            .expect("the document has paths");
        let mut names = std::collections::BTreeSet::new();
        for route in ROUTES {
            let (method, path) = route
                .split_once(' ')
                .expect("a route is a method and a path");
            let operation = paths
                .get(path)
                .and_then(|entry| entry.get(method.to_lowercase()))
                .unwrap_or_else(|| panic!("{route} is served but not described"));

            assert!(
                operation["summary"].as_str().is_some_and(|s| !s.is_empty()),
                "{route} is described without saying what it is for"
            );
            let name = operation["operationId"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            assert!(!name.is_empty(), "{route} is described without a name");
            assert!(
                names.insert(name),
                "{route} shares its name with another route"
            );

            // Every placeholder in the path is a parameter of the operation.
            let declared: Vec<&str> = operation["parameters"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p["in"] == "path")
                .filter_map(|p| p["name"].as_str())
                .collect();
            for placeholder in path
                .split('/')
                .filter_map(|s| s.strip_prefix('{')?.strip_suffix('}'))
            {
                assert!(
                    declared.contains(&placeholder),
                    "{route} hides {placeholder}"
                );
            }
        }

        // Nothing is described that is not served.
        let described: usize = paths
            .values()
            .filter_map(serde_json::Value::as_object)
            .map(serde_json::Map::len)
            .sum();
        assert_eq!(
            described,
            ROUTES.len(),
            "the description and the route list disagree"
        );

        // The bodies come from the structs the handlers read, which is what
        // makes this a description rather than a second hand-written list.
        let write = &paths["/v1/ports/{port}/write"]["post"]["requestBody"]["content"]["application/json"]
            ["schema"];
        assert!(
            write["properties"]["data"].is_object() && write["properties"]["hex"].is_object(),
            "the write body is not described from its own struct: {write}"
        );
        let lines = &paths["/v1/ports/{port}/lines"]["get"]["parameters"];
        let query: Vec<&str> = lines
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["in"] == "query")
            .filter_map(|p| p["name"].as_str())
            .collect();
        for field in ["start", "after", "tail", "since", "limit", "wait_ms"] {
            assert!(query.contains(&field), "{field} is not described: {lines}");
        }
    }

    /// Open a stream, read what arrives, and hang up.
    ///
    /// A streaming response never closes, so this reads until it has seen the
    /// marker it was told to wait for or the deadline passes, then drops the
    /// socket. Dropping is the point of the second test: it is what a client
    /// going away looks like from the server's side.
    fn stream_until(
        addr: std::net::SocketAddr,
        path: &str,
        last_event_id: Option<i64>,
        marker: &str,
        events: usize,
    ) -> String {
        use std::io::{Read as _, Write as _};

        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        let resume =
            last_event_id.map_or_else(String::new, |id| format!("Last-Event-ID: {id}\r\n"));
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: text/event-stream\r\n{resume}\r\n"
        );
        stream.write_all(request.as_bytes()).expect("write");
        stream
            .set_read_timeout(Some(std::time::Duration::from_millis(250)))
            .expect("timeout");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = String::new();
        let mut buf = [0u8; 2048];
        while std::time::Instant::now() < deadline {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    seen.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if seen.matches(marker).count() >= events {
                        break;
                    }
                }
                Err(_) => {}
            }
        }
        seen
    }

    /// The stream carries every line with its id, and resumes from it.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_stream_carries_line_ids_and_resumes_from_them() {
        let (_server, addr, _engine, _dir, storage) = listening_with_lines().await;
        let path = "/v1/ports/mock_rest_port/lines/stream";

        let first = stream_until(addr, path, None, "event: line", 3);
        assert!(first.starts_with("HTTP/1.1 200"), "{first}");
        assert!(
            first.contains("text/event-stream"),
            "the stream has to say what it is: {first}"
        );
        assert_eq!(first.matches("event: line").count(), 3, "{first}");
        assert!(
            first.contains("id: 1") && first.contains("id: 3"),
            "{first}"
        );
        assert!(first.contains("Guru Meditation"), "{first}");

        // A line written while nobody was listening.
        storage
            .lock()
            .expect("storage")
            .insert_lines(&[(1_767_225_602_000_000_000, "after the gap")])
            .expect("insert");

        // Resuming from the second line gives the third and the new one, and
        // not the two the client already had.
        let resumed = stream_until(addr, path, Some(2), "event: line", 2);
        assert_eq!(resumed.matches("event: line").count(), 2, "{resumed}");
        assert!(resumed.contains("Guru Meditation"), "{resumed}");
        assert!(resumed.contains("after the gap"), "{resumed}");
        assert!(
            !resumed.contains("boot ok"),
            "resuming replayed a line the client had already seen: {resumed}"
        );
    }

    /// A client that goes away leaves the daemon holding nothing.
    ///
    /// Measured rather than assumed: the streaming response holds a clone of
    /// the engine, so the count of engines sharing the listener state rises
    /// while it runs and falls back when the socket is dropped. A loop that
    /// kept running would keep its clone and the count would stay up.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_that_hangs_up_releases_the_engine() {
        let (_server, addr, engine, _dir, _storage) = listening_with_lines().await;
        let before = engine.shared_handles();

        let seen = stream_until(
            addr,
            "/v1/ports/mock_rest_port/lines/stream",
            None,
            "event: line",
            3,
        );
        assert_eq!(seen.matches("event: line").count(), 3, "{seen}");

        // The socket is dropped by now. Give the server a moment to notice.
        for _ in 0..100 {
            if engine.shared_handles() == before {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!(
            "two seconds after the client hung up the daemon still holds {} engines, was {before}",
            engine.shared_handles()
        );
    }
}
