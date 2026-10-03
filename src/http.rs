//! Module http contains a HTTP service that exposes status information about the agent.

use std::sync;
use std::thread;

use crate::check;

pub struct Service<'a> {
    hostname: String,
    check_manager: &'a check::Manager<'a>,
}

impl<'a> Service<'a> {
    pub fn new(hostname: String, check_manager: &'a check::Manager) -> Self {
        Self {
            hostname,
            check_manager,
        }
    }
    pub fn start<'scope>(&'a self, scope: &'scope thread::Scope<'scope, 'a>, port: u16) -> Stopper {
        let server = sync::Arc::new(tiny_http::Server::http(format!("0.0.0.0:{port}")).unwrap());
        let server_cloned = server.clone();
        scope.spawn(move || {
            eprintln!("[http_service] listening for requests on port {port}");
            for request in server_cloned.incoming_requests() {
                match request.method() {
                    tiny_http::Method::Get => {}
                    _ => {
                        let response = tiny_http::Response::empty(tiny_http::StatusCode(405));
                        let _ = request.respond(response);
                        continue;
                    }
                }
                let path = request.url().split('?').next().unwrap_or("/");
                let (data, content_type) = match path {
                    "/" | "/index.html" => (self.index_html(), "text/html; charset=UTF-8"),
                    "/status.json" => (self.status_json(), "application/json; charset=UTF-8"),
                    "/metrics" => (
                        crate::metrics::get().render(),
                        "text/plain; version=0.0.4; charset=UTF-8",
                    ),
                    _ => {
                        let response = tiny_http::Response::empty(tiny_http::StatusCode(404));
                        let _ = request.respond(response);
                        continue;
                    }
                };
                let header = tiny_http::Header::from_bytes("Content-Type", content_type).unwrap();
                // Allows the aggregate UI on another host to fetch this agent's status.
                let cors =
                    tiny_http::Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap();
                let response = tiny_http::Response::from_string(data)
                    .with_header(header)
                    .with_header(cors);
                let _ = request.respond(response);
            }
        });
        Stopper { server }
    }
    fn index_html(&self) -> String {
        STATUS_DOT_HTML.replace("__AGENTS__", "[]")
    }
    fn status_json(&self) -> String {
        let data = Data {
            hostname: self.hostname.clone(),
            poll_interval_secs: self.check_manager.poll_interval().num_seconds(),
            checks: self.check_manager.checks(),
        };
        serde_json::to_string_pretty(&data).unwrap()
    }
}

pub struct Stopper {
    server: sync::Arc<tiny_http::Server>,
}

impl Stopper {
    pub fn stop(self) {
        eprintln!("[http_service] shutdown signal received");
        self.server.unblock();
        eprintln!("[http_service] unblocked listening thread; waiting to stop");
    }
}

pub fn start_ui<'scope>(
    scope: &'scope thread::Scope<'scope, '_>,
    port: u16,
    agents: Vec<String>,
) -> Stopper {
    let agents_json = serde_json::to_string(&agents).unwrap();
    let html = STATUS_DOT_HTML.replace("__AGENTS__", &agents_json);
    let server = sync::Arc::new(tiny_http::Server::http(format!("0.0.0.0:{port}")).unwrap());
    let server_cloned = server.clone();
    scope.spawn(move || {
        eprintln!("[http_service] ui-only server listening on port {port}");
        for request in server_cloned.incoming_requests() {
            let path = request.url().split('?').next().unwrap_or("/");
            let (status, body, content_type) = match (request.method(), path) {
                (tiny_http::Method::Get, "/" | "/index.html") => {
                    (200u16, html.clone(), "text/html; charset=UTF-8")
                }
                _ => (404, "Not found".to_string(), "text/plain"),
            };
            let header = tiny_http::Header::from_bytes("Content-Type", content_type).unwrap();
            let response = tiny_http::Response::from_string(body)
                .with_status_code(tiny_http::StatusCode(status))
                .with_header(header);
            let _ = request.respond(response);
        }
    });
    Stopper { server }
}

static STATUS_DOT_HTML: &str = include_str!("status.html");

#[derive(Debug, serde::Serialize)]
struct Data {
    hostname: String,
    poll_interval_secs: i64,
    checks: Vec<check::Check>,
}
