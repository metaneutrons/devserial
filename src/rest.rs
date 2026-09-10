// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! The HTTP interface, and the switch that turns it on.
//!
//! A fourth transport over [`crate::engine::CommandEngine`], beside the CLI,
//! the IPC daemon and the MCP server, and like them it carries no operation
//! logic of its own. This slice builds the listener, the switch and the way a
//! failure is reported; the routes that do the work follow.
//!
//! Two things here are decisions rather than mechanics, and both are written
//! down where they are made: the bind happens inside the request that asked
//! for it, so the answer says what happened, and a request to loopback needs
//! no token while a bind to anything else refuses to start without one.

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
    "DELETE /v1/ports/{port}/lines",
    "GET /v1/ports/{port}/search",
    "POST /v1/ports/{port}/export",
];

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
        .route("/v1/ports/{port}/search", get(search))
        .route("/v1/ports/{port}/export", post(export))
        .with_state(guards.engine.clone())
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

// ------------------------------------------------------------- parameters

/// `GET /v1/ports`
#[derive(Debug, serde::Deserialize)]
struct PortsQuery {
    hardware: Option<bool>,
}

/// `DELETE /v1/ports/{port}/lines`
#[derive(Debug, serde::Deserialize)]
struct ClearQuery {
    archive: Option<bool>,
}

/// `POST /v1/ports/{port}/export`
#[derive(Debug, serde::Deserialize)]
struct ExportBody {
    path: String,
    format: Option<crate::export::ExportFormat>,
    start_line: Option<i64>,
    end_line: Option<i64>,
}

/// `GET /v1/ports/{port}/lines`
///
/// Every field of `ReadWindow` is reachable, so the HTTP caller can ask the
/// same questions the CLI and the MCP server can. `since` is RFC 3339 rather
/// than a nanosecond count, parsed by the same function the MCP server uses.
#[derive(Debug, serde::Deserialize)]
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
#[derive(Debug, serde::Deserialize)]
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
    fn served_by(payload: &crate::protocol::RequestPayload) -> Result<&'static str, &'static str> {
        use crate::protocol::RequestPayload as R;
        match payload {
            R::Ping => Ok("GET /v1/health"),
            R::ListPorts | R::ListHardware => Ok("GET /v1/ports"),
            R::OpenPort { .. } | R::ReconfigurePort { .. } => Ok("PUT /v1/ports/{port}"),
            R::ClosePort { .. } => Ok("DELETE /v1/ports/{port}"),
            R::GetStatus { .. } => Ok("GET /v1/ports/{port}"),
            R::GetStats { .. } => Ok("GET /v1/ports/{port}/stats"),
            R::ReadLines { .. } => Ok("GET /v1/ports/{port}/lines"),
            R::Clear { .. } => Ok("DELETE /v1/ports/{port}/lines"),
            R::Search { .. } => Ok("GET /v1/ports/{port}/search"),
            R::Export { .. } => Ok("POST /v1/ports/{port}/export"),

            // Recorded exceptions, each with the reason it is one.
            R::Shutdown => Err(
                "stopping the daemon over HTTP would let a caller remove the thing answering it",
            ),
            R::WriteData { .. }
            | R::SendBreak { .. }
            | R::SetSignal { .. }
            | R::ExecuteMacro { .. }
            | R::SendFile { .. }
            | R::ReceiveFile { .. } => Err("driving the device is the next slice"),
            #[cfg(feature = "esp")]
            R::EspFlash { .. } | R::EspInfo { .. } | R::EspErase { .. } | R::EspWriteBin { .. } => {
                Err("the ESP routes are the next slice")
            }
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
                Ok(route) => assert!(
                    ROUTES.contains(&route),
                    "{payload:?} claims {route}, which the router does not serve"
                ),
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
            .collect();
        for route in ROUTES {
            // `/v1/version` describes the build rather than carrying out a
            // request, so it is the one route with nothing to claim it.
            if *route == "GET /v1/version" {
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

        let (mock, _ctrl) = crate::testutil::mock_serial::mock_serial(4096);
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
}
