//! Minimal HTTP/1.1 responder for `/metrics`, `/healthz` and `/readyz`.
//!
//! One short-lived task per connection, one request per connection
//! (`Connection: close`; Prometheus and kubelets are fine with that). A
//! client can hold at most `MAX_HEAD_BYTES` of memory for `request_timeout`
//! and there are at most `MAX_CONNECTIONS` clients, so a stuck or hostile
//! client cannot hold resources. Request bodies are never read.

use super::{encode::CONTENT_TYPE, Exposition, Metrics};
use anyhow::{Context, Result};
use log::{debug, info, warn};
use std::{future::Future, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
    time::timeout,
};

/// Request line plus headers. Scrapers send a few hundred bytes.
const MAX_HEAD_BYTES: usize = 8 * 1024;
/// Connections served at the same time; further ones are closed at once.
const MAX_CONNECTIONS: usize = 64;
/// Receiving the request head, and (separately) sending the response.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause after a failed `accept` (out of file descriptors and the like).
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

const TEXT_PLAIN: &str = "text/plain; charset=utf-8";

/// A bound, not yet serving, metrics endpoint. Splitting [`bind`] from
/// [`Server::run`] lets startup fail early on a taken port and tells the
/// caller which port `:0` resolved to.
pub struct Server {
    listener: TcpListener,
    /// One chain's `Metrics`, or a whole fleet's (`fleet::metrics`). The
    /// responder only ever calls `render` and `readiness`.
    metrics: Arc<dyn Exposition>,
    request_timeout: Duration,
}

/// Binds the metrics endpoint of ONE chain (`indexer run`).
pub async fn bind(addr: SocketAddr, metrics: Metrics) -> Result<Server> {
    bind_exposition(addr, Arc::new(metrics)).await
}

/// Binds the metrics endpoint over any [`Exposition`] - what `indexer
/// fleet` uses to serve every chain from one port.
pub async fn bind_exposition(
    addr: SocketAddr,
    metrics: Arc<dyn Exposition>,
) -> Result<Server> {
    let listener = TcpListener::bind(addr).await.with_context(|| {
        format!(
            "cannot bind the metrics endpoint to {addr} (is the port \
             already in use?)"
        )
    })?;

    Ok(Server { listener, metrics, request_timeout: REQUEST_TIMEOUT })
}

/// Binds `addr` and serves until `shutdown` completes.
pub async fn serve(
    addr: SocketAddr,
    metrics: Metrics,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    bind(addr, metrics).await?.run(shutdown).await
}

impl Server {
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.listener
            .local_addr()
            .context("metrics endpoint has no local address")
    }

    /// Time a client gets to send its request, and again to receive the
    /// response (default 5s).
    pub fn with_request_timeout(
        mut self,
        request_timeout: Duration,
    ) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Serves until `shutdown` completes; connections still open at that
    /// point are dropped.
    pub async fn run(
        self,
        shutdown: impl Future<Output = ()>,
    ) -> Result<()> {
        let Server { listener, metrics, request_timeout } = self;

        if let Ok(addr) = listener.local_addr() {
            info!("Metrics endpoint listening on http://{addr}/metrics");
        }

        // Dropping the set aborts whatever is still running.
        let mut connections = JoinSet::new();
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                _ = &mut shutdown => return Ok(()),
                // Reap finished connections. A panic in one of them (there
                // is no known way to cause one) must not stop the server.
                Some(finished) = connections.join_next() => {
                    if let Err(e) = finished {
                        warn!("Metrics connection task failed: {e}");
                    }
                }
                accepted = listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        if connections.len() >= MAX_CONNECTIONS {
                            debug!("Metrics endpoint busy, dropping {peer}");
                            continue;
                        }
                        connections.spawn(handle(
                            stream,
                            metrics.clone(),
                            request_timeout,
                        ));
                    }
                    Err(e) => {
                        warn!("Metrics endpoint accept failed: {e}");
                        tokio::time::sleep(ACCEPT_BACKOFF).await;
                    }
                },
            }
        }
    }
}

struct Response {
    status: &'static str,
    content_type: &'static str,
    /// Extra header line, CRLF included.
    extra_header: &'static str,
    body: String,
}

impl Response {
    fn new(status: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: TEXT_PLAIN,
            extra_header: "",
            body: body.into(),
        }
    }
}

enum Head {
    Complete(Vec<u8>),
    TooLarge,
    /// The client went away before finishing the request.
    Gone,
}

async fn handle(
    mut stream: TcpStream,
    metrics: Arc<dyn Exposition>,
    request_timeout: Duration,
) {
    let (response, head_only) =
        match timeout(request_timeout, read_head(&mut stream)).await {
            Ok(Head::Complete(head)) => respond(&head, metrics.as_ref()),
            Ok(Head::TooLarge) => (
                Response::new(
                    "431 Request Header Fields Too Large",
                    "request too large\n",
                ),
                false,
            ),
            Ok(Head::Gone) => return,
            Err(_) => (
                Response::new("408 Request Timeout", "request timeout\n"),
                false,
            ),
        };

    // The peer may be gone or not reading: nothing to do about either.
    let _ =
        timeout(request_timeout, write(&mut stream, response, head_only))
            .await;
}

/// Reads until the blank line that ends the headers.
async fn read_head(stream: &mut TcpStream) -> Head {
    let mut head = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];

    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return Head::Gone,
            Ok(n) => {
                // The terminator may straddle two reads.
                let scan_from = head.len().saturating_sub(3);
                head.extend_from_slice(&chunk[..n]);

                if ends_headers(&head[scan_from..]) {
                    return Head::Complete(head);
                }
                if head.len() >= MAX_HEAD_BYTES {
                    return Head::TooLarge;
                }
            }
        }
    }
}

fn ends_headers(bytes: &[u8]) -> bool {
    bytes.windows(4).any(|w| w == b"\r\n\r\n")
        || bytes.windows(2).any(|w| w == b"\n\n")
}

/// The response, and whether the body must be left out (`HEAD`).
fn respond(head: &[u8], metrics: &dyn Exposition) -> (Response, bool) {
    let bad_request =
        || (Response::new("400 Bad Request", "bad request\n"), false);

    let line_end =
        head.iter().position(|b| *b == b'\n').unwrap_or(head.len());
    let Ok(line) = std::str::from_utf8(&head[..line_end]) else {
        return bad_request();
    };

    let mut parts = line.trim_end_matches('\r').split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return bad_request();
    };

    if !version.starts_with("HTTP/1.") || !target.starts_with('/') {
        return bad_request();
    }

    let head_only = match method {
        "GET" => false,
        "HEAD" => true,
        _ => {
            let mut response = Response::new(
                "405 Method Not Allowed",
                "method not allowed\n",
            );
            response.extra_header = "Allow: GET, HEAD\r\n";
            return (response, false);
        }
    };

    let path = target.split(['?', '#']).next().unwrap_or(target);

    let response = match path {
        "/metrics" => {
            let mut response = Response::new("200 OK", metrics.render());
            response.content_type = CONTENT_TYPE;
            response
        }
        "/healthz" => Response::new("200 OK", "ok\n"),
        "/readyz" => match metrics.readiness() {
            Ok(()) => Response::new("200 OK", "ready\n"),
            Err(reason) => Response::new(
                "503 Service Unavailable",
                format!("{reason}\n"),
            ),
        },
        _ => Response::new(
            "404 Not Found",
            "not found; try /metrics, /healthz or /readyz\n",
        ),
    };

    (response, head_only)
}

async fn write(
    stream: &mut TcpStream,
    response: Response,
    head_only: bool,
) -> std::io::Result<()> {
    let mut bytes = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{}\
         Connection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len(),
        response.extra_header,
    )
    .into_bytes();

    if !head_only {
        bytes.extend_from_slice(response.body.as_bytes());
    }

    stream.write_all(&bytes).await?;
    stream.shutdown().await
}
