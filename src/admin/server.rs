//! The accept loop, and the limits that keep an unauthenticated client from
//! taking the indexer down with it.
//!
//! # Why this is not `axum::serve`
//!
//! It was, and that was the security review's MAJOR 1. `axum::serve` sets no
//! timeout of any kind and no limit on how many connections it accepts, so
//! anyone who can reach the port could open sockets, send half a request
//! line and never finish: 4000 of them were still held 76 seconds later.
//! Those file descriptors belong to the process that indexes every chain,
//! so past its `RLIMIT_NOFILE` the ClickHouse client and the HyperSync
//! streams stop being able to open sockets and ALL INDEXING STOPS. No
//! password is needed for any of it.
//!
//! `axum::serve` does not expose hyper's connection settings, so the loop
//! below is hyper's own with four limits added. Each one answers a
//! different way of holding a connection:
//!
//! | Limit | The attack it answers |
//! |---|---|
//! | [`Limits::max_connections`] | opening more sockets than the process can afford. A connection with no permit is closed at once, so the cost of a flood is bounded by the cap and not by the attacker's patience |
//! | [`Limits::header_read`] | a request line that never ends (slowloris). hyper's own `header_read_timeout`, which is the only thing that can see a header that never arrives |
//! | [`Limits::request`] | a complete request whose body dribbles in, or a handler that hangs. Wraps the WHOLE connection |
//! | [`Limits::idle`] | a finished keep-alive connection held open for nothing |
//!
//! A connection also runs in its own task, so a panic anywhere in a handler
//! unwinds into that task and never reaches the supervisor.

use anyhow::{Context, Result};
use axum::Router;
use futures::future::BoxFuture;
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto::Builder,
    service::TowerToHyperService,
};
use log::{debug, warn};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Semaphore,
};
use tower::Service;

/// What a single client may cost the process.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Connections served at once. Further ones are closed immediately.
    /// The panel has ONE user; this is generous already.
    pub max_connections: usize,
    /// Time to finish sending the request head. This is the slowloris
    /// answer: only hyper can enforce it, because only hyper knows the
    /// head is still incomplete.
    pub header_read: Duration,
    /// Cap on a whole connection, head, body, handler and response.
    pub request: Duration,
    /// A keep-alive connection with nothing in flight is closed after this.
    pub idle: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 64,
            header_read: Duration::from_secs(10),
            request: Duration::from_secs(30),
            idle: Duration::from_secs(60),
        }
    }
}

/// Serves `app` on `listener` until `shutdown` resolves.
///
/// Never returns an error for a single bad connection: one client must not
/// be able to stop the panel, let alone the process it lives in.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    limits: Limits,
    shutdown: BoxFuture<'static, ()>,
) {
    // One permit per connection we are willing to hold. `acquire_owned`
    // moves the permit into the connection task, so it is returned when the
    // task ends however it ends - including by panic.
    let permits = Arc::new(Semaphore::new(limits.max_connections.max(1)));

    let mut app = app.into_make_service_with_connect_info::<SocketAddr>();
    tokio::pin!(shutdown);

    loop {
        let accepted = tokio::select! {
            _ = &mut shutdown => return,
            accepted = listener.accept() => accepted,
        };

        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(e) => {
                // Out of file descriptors, or the peer vanished between
                // the readiness and the accept. Pause rather than spin.
                warn!("Control panel: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        // Full: close this one NOW instead of queueing it. Dropping the
        // stream sends the FIN, so the attacker's socket does not stay
        // charged to this process.
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            debug!("Control panel is at its connection limit, dropping {peer}");
            drop(stream);
            continue;
        };

        // `app` is a MakeService: calling it is what attaches the peer
        // address that the login throttle keys on.
        let service = match app.call(peer).await {
            Ok(service) => service,
            Err(e) => {
                warn!("Control panel: could not build a service: {e:?}");
                continue;
            }
        };

        // Its own task: a panic in a handler unwinds here and the chain
        // supervisors never see it.
        tokio::spawn(async move {
            let _permit = permit;
            connection(stream, service, limits).await;
        });
    }
}

/// One connection, under every limit at once.
async fn connection<S>(stream: TcpStream, service: S, limits: Limits)
where
    S: Service<
            axum::http::Request<hyper::body::Incoming>,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    // Nagle off: the panel's responses are small and latency is what the
    // owner sees.
    let _ = stream.set_nodelay(true);

    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        // hyper needs a timer before it will honour a timeout, and it
        // panics inside the connection task if one is missing rather than
        // quietly not timing out. (That panic was how this was found; it
        // stayed inside the task, which is the isolation working.)
        .timer(TokioTimer::new())
        // The ONLY limit that can see a head that never arrives.
        .header_read_timeout(limits.header_read)
        .keep_alive(true);

    let served = builder.serve_connection_with_upgrades(
        TokioIo::new(stream),
        TowerToHyperService::new(service),
    );

    // And a hard cap on the whole thing, so a body that dribbles in one
    // byte at a time, or a handler that hangs, still ends.
    match tokio::time::timeout(limits.request.max(limits.idle), served)
        .await
    {
        Ok(Ok(())) => {}
        // A client that hangs up mid-request is ordinary, not a fault.
        Ok(Err(e)) => debug!("Control panel connection ended: {e}"),
        Err(_) => debug!("Control panel connection timed out"),
    }
}

/// Binds the panel's port, with the error an operator can act on.
pub async fn bind(addr: SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(addr).await.with_context(|| {
        format!(
            "cannot bind the control panel to {addr} (is the port already \
             in use?)"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_bound_a_client_in_every_dimension() {
        let limits = Limits::default();

        assert!(limits.max_connections > 0);
        assert!(limits.header_read <= Duration::from_secs(30));
        assert!(limits.request >= limits.header_read);
        assert!(limits.idle <= Duration::from_secs(300));
    }
}
