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
//! | [`Limits::max_per_address`] | one address taking every slot of that cap and leaving none for the owner (security re-check residual 1) |
//! | [`Limits::header_read`] | a request line that never ends (slowloris). hyper's own `header_read_timeout`, which is the only thing that can see a header that never arrives |
//! | [`Limits::request`] | a complete request whose body dribbles in, or a handler that hangs. Applied to ONE request, by a layer inside the router (`admin::router`) so that its 408 still carries the security headers |
//! | [`Limits::idle`] | the whole connection, which for a finished keep-alive connection is the time it may be held open for nothing |
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
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tower::Service;

/// What a single client may cost the process.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Connections served at once. Further ones are closed immediately.
    /// The panel has ONE user; this is generous already.
    pub max_connections: usize,
    /// Of those, how many one source address may hold.
    ///
    /// Without it the global cap is itself a way to lock the owner out:
    /// the re-check held 80 sockets from one address, and the owner's
    /// `GET /` got no answer at all until the header timeout cleared them
    /// (re-check residual 1). The indexing was never at risk - that is
    /// what the global cap fixed - but the panel was unreachable for as
    /// long as the attacker cared to keep re-opening.
    pub max_per_address: usize,
    /// Time to finish sending the request head. This is the slowloris
    /// answer: only hyper can enforce it, because only hyper knows the
    /// head is still incomplete.
    pub header_read: Duration,
    /// Cap on ONE request: head done, body read, handler, response.
    ///
    /// Enforced by a layer inside the router, not here. It used to be
    /// `max(request, idle)` around the whole connection, which is to say
    /// it was never the limit that applied and a dribbling body got the
    /// idle value instead (re-check residual 2).
    pub request: Duration,
    /// Cap on a whole connection, however many requests it carries. For a
    /// keep-alive connection with nothing in flight this is how long it
    /// may be held open for nothing.
    pub idle: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 64,
            max_per_address: 8,
            header_read: Duration::from_secs(10),
            request: Duration::from_secs(30),
            idle: Duration::from_secs(60),
        }
    }
}

/// The connection budget: a global cap with a per-address share inside it.
///
/// Both halves matter and neither replaces the other. The global cap is
/// what keeps a flood from costing the *indexer* its file descriptors; the
/// per-address share is what keeps one flooding address from costing the
/// *owner* the panel.
struct Slots {
    /// The global cap. An owned permit travels with the connection task,
    /// so it comes back however the task ends - including by panic.
    permits: Arc<Semaphore>,
    per_address: usize,
    /// How many connections each address is holding right now. An address
    /// is removed as soon as it drops to zero, so this map is never larger
    /// than the global cap.
    held: Arc<Mutex<HashMap<IpAddr, usize>>>,
}

/// One connection's place in the budget, given back on drop.
struct Slot {
    _permit: OwnedSemaphorePermit,
    address: IpAddr,
    held: Arc<Mutex<HashMap<IpAddr, usize>>>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut held = lock(&self.held);
        if let Some(count) = held.get_mut(&self.address) {
            *count -= 1;
            if *count == 0 {
                held.remove(&self.address);
            }
        }
    }
}

/// A poisoned lock here means some other connection task panicked while
/// holding it. The map is a counter, not an invariant, so carrying on with
/// it is right: refusing every connection because one task fell over would
/// be the outage this whole module exists to prevent.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Slots {
    fn new(limits: Limits) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(
                limits.max_connections.max(1),
            )),
            per_address: limits.max_per_address.max(1),
            held: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A place for a connection from `address`, or `None` when either cap
    /// is full. The global permit is taken first and dropped again if the
    /// address is over its share, so a refusal costs nothing.
    fn take(&self, address: IpAddr) -> Option<Slot> {
        let permit = self.permits.clone().try_acquire_owned().ok()?;

        {
            let mut held = lock(&self.held);
            let count = held.entry(address).or_insert(0);
            if *count >= self.per_address {
                return None;
            }
            *count += 1;
        }

        Some(Slot { _permit: permit, address, held: self.held.clone() })
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
    let slots = Slots::new(limits);

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

        // Full - globally, or for this address on its own. Close this one
        // NOW instead of queueing it: dropping the stream sends the FIN,
        // so the attacker's socket does not stay charged to this process.
        let Some(slot) = slots.take(peer.ip()) else {
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
            let _slot = slot;
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

    // And a hard cap on the whole connection, so a keep-alive socket that
    // is held open for nothing still ends. A single slow REQUEST is bounded
    // separately and much sooner by `Limits::request`, which is a layer
    // inside the router: this used to be `max(request, idle)`, i.e. the
    // idle value always won and the request limit never applied to anything
    // (re-check residual 2).
    match tokio::time::timeout(limits.idle, served).await {
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
        assert!(limits.max_per_address > 0);
        assert!(limits.max_per_address < limits.max_connections);
        assert!(limits.header_read <= Duration::from_secs(30));
        assert!(limits.request >= limits.header_read);
        assert!(limits.idle <= Duration::from_secs(300));
    }

    fn budget(max_connections: usize, max_per_address: usize) -> Slots {
        Slots::new(Limits {
            max_connections,
            max_per_address,
            ..Limits::default()
        })
    }

    fn address(last: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, last])
    }

    /// Re-check residual 1: with a global cap alone, one address could hold
    /// every slot and the owner's own request never got answered.
    #[test]
    fn one_address_can_not_take_every_slot() {
        let slots = budget(16, 4);

        let mut flood = Vec::new();
        for _ in 0..4 {
            flood.push(
                slots
                    .take(address(1))
                    .expect("under its own share of the cap"),
            );
        }

        assert!(
            slots.take(address(1)).is_none(),
            "one address went past its share of the connection cap"
        );
        assert!(
            slots.take(address(2)).is_some(),
            "the owner was starved out by another address"
        );
    }

    #[test]
    fn the_global_cap_still_applies_across_addresses() {
        let slots = budget(4, 4);

        let mut held = Vec::new();
        for last in 1..=4 {
            held.push(slots.take(address(last)).expect("under the cap"));
        }

        assert!(
            slots.take(address(5)).is_none(),
            "the global cap was exceeded"
        );
    }

    #[test]
    fn a_closed_connection_gives_its_slot_back() {
        let slots = budget(16, 2);

        let first = slots.take(address(1)).expect("under the cap");
        let second = slots.take(address(1)).expect("under the cap");
        assert!(slots.take(address(1)).is_none());

        drop(first);
        let third = slots.take(address(1)).expect("a slot came free");

        drop(second);
        drop(third);
        assert!(
            lock(&slots.held).is_empty(),
            "an address stayed in the table after its last connection went"
        );
    }

    /// A refused connection must not hold a global permit either, or an
    /// address over its share would eat the whole cap on the way out.
    #[test]
    fn a_refused_connection_costs_nothing() {
        let slots = budget(4, 1);

        let held = slots.take(address(1)).expect("under the cap");
        for _ in 0..50 {
            assert!(slots.take(address(1)).is_none());
        }

        assert!(
            slots.take(address(2)).is_some(),
            "refusals leaked permits out of the global cap"
        );
        drop(held);
    }
}
