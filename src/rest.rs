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

use std::sync::Arc;

use crate::config::RestConfig;
use crate::protocol::RestState;

/// The routes this slice serves.
///
/// Kept as a list so the test at the bottom can assert that nothing else
/// answers yet. M4 replaces it with the route table.
pub const ROUTES: &[&str] = &["/v1/health", "/v1/version"];

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
    pub fn enable(&mut self, config: &RestConfig) -> Result<RestState, String> {
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
        let router = router(token.clone());
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

/// The router this slice serves.
fn router(token: Option<String>) -> axum::Router {
    use axum::routing::get;

    let open = axum::Router::new().route("/v1/health", get(health));

    let guarded = axum::Router::new()
        .route("/v1/version", get(version))
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(token),
            require_token,
        ));

    open.merge(guarded)
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

/// Refuse a request that carries no valid bearer token, when one is required.
///
/// No token is configured for a loopback bind, and then this passes everything
/// through. That is the decided trade and it is stated in the module header.
async fn require_token(
    axum::extract::State(token): axum::extract::State<Arc<Option<String>>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let Some(expected) = token.as_ref() else {
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
    #[test]
    fn a_non_loopback_bind_without_a_token_is_refused() {
        let mut server = RestServer::new(&config("0.0.0.0", 0));
        let refused = server
            .enable(&config("0.0.0.0", 0))
            .expect_err("0.0.0.0 without a token has to be refused");
        assert!(refused.contains("token"), "{refused}");
        assert!(!server.state().listening);
    }

    /// An address that is not an address is reported before anything binds.
    #[test]
    fn a_bind_that_is_not_an_address_is_reported() {
        let mut server = RestServer::new(&config("localhost", 0));
        let refused = server
            .enable(&config("localhost", 0))
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

    /// This slice serves two routes and no more.
    ///
    /// The list is what M4 grows; until then a third route appearing without
    /// being named here would be a route nothing documents.
    #[test]
    fn only_health_and_version_answer_yet() {
        assert_eq!(ROUTES, ["/v1/health", "/v1/version"]);
    }

    /// One HTTP request, spoken by hand.
    ///
    /// No client crate for two GETs: a raw request is fewer moving parts than
    /// a dependency, and it proves the listener speaks HTTP rather than that a
    /// client library agrees with a server library.
    fn get(addr: std::net::SocketAddr, path: &str, token: Option<&str>) -> String {
        use std::io::{Read as _, Write as _};

        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        let auth = token.map_or_else(String::new, |t| format!("Authorization: Bearer {t}\r\n"));
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Connection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).expect("write");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("timeout");
        let mut response = String::new();
        drop(stream.read_to_string(&mut response));
        response
    }

    /// A listener on an ephemeral loopback port.
    ///
    /// The tests using this need a multi-threaded runtime. The request below
    /// blocks its thread, and on a current-thread runtime that thread is also
    /// the one driving the server, so the request waits for an answer nobody
    /// is left to write. It shows up as an empty response rather than as a
    /// deadlock, which is why it is worth a note.
    fn listening(token: Option<&str>) -> (RestServer, std::net::SocketAddr) {
        let mut config = config("127.0.0.1", 0);
        config.token = token.map(str::to_owned);
        let mut server = RestServer::new(&config);
        let state = server.enable(&config).expect("binding loopback");

        // The port was ephemeral, so the address comes from the state.
        let addr = format!("{}:{}", state.bind, state.port)
            .parse()
            .expect("the state carries a usable address");
        (server, addr)
    }

    /// The listener answers both routes, and says what it is.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_listener_answers_health_and_version() {
        let (_server, addr) = listening(None);

        let health = get(addr, "/v1/health", None);
        assert!(health.starts_with("HTTP/1.1 200"), "{health}");
        assert!(health.contains("\"status\":\"ok\""), "{health}");

        let version = get(addr, "/v1/version", None);
        assert!(version.starts_with("HTTP/1.1 200"), "{version}");
        assert!(version.contains(env!("CARGO_PKG_VERSION")), "{version}");
        assert!(version.contains("\"rest\""), "{version}");

        // Nothing else answers yet.
        let absent = get(addr, "/v1/ports", None);
        assert!(absent.starts_with("HTTP/1.1 404"), "{absent}");
    }

    /// A configured token is enforced, and health stays reachable without it.
    ///
    /// Health has to answer before a caller is told it is not allowed in,
    /// otherwise "is anything there" and "may I in" give the same answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_token_is_enforced_but_health_is_not_behind_it() {
        let (_server, addr) = listening(Some("s3cret"));

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
        let (mut server, addr) = listening(None);
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
}
