//! One [`EthCaller`] over several JSON-RPC endpoints, so that no single
//! node can stall, disable or **poison** token metadata.
//!
//! Availability:
//!
//! * every endpoint has its own circuit breaker (doubling cool-down) and is
//!   only used once it proved (`eth_chainId`) that it serves the indexed
//!   chain; an endpoint on another chain is set aside (and asked again an
//!   hour later) with a single error log while the others keep working;
//! * a call goes to the best endpoint first (configured before discovered,
//!   breaker closed, lowest EWMA latency; endpoints that were never
//!   measured go first so they all get explored) and rotates to the next
//!   one on a transient failure, within the same call;
//! * [`CallError::Execution`] is returned as is: the node executed the call
//!   and the EVM failed, which is a property of the contract;
//! * when every breaker is open the endpoint closest to the end of its
//!   cool-down is probed anyway, so with a single endpoint this behaves
//!   exactly like a plain caller and the pause policy stays where it was:
//!   in the fetcher's own breaker.
//!
//! Trust (tirith decision "Public RPC endpoints are untrusted"):
//!
//! * every answer names its [`Source`]: the endpoint, its *group* (the
//!   provider: `a.example.org` and `b.example.org` are one opinion) and
//!   whether it is trusted (configured) or not (discovered). The fetcher
//!   uses [`Route`]s to get second opinions from other groups;
//! * **freshness**: block heights are tracked per endpoint, both as
//!   reported by `eth_blockNumber` (refreshed about once a minute) and as
//!   seen inside `eth_call` (`Multicall3.getBlockNumber()`, reported by
//!   the fetcher with every aggregate). An endpoint more than
//!   `max_lag_blocks` behind the best height is put in cool-down and its
//!   answer discarded. "Best" is the indexer's own head, any configured
//!   endpoint, or the *second* highest discovered group: one public
//!   endpoint claiming an absurd height cannot disqualify the honest ones;
//! * endpoints that lose votes against two agreeing others go to
//!   cool-down, and are banned for an hour once they keep losing.

use std::{
    collections::{hash_map::DefaultHasher, HashMap, HashSet},
    hash::{Hash, Hasher},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex, RwLock,
    },
    time::Duration,
};

use alloy::primitives::{Address, Bytes};
use anyhow::Context;
use futures::future::{join_all, BoxFuture};
use log::{debug, error, info, warn};
use tokio::time::Instant;

use super::{
    breaker::CircuitBreaker,
    multicall::{
        CallError, CallerHealth, EmptyCheck, EthCaller, HeightKind,
        HttpCaller, Route, Routed, RoutedError, Source, Vote,
    },
};

/// Tunables of the [`MultiEndpointCaller`].
#[derive(Debug, Clone)]
pub struct EndpointOptions {
    /// An endpoint that failed is avoided for this long. Doubles on every
    /// consecutive failure.
    pub breaker_cooldown: Duration,
    /// Upper bound of the doubling cool-down.
    pub breaker_max_cooldown: Duration,
    /// Endpoints tried by one `call` before it reports the failure (the
    /// fetcher retries with backoff on top of this).
    pub max_attempts_per_call: usize,
    /// An endpoint this many blocks behind the best known height is
    /// stale: its answers are discarded.
    pub max_lag_blocks: u64,
    /// How often `eth_blockNumber` is asked of an endpoint in use.
    pub height_refresh: Duration,
    /// "Serves another chain" is checked again after this long.
    pub chain_recheck: Duration,
    /// Lost votes (within `vote_memory`) before an endpoint is banned.
    pub vote_ban_after: u32,
    pub vote_ban: Duration,
    pub vote_memory: Duration,
}

impl Default for EndpointOptions {
    fn default() -> Self {
        Self {
            breaker_cooldown: Duration::from_secs(30),
            breaker_max_cooldown: Duration::from_secs(300),
            max_attempts_per_call: 3,
            max_lag_blocks: 32,
            height_refresh: Duration::from_secs(60),
            chain_recheck: Duration::from_secs(3_600),
            vote_ban_after: 3,
            vote_ban: Duration::from_secs(3_600),
            vote_memory: Duration::from_secs(24 * 3_600),
        }
    }
}

/// One endpoint handed to the [`MultiEndpointCaller`].
pub struct EndpointSpec {
    /// Identity used to keep the state of an endpoint across
    /// [`MultiEndpointCaller::replace_discovered`]. Typically the URL:
    /// never logged.
    pub key: String,
    /// Name used in logs and error messages. Must not contain secrets.
    pub label: String,
    /// Who runs it (see [`provider_of`]): endpoints of one provider are
    /// one opinion. Never logged.
    pub provider: String,
    pub caller: Arc<dyn EthCaller>,
    /// Found by `--rpc auto` (replaceable, **untrusted**) rather than
    /// configured.
    pub discovered: bool,
}

/// The provider behind a URL, as well as it can be told without a public
/// suffix list: the last two labels of the host (`rpc.ankr.com` and
/// `eth.ankr.com` are both `ankr.com`). Erring on this side merges
/// unrelated hosts of a `co.uk`-style suffix, which only costs an
/// opinion; the other side would let one provider confirm itself.
pub fn provider_of(url: &str) -> String {
    let host = url::Url::parse(url.trim())
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase));

    let Some(host) = host else {
        return url.trim().to_string();
    };

    let host = host.trim_end_matches('.');
    if host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') {
        return host.to_string();
    }

    let labels: Vec<&str> = host.rsplit('.').take(2).collect();
    labels.into_iter().rev().collect::<Vec<_>>().join(".")
}

const CHAIN_UNCHECKED: u8 = 0;
const CHAIN_VERIFIED: u8 = 1;
const CHAIN_MISMATCH: u8 = 2;

/// Floor between two `eth_blockNumber` requests to one endpoint.
const MIN_HEIGHT_REFRESH: Duration = Duration::from_secs(5);

static NEXT_ENDPOINT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct Votes {
    lost: u32,
    last_lost: Option<Instant>,
    banned_until: Option<Instant>,
}

struct Endpoint {
    id: u64,
    group: u64,
    key: String,
    label: String,
    caller: Arc<dyn EthCaller>,
    discovered: bool,
    chain_state: AtomicU8,
    observed_chain_id: AtomicU64,
    mismatch_at: Mutex<Option<Instant>>,
    chain_recheck: Duration,
    breaker: CircuitBreaker,
    /// EWMA of the request latency in microseconds, 0 = never measured.
    latency_micros: AtomicU64,
    down: AtomicBool,
    /// Last heights seen, 0 = never.
    evm_height: AtomicU64,
    rpc_height: AtomicU64,
    rpc_height_checked: Mutex<Option<Instant>>,
    votes: Mutex<Votes>,
}

enum Verification {
    Verified,
    WrongChain,
    Unavailable(String),
}

enum Attempt {
    Returned(Bytes),
    /// The node is fine, the contract is not.
    Execution(String),
    /// Transient failure (the breaker of the endpoint is now open).
    Failed(String),
    WrongChain,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Endpoint {
    fn new(spec: EndpointSpec, options: &EndpointOptions) -> Self {
        let mut hasher = DefaultHasher::new();
        spec.provider.hash(&mut hasher);

        Self {
            id: NEXT_ENDPOINT_ID.fetch_add(1, Ordering::Relaxed),
            group: hasher.finish(),
            key: spec.key,
            label: spec.label,
            caller: spec.caller,
            discovered: spec.discovered,
            chain_state: AtomicU8::new(CHAIN_UNCHECKED),
            observed_chain_id: AtomicU64::new(0),
            mismatch_at: Mutex::new(None),
            chain_recheck: options.chain_recheck,
            breaker: CircuitBreaker::new(
                options.breaker_cooldown,
                options.breaker_max_cooldown,
            ),
            latency_micros: AtomicU64::new(0),
            down: AtomicBool::new(false),
            evm_height: AtomicU64::new(0),
            rpc_height: AtomicU64::new(0),
            rpc_height_checked: Mutex::new(None),
            votes: Mutex::new(Votes::default()),
        }
    }

    fn source(&self) -> Source {
        Source {
            id: self.id,
            group: self.group,
            trusted: !self.discovered,
        }
    }

    /// The verdict expires: a load balancer may have routed one request
    /// to the wrong network, the operator may have fixed the node.
    fn wrong_chain(&self) -> bool {
        if self.chain_state.load(Ordering::Relaxed) != CHAIN_MISMATCH {
            return false;
        }

        let expired = lock(&self.mismatch_at)
            .is_none_or(|at| at.elapsed() >= self.chain_recheck);
        if expired {
            self.chain_state.store(CHAIN_UNCHECKED, Ordering::Relaxed);
        }
        !expired
    }

    fn verified(&self) -> bool {
        self.chain_state.load(Ordering::Relaxed) == CHAIN_VERIFIED
    }

    /// Banned for losing votes.
    fn banned(&self) -> bool {
        lock(&self.votes)
            .banned_until
            .is_some_and(|until| Instant::now() < until)
    }

    fn usable(&self) -> bool {
        !self.wrong_chain() && !self.banned()
    }

    /// Usable and its last request worked. An endpoint whose cool-down
    /// ended stays unhealthy until it proves itself again.
    fn healthy(&self) -> bool {
        self.usable() && !self.down.load(Ordering::Relaxed)
    }

    fn height(&self, kind: HeightKind) -> u64 {
        match kind {
            HeightKind::Evm => self.evm_height.load(Ordering::Relaxed),
            HeightKind::Rpc => self.rpc_height.load(Ordering::Relaxed),
        }
    }

    fn record_latency(&self, started: Instant) {
        let sample = u64::try_from(started.elapsed().as_micros())
            .unwrap_or(u64::MAX)
            .max(1);
        let previous = self.latency_micros.load(Ordering::Relaxed);
        let next = if previous == 0 {
            sample
        } else {
            // alpha = 1/5
            (previous / 5).saturating_mul(4).saturating_add(sample / 5)
        };
        self.latency_micros.store(next.max(1), Ordering::Relaxed);
    }

    fn succeeded(&self) {
        self.breaker.reset();
        if self.down.swap(false, Ordering::Relaxed) {
            debug!("RPC endpoint {} recovered", self.label);
        }
    }

    fn failed(&self, error: &str) {
        self.down.store(true, Ordering::Relaxed);
        if let Some(cooldown) = self.breaker.trip() {
            debug!(
                "RPC endpoint {} failed, avoiding it for {cooldown:?}: \
                 {error}",
                self.label
            );
        }
    }

    /// Makes sure the endpoint serves `expected` before it is used.
    async fn verify(&self, expected: u64) -> Verification {
        if self.wrong_chain() {
            return Verification::WrongChain;
        }
        if self.verified() {
            return Verification::Verified;
        }

        // Latency is only measured on `eth_call`s (comparable requests).
        match self.caller.chain_id().await {
            Ok(actual) if actual == expected => {
                self.chain_state.store(CHAIN_VERIFIED, Ordering::Relaxed);
                Verification::Verified
            }
            Ok(actual) => {
                let first =
                    self.observed_chain_id.swap(actual, Ordering::Relaxed)
                        != actual;
                *lock(&self.mismatch_at) = Some(Instant::now());
                self.chain_state.store(CHAIN_MISMATCH, Ordering::Relaxed);

                if first {
                    error!(
                        "RPC endpoint {} serves chain {actual} but chain \
                         {expected} is being indexed: it is not used \
                         (checked again in {:?})",
                        self.label, self.chain_recheck
                    );
                }
                Verification::WrongChain
            }
            Err(
                CallError::Transient(error) | CallError::Execution(error),
            ) => {
                self.failed(&error);
                Verification::Unavailable(error)
            }
        }
    }
}

/// Result of [`MultiEndpointCaller::verify_all`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VerifySummary {
    pub verified: usize,
    pub wrong_chain: usize,
    /// Could not be asked; verified again before their first use.
    pub unreachable: usize,
}

/// See the [module documentation](self).
pub struct MultiEndpointCaller {
    expected_chain_id: u64,
    options: EndpointOptions,
    endpoints: RwLock<Arc<Vec<Arc<Endpoint>>>>,
    /// The set of endpoints may still change (`--rpc auto`), so "every
    /// endpoint serves another chain" is not a final verdict.
    refreshable: AtomicBool,
    /// Highest block the indexer itself has seen (`HeightKind::Rpc`).
    head_hint: AtomicU64,
}

impl MultiEndpointCaller {
    pub fn new(
        expected_chain_id: u64,
        endpoints: Vec<EndpointSpec>,
        options: EndpointOptions,
    ) -> Self {
        let endpoints = dedupe(endpoints)
            .into_iter()
            .map(|spec| Arc::new(Endpoint::new(spec, &options)))
            .collect();

        Self {
            expected_chain_id,
            options,
            endpoints: RwLock::new(Arc::new(endpoints)),
            refreshable: AtomicBool::new(false),
            head_hint: AtomicU64::new(0),
        }
    }

    /// Configured (trusted) endpoints over HTTP(S) JSON-RPC URLs. Only an
    /// invalid URL is an error (it names the position of the entry, never
    /// the URL itself).
    pub fn from_urls<S: AsRef<str>>(
        expected_chain_id: u64,
        urls: &[S],
        call_timeout: Duration,
        options: EndpointOptions,
    ) -> anyhow::Result<Self> {
        let specs = urls
            .iter()
            .enumerate()
            .map(|(index, url)| {
                url_endpoint(url.as_ref(), index + 1, false, call_timeout)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(Self::new(expected_chain_id, specs, options))
    }

    /// Marks the endpoint set as replaceable, see
    /// [`replace_discovered`](Self::replace_discovered).
    pub fn set_refreshable(&self, refreshable: bool) {
        self.refreshable.store(refreshable, Ordering::Relaxed);
    }

    fn endpoints(&self) -> Arc<Vec<Arc<Endpoint>>> {
        self.endpoints.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn len(&self) -> usize {
        self.endpoints().len()
    }

    pub fn is_empty(&self) -> bool {
        self.endpoints().is_empty()
    }

    /// Replaces the discovered endpoints, keeping the configured ones and
    /// the state (chain check, breaker, latency, votes) of the discovered
    /// endpoints that are still listed.
    pub fn replace_discovered(&self, specs: Vec<EndpointSpec>) {
        let current = self.endpoints();

        let mut next: Vec<Arc<Endpoint>> = current
            .iter()
            .filter(|endpoint| !endpoint.discovered)
            .cloned()
            .collect();
        let mut keys: HashSet<String> =
            next.iter().map(|endpoint| endpoint.key.clone()).collect();

        for spec in specs {
            if !keys.insert(spec.key.clone()) {
                continue;
            }

            let kept =
                current.iter().find(|endpoint| endpoint.key == spec.key);

            next.push(match kept {
                Some(endpoint) => endpoint.clone(),
                None => Arc::new(Endpoint::new(
                    EndpointSpec { discovered: true, ..spec },
                    &self.options,
                )),
            });
        }

        *self.endpoints.write().unwrap_or_else(|e| e.into_inner()) =
            Arc::new(next);
    }

    /// Checks the chain id of every endpoint that was not checked yet,
    /// concurrently. Meant for startup; endpoints that cannot be reached
    /// are checked again before their first use.
    pub async fn verify_all(&self) -> VerifySummary {
        let endpoints = self.endpoints();
        let results = join_all(
            endpoints
                .iter()
                .map(|endpoint| endpoint.verify(self.expected_chain_id)),
        )
        .await;

        let mut summary = VerifySummary::default();
        for result in results {
            match result {
                Verification::Verified => summary.verified += 1,
                Verification::WrongChain => summary.wrong_chain += 1,
                Verification::Unavailable(_) => summary.unreachable += 1,
            }
        }
        summary
    }

    /// Usable endpoints the route admits, best first: configured before
    /// discovered, closed breakers by latency, then the open breaker that
    /// closes the soonest as a last resort.
    fn candidates(&self, route: &Route) -> Vec<Arc<Endpoint>> {
        let endpoints = self.endpoints();

        let (mut closed, open): (Vec<_>, Vec<_>) = endpoints
            .iter()
            .filter(|endpoint| {
                endpoint.usable() && route.admits(&endpoint.source())
            })
            .cloned()
            .partition(|endpoint| !endpoint.breaker.is_open());

        // Stable: ties keep the configured order.
        closed.sort_by_key(|endpoint| {
            (
                endpoint.discovered,
                endpoint.latency_micros.load(Ordering::Relaxed),
            )
        });

        let last_resort = open.into_iter().min_by_key(|endpoint| {
            (endpoint.discovered, endpoint.breaker.open_until())
        });
        closed.extend(last_resort);

        closed
    }

    /// The height an endpoint must keep up with.
    fn best_height(&self, kind: HeightKind) -> u64 {
        let mut best = match kind {
            HeightKind::Rpc => self.head_hint.load(Ordering::Relaxed),
            HeightKind::Evm => 0,
        };

        // Untrusted heights count per provider, and only the second
        // highest: nobody gets to move the bar on their own.
        let mut providers: HashMap<u64, u64> = HashMap::new();

        for endpoint in self.endpoints().iter() {
            let height = endpoint.height(kind);
            if height == 0 || !endpoint.usable() {
                continue;
            }

            if endpoint.discovered {
                let known = providers.entry(endpoint.group).or_default();
                *known = (*known).max(height);
            } else {
                best = best.max(height);
            }
        }

        let mut heights: Vec<u64> = providers.into_values().collect();
        heights.sort_unstable_by(|a, b| b.cmp(a));
        if let Some(second) = heights.get(1) {
            best = best.max(*second);
        }

        best
    }

    /// Records the height of an endpoint; `false` (and a cool-down) when
    /// that is too far behind.
    fn observe(
        &self,
        endpoint: &Endpoint,
        kind: HeightKind,
        height: u64,
    ) -> bool {
        match kind {
            HeightKind::Evm => &endpoint.evm_height,
            HeightKind::Rpc => &endpoint.rpc_height,
        }
        .store(height, Ordering::Relaxed);

        let best = self.best_height(kind);
        if height.saturating_add(self.options.max_lag_blocks) >= best {
            return true;
        }

        endpoint.failed(&format!(
            "it is at block {height}, {} blocks behind",
            best - height
        ));
        false
    }

    async fn execute(
        &self,
        endpoint: &Endpoint,
        to: Address,
        data: Bytes,
        quiet: bool,
    ) -> Attempt {
        match endpoint.verify(self.expected_chain_id).await {
            Verification::Verified => {}
            Verification::WrongChain => return Attempt::WrongChain,
            Verification::Unavailable(error) => {
                return Attempt::Failed(error)
            }
        }

        // Known to be behind and asked about it a moment ago: still is.
        let behind = {
            let height = endpoint.height(HeightKind::Rpc);
            height > 0
                && height.saturating_add(self.options.max_lag_blocks)
                    < self.best_height(HeightKind::Rpc)
        };
        let height_age = lock(&endpoint.rpc_height_checked)
            .map(|at| at.elapsed())
            .unwrap_or(Duration::MAX);

        if behind && height_age < MIN_HEIGHT_REFRESH {
            return Attempt::Failed("stale node".to_string());
        }

        let started = Instant::now();
        let result = endpoint.caller.call(to, data).await;
        endpoint.record_latency(started);

        let attempt = match result {
            Ok(returned) => Attempt::Returned(returned),
            // The node is alive and executed the call.
            Err(CallError::Execution(error)) => Attempt::Execution(error),
            Err(CallError::Transient(error)) => {
                if !quiet {
                    endpoint.failed(&error);
                }
                return Attempt::Failed(error);
            }
        };

        // An answer is only as good as the state it was read from. The
        // height is refreshed about once a minute, sooner when the last
        // one known is behind.
        let due = behind || height_age >= self.options.height_refresh;

        if due {
            match endpoint.caller.block_number().await {
                Ok(Some(height)) => {
                    *lock(&endpoint.rpc_height_checked) =
                        Some(Instant::now());
                    if !self.observe(endpoint, HeightKind::Rpc, height) {
                        return Attempt::Failed(format!(
                            "stale node (block {height})"
                        ));
                    }
                }
                // The backend cannot tell: nothing to compare.
                Ok(None) | Err(CallError::Execution(_)) => {
                    *lock(&endpoint.rpc_height_checked) =
                        Some(Instant::now());
                }
                Err(CallError::Transient(error)) => {
                    endpoint.failed(&error);
                    return Attempt::Failed(error);
                }
            }
        }

        endpoint.succeeded();
        attempt
    }

    fn unavailable(&self) -> String {
        format!(
            "no RPC endpoint is available for chain {}",
            self.expected_chain_id
        )
    }
}

impl EthCaller for MultiEndpointCaller {
    fn call(
        &self,
        to: Address,
        data: Bytes,
    ) -> BoxFuture<'_, Result<Bytes, CallError>> {
        Box::pin(async move {
            static ANYBODY: Route = Route {
                only: None,
                exclude_groups: Vec::new(),
                quiet: false,
            };

            self.call_routed(to, data, &ANYBODY)
                .await
                .map(|routed| routed.data)
                .map_err(|failure| failure.error)
        })
    }

    fn call_routed<'a>(
        &'a self,
        to: Address,
        data: Bytes,
        route: &'a Route,
    ) -> BoxFuture<'a, Result<Routed, RoutedError>> {
        Box::pin(async move {
            let candidates = self.candidates(route);
            if candidates.is_empty() {
                return Err(RoutedError::no_source(&self.unavailable()));
            }

            let mut attempts = 0usize;
            let mut last_error = None;

            for endpoint in candidates {
                if attempts >= self.options.max_attempts_per_call.max(1) {
                    break;
                }

                let source = endpoint.source();
                match self
                    .execute(&endpoint, to, data.clone(), route.quiet)
                    .await
                {
                    Attempt::Returned(data) => {
                        return Ok(Routed { data, source })
                    }
                    // Not failed over: every node would say the same
                    // (and if this one lies, the vote will tell).
                    Attempt::Execution(error) => {
                        return Err(RoutedError {
                            error: CallError::Execution(error),
                            source: Some(source),
                            no_source: false,
                        })
                    }
                    Attempt::Failed(error) => {
                        attempts += 1;
                        last_error = Some((
                            format!("{}: {error}", endpoint.label),
                            source,
                        ));
                    }
                    Attempt::WrongChain => {}
                }
            }

            Err(match last_error {
                Some((error, source)) => RoutedError {
                    error: CallError::Transient(error),
                    source: Some(source),
                    no_source: false,
                },
                None => RoutedError::no_source(&self.unavailable()),
            })
        })
    }

    /// The indexed chain id as soon as one endpoint serves it. Another
    /// chain id only when *every* endpoint serves another chain and the
    /// set is final, which is what disables token metadata.
    fn chain_id(&self) -> BoxFuture<'_, Result<u64, CallError>> {
        Box::pin(async move {
            let endpoints = self.endpoints();

            if endpoints.iter().any(|endpoint| endpoint.verified()) {
                return Ok(self.expected_chain_id);
            }

            let results =
                join_all(endpoints.iter().map(|endpoint| {
                    endpoint.verify(self.expected_chain_id)
                }))
                .await;

            let mut last_error = None;
            for result in results {
                match result {
                    Verification::Verified => {
                        return Ok(self.expected_chain_id)
                    }
                    Verification::Unavailable(error) => {
                        last_error = Some(error)
                    }
                    Verification::WrongChain => {}
                }
            }

            if let Some(error) = last_error {
                return Err(CallError::Transient(error));
            }

            match endpoints.first() {
                Some(endpoint)
                    if !self.refreshable.load(Ordering::Relaxed) =>
                {
                    Ok(endpoint.observed_chain_id.load(Ordering::Relaxed))
                }
                _ => Err(CallError::Transient(self.unavailable())),
            }
        })
    }

    /// Multicall3 (or any contract) being absent is a property of the
    /// chain, not of the endpoint that said so. Every usable endpoint is
    /// asked the same call:
    ///
    /// * any endpoint returning data refutes it (the endpoints that
    ///   answered empty are put in cool-down: they lag or are broken);
    /// * it is confirmed by two providers agreeing with nobody
    ///   disagreeing (one when there is only one);
    /// * anything else is undecided, and the caller must not remember it.
    fn confirm_empty(
        &self,
        to: Address,
        data: Bytes,
    ) -> BoxFuture<'_, EmptyCheck> {
        Box::pin(async move {
            let endpoints: Vec<_> = self
                .endpoints()
                .iter()
                .filter(|endpoint| endpoint.usable())
                .cloned()
                .collect();

            let answers = join_all(endpoints.iter().map(|endpoint| {
                self.execute(endpoint, to, data.clone(), false)
            }))
            .await;

            let mut empty = Vec::new();
            let mut refuted = None;
            let mut providers = HashSet::new();

            for (endpoint, answer) in endpoints.iter().zip(answers) {
                if !matches!(answer, Attempt::WrongChain) {
                    providers.insert(endpoint.group);
                }
                match answer {
                    Attempt::Returned(returned) if returned.is_empty() => {
                        empty.push(endpoint)
                    }
                    Attempt::Returned(returned) => {
                        refuted.get_or_insert(returned);
                    }
                    _ => {}
                }
            }

            if let Some(returned) = refuted {
                for endpoint in empty {
                    endpoint.failed(
                        "answered with no data for a contract that \
                         another endpoint has",
                    );
                }
                return EmptyCheck::Refuted(returned);
            }

            let agreeing: HashSet<u64> =
                empty.iter().map(|endpoint| endpoint.group).collect();

            if !agreeing.is_empty()
                && agreeing.len() >= providers.len().min(2)
            {
                EmptyCheck::Confirmed
            } else {
                EmptyCheck::Undecided
            }
        })
    }

    fn health(&self) -> Option<CallerHealth> {
        let endpoints = self.endpoints();
        let count = |test: fn(&Endpoint) -> bool| {
            endpoints.iter().filter(|endpoint| test(endpoint)).count()
        };

        Some(CallerHealth {
            endpoints_total: endpoints.len(),
            endpoints_healthy: count(Endpoint::healthy),
            endpoints_wrong_chain: count(Endpoint::wrong_chain),
            endpoints_distrusted: count(Endpoint::banned),
        })
    }

    fn source_count(&self) -> usize {
        self.endpoints()
            .iter()
            .filter(|endpoint| endpoint.usable())
            .map(|endpoint| endpoint.group)
            .collect::<HashSet<_>>()
            .len()
    }

    fn observe_height(
        &self,
        source: Source,
        kind: HeightKind,
        height: u64,
    ) -> bool {
        let endpoints = self.endpoints();
        match endpoints.iter().find(|endpoint| endpoint.id == source.id) {
            Some(endpoint) => self.observe(endpoint, kind, height),
            // Replaced meanwhile: nothing to hold against it.
            None => true,
        }
    }

    fn head_hint(&self, block: u64) {
        self.head_hint.fetch_max(block, Ordering::Relaxed);
    }

    fn report_vote(&self, source: Source, vote: Vote) {
        if vote == Vote::Won {
            return;
        }

        let endpoints = self.endpoints();
        let Some(endpoint) =
            endpoints.iter().find(|endpoint| endpoint.id == source.id)
        else {
            return;
        };

        endpoint.failed("two other endpoints agree on a different answer");

        let now = Instant::now();
        let mut votes = lock(&endpoint.votes);
        let recent = votes.last_lost.is_some_and(|at| {
            now.saturating_duration_since(at) < self.options.vote_memory
        });
        votes.lost = if recent { votes.lost.saturating_add(1) } else { 1 };
        votes.last_lost = Some(now);

        if votes.lost >= self.options.vote_ban_after.max(1) {
            votes.banned_until = Some(now + self.options.vote_ban);
            warn!(
                "RPC endpoint {} keeps contradicting the other endpoints \
                 ({} times): it is not used for {:?}",
                endpoint.label, votes.lost, self.options.vote_ban
            );
        }
    }
}

fn dedupe(specs: Vec<EndpointSpec>) -> Vec<EndpointSpec> {
    let mut keys = HashSet::new();
    specs
        .into_iter()
        .filter(|spec| keys.insert(spec.key.clone()))
        .collect()
}

/// An [`EndpointSpec`] over an HTTP(S) URL. `position` (1 based) names it
/// in logs: the URL of a configured endpoint may embed an API key, so only
/// public (discovered) endpoints show their host.
pub fn url_endpoint(
    url: &str,
    position: usize,
    discovered: bool,
    call_timeout: Duration,
) -> anyhow::Result<EndpointSpec> {
    let url = url.trim();

    let caller = if discovered {
        HttpCaller::public(url, call_timeout)
    } else {
        HttpCaller::new(url, call_timeout)
    }
    .with_context(|| format!("rpc endpoint #{position}"))?;

    Ok(spec_for_url(url, position, discovered, Arc::new(caller)))
}

/// The [`EndpointSpec`] of a URL over any caller (tests, custom stacks).
pub fn spec_for_url(
    url: &str,
    position: usize,
    discovered: bool,
    caller: Arc<dyn EthCaller>,
) -> EndpointSpec {
    let url = url.trim();

    let host = if discovered {
        url::Url::parse(url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
    } else {
        None
    };

    let label = match host {
        Some(host) => format!("#{position} ({host})"),
        None => format!("#{position}"),
    };

    EndpointSpec {
        key: url.trim_end_matches('/').to_string(),
        label,
        provider: provider_of(url),
        caller,
        discovered,
    }
}

/// Logs what the startup verification found.
pub(crate) fn log_summary(summary: VerifySummary, total: usize) {
    info!(
        "Token metadata RPC: {} of {total} endpoints verified ({} on \
         another chain, {} unreachable for now)",
        summary.verified, summary.wrong_chain, summary.unreachable
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::{
        multicall::{
            testing::{fast_options, FakeChain, FakeToken},
            MetadataFetcher, MULTICALL3_ADDRESS,
        },
        TokenStandard,
    };

    fn addr(n: u64) -> Address {
        Address::left_padding_from(&n.to_be_bytes())
    }

    /// A fake node with a fixed latency (under `tokio::time::pause`).
    struct Slow {
        chain: Arc<FakeChain>,
        latency: Duration,
    }

    impl EthCaller for Slow {
        fn call(
            &self,
            to: Address,
            data: Bytes,
        ) -> BoxFuture<'_, Result<Bytes, CallError>> {
            Box::pin(async move {
                tokio::time::sleep(self.latency).await;
                self.chain.call(to, data).await
            })
        }

        fn chain_id(&self) -> BoxFuture<'_, Result<u64, CallError>> {
            self.chain.chain_id()
        }
    }

    fn node() -> Arc<FakeChain> {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("USD Coin", "USDC", 6));
        chain.add(addr(9), FakeToken::reverting());
        chain
    }

    fn spec(n: usize, caller: Arc<dyn EthCaller>) -> EndpointSpec {
        EndpointSpec {
            key: format!("node-{n}"),
            label: format!("#{n}"),
            provider: format!("provider-{n}"),
            caller,
            discovered: false,
        }
    }

    fn pool(
        nodes: &[Arc<FakeChain>],
        options: EndpointOptions,
    ) -> MultiEndpointCaller {
        let specs = nodes
            .iter()
            .enumerate()
            .map(|(n, node)| spec(n + 1, node.clone()))
            .collect();
        MultiEndpointCaller::new(1, specs, options)
    }

    fn options() -> EndpointOptions {
        EndpointOptions {
            breaker_cooldown: Duration::from_secs(30),
            breaker_max_cooldown: Duration::from_secs(120),
            max_attempts_per_call: 3,
            ..EndpointOptions::default()
        }
    }

    fn attempts(node: &FakeChain) -> usize {
        node.attempts.load(Ordering::SeqCst)
    }

    fn name_call() -> Bytes {
        Bytes::from(vec![0x06, 0xfd, 0xde, 0x03])
    }

    #[tokio::test(start_paused = true)]
    async fn fails_over_within_the_same_call() {
        let nodes = [node(), node(), node()];
        nodes[0].offline.store(true, Ordering::SeqCst);
        let pool = pool(&nodes, options());

        let returned = pool.call(addr(1), name_call()).await;
        assert!(returned.is_ok_and(|data| !data.is_empty()));
        // The first endpoint never got past its chain id check, the
        // second one answered, the third was not needed.
        assert_eq!(attempts(&nodes[0]), 0);
        assert_eq!(nodes[0].chain_id_calls.load(Ordering::SeqCst), 1);
        assert_eq!(attempts(&nodes[1]), 1);
        assert_eq!(attempts(&nodes[2]), 0);

        let health = pool.health().unwrap();
        assert_eq!(health.endpoints_total, 3);
        assert_eq!(health.endpoints_healthy, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_endpoint_is_avoided_until_its_cooldown_ends() {
        let nodes = [node(), node()];
        let pool = pool(&nodes, options());
        assert_eq!(pool.verify_all().await.verified, 2);

        nodes[0].offline.store(true, Ordering::SeqCst);
        assert!(pool.call(addr(1), name_call()).await.is_ok());
        assert_eq!((attempts(&nodes[0]), attempts(&nodes[1])), (1, 1));

        // Breaker open: endpoint 1 is not even tried, although it is back.
        nodes[0].offline.store(false, Ordering::SeqCst);
        for _ in 0..5 {
            assert!(pool.call(addr(1), name_call()).await.is_ok());
        }
        assert_eq!((attempts(&nodes[0]), attempts(&nodes[1])), (1, 6));
        assert_eq!(pool.health().unwrap().endpoints_healthy, 1);

        // Cool-down over: a candidate again (first in line, even), but
        // not reported healthy before it proves itself.
        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(pool
            .candidates(&Route::default())
            .iter()
            .all(|e| !e.breaker.is_open()));
        assert_eq!(pool.candidates(&Route::default()).len(), 2);
        assert_eq!(pool.health().unwrap().endpoints_healthy, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn breaker_cooldown_doubles_per_endpoint() {
        let nodes = [node(), node()];
        // Endpoint 1 is the preferred (fastest) one, so it is tried again
        // as soon as its breaker closes.
        let specs = nodes
            .iter()
            .zip([10u64, 50])
            .enumerate()
            .map(|(n, (chain, latency))| {
                spec(
                    n + 1,
                    Arc::new(Slow {
                        chain: chain.clone(),
                        latency: Duration::from_millis(latency),
                    }),
                )
            })
            .collect();
        let pool = MultiEndpointCaller::new(1, specs, options());
        pool.verify_all().await;
        assert!(pool.call(addr(1), name_call()).await.is_ok());
        assert!(pool.call(addr(1), name_call()).await.is_ok());
        nodes[0].offline.store(true, Ordering::SeqCst);

        // 30s, then 60s, then 120s (max) of being avoided.
        for cooldown in [30u64, 60, 120, 120] {
            let before = attempts(&nodes[0]);
            assert!(pool.call(addr(1), name_call()).await.is_ok());
            let failed = attempts(&nodes[0]);
            assert_eq!(failed, before + 1, "{cooldown}");

            tokio::time::advance(Duration::from_secs(cooldown - 1)).await;
            assert!(pool.call(addr(1), name_call()).await.is_ok());
            assert_eq!(attempts(&nodes[0]), failed, "{cooldown}");

            tokio::time::advance(Duration::from_secs(2)).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn prefers_the_fastest_endpoint_after_exploring_them_all() {
        let chains = [node(), node(), node()];
        let latencies = [300u64, 20, 150];
        let specs = chains
            .iter()
            .zip(latencies)
            .enumerate()
            .map(|(n, (chain, latency))| {
                spec(
                    n + 1,
                    Arc::new(Slow {
                        chain: chain.clone(),
                        latency: Duration::from_millis(latency),
                    }),
                )
            })
            .collect();
        let pool = MultiEndpointCaller::new(1, specs, options());

        for _ in 0..20 {
            assert!(pool.call(addr(1), name_call()).await.is_ok());
        }

        // Each endpoint was measured, then the fastest one got the rest.
        assert_eq!(attempts(&chains[0]), 1);
        assert_eq!(attempts(&chains[2]), 1);
        assert_eq!(attempts(&chains[1]), 18);

        // It dies: the second fastest takes over.
        chains[1].offline.store(true, Ordering::SeqCst);
        for _ in 0..5 {
            assert!(pool.call(addr(1), name_call()).await.is_ok());
        }
        assert_eq!(attempts(&chains[2]), 6);
        assert_eq!(attempts(&chains[0]), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn wrong_chain_endpoint_is_disabled_and_the_others_work() {
        let nodes = [node(), node()];
        nodes[0].chain_id.store(56, Ordering::SeqCst);
        let pool = pool(&nodes, options());

        for _ in 0..3 {
            assert!(pool.call(addr(1), name_call()).await.is_ok());
        }
        // Asked for its chain id once, never called, never asked again.
        assert_eq!(nodes[0].chain_id_calls.load(Ordering::SeqCst), 1);
        assert_eq!(attempts(&nodes[0]), 0);
        assert_eq!(pool.chain_id().await, Ok(1));

        let health = pool.health().unwrap();
        assert_eq!(health.endpoints_wrong_chain, 1);
        assert_eq!(health.endpoints_healthy, 1);

        // Left alone for an hour...
        tokio::time::advance(Duration::from_secs(3_500)).await;
        assert!(pool.call(addr(1), name_call()).await.is_ok());
        assert_eq!(nodes[0].chain_id_calls.load(Ordering::SeqCst), 1);

        // ...then asked again: still wrong, still set aside.
        tokio::time::advance(Duration::from_secs(200)).await;
        assert!(pool.call(addr(1), name_call()).await.is_ok());
        assert_eq!(nodes[0].chain_id_calls.load(Ordering::SeqCst), 2);
        assert_eq!(attempts(&nodes[0]), 0);
        assert_eq!(pool.health().unwrap().endpoints_wrong_chain, 1);

        // The operator fixes the node: back in service an hour later,
        // without a restart.
        nodes[0].chain_id.store(1, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(3_700)).await;
        assert_eq!(pool.verify_all().await.verified, 2);
        assert_eq!(pool.health().unwrap().endpoints_wrong_chain, 0);
        assert_eq!(pool.source_count(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn all_endpoints_on_the_wrong_chain_disable_resolution() {
        let nodes = [node(), node()];
        for node in &nodes {
            node.chain_id.store(56, Ordering::SeqCst);
        }
        let pool = Arc::new(pool(&nodes, options()));

        assert_eq!(pool.chain_id().await, Ok(56));
        assert!(matches!(
            pool.call(addr(1), name_call()).await,
            Err(CallError::Transient(_))
        ));

        // The fetcher turns that into "disabled", without any error.
        let fetcher = MetadataFetcher::new(pool.clone(), fast_options())
            .expect_chain_id(1);
        let outcome =
            fetcher.fetch(&[(addr(1), TokenStandard::Erc20)]).await;
        assert!(outcome.resolved.is_empty() && outcome.empty.is_empty());
        assert!(fetcher.wrong_chain());
        assert!(!fetcher.is_available());
        assert_eq!(attempts(&nodes[0]) + attempts(&nodes[1]), 0);

        // While the set can still change it is only "unavailable".
        let pool = self::pool(&nodes, options());
        pool.set_refreshable(true);
        assert!(matches!(
            pool.chain_id().await,
            Err(CallError::Transient(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn execution_errors_are_not_failed_over() {
        let nodes = [node(), node()];
        let pool = pool(&nodes, options());

        let result = pool.call(addr(9), name_call()).await;
        assert!(matches!(result, Err(CallError::Execution(_))));
        assert_eq!((attempts(&nodes[0]), attempts(&nodes[1])), (1, 0));
        // The endpoint is perfectly healthy.
        assert_eq!(pool.health().unwrap().endpoints_healthy, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn total_outage_probes_one_endpoint_and_reports_transient() {
        let nodes = [node(), node(), node(), node()];
        for node in &nodes {
            node.offline.store(true, Ordering::SeqCst);
        }
        let pool = pool(&nodes, options());
        pool.verify_all().await;

        // Nothing verified and everything in cool-down: a single last
        // resort probe per call, whose error never names a URL.
        for _ in 0..3 {
            let Err(CallError::Transient(error)) =
                pool.call(addr(1), name_call()).await
            else {
                panic!("expected a transient error");
            };
            assert!(error.starts_with('#'), "{error}");
        }
        let probes: usize = nodes
            .iter()
            .map(|node| node.chain_id_calls.load(Ordering::SeqCst))
            .sum();
        assert_eq!(probes, 4 + 3);

        // Back online: the very next call works, no cool-down to sit out.
        for node in &nodes {
            node.offline.store(false, Ordering::SeqCst);
        }
        assert!(pool.call(addr(1), name_call()).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn attempts_per_call_are_bounded() {
        let nodes: Vec<_> = (0..6).map(|_| node()).collect();
        let pool = pool(&nodes, options());
        pool.verify_all().await;
        for node in &nodes {
            node.offline.store(true, Ordering::SeqCst);
        }

        assert!(pool.call(addr(1), name_call()).await.is_err());
        let total: usize = nodes.iter().map(|node| attempts(node)).sum();
        assert_eq!(total, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn no_endpoints_is_a_transient_error() {
        let pool = MultiEndpointCaller::new(1, Vec::new(), options());
        assert!(pool.is_empty());
        assert!(matches!(
            pool.call(addr(1), name_call()).await,
            Err(CallError::Transient(_))
        ));
        assert!(matches!(
            pool.chain_id().await,
            Err(CallError::Transient(_))
        ));
        assert_eq!(pool.confirm_empty(addr(1), name_call()).await, {
            EmptyCheck::Undecided
        });
    }

    #[tokio::test(start_paused = true)]
    async fn one_endpoint_without_multicall_does_not_flip_the_flag() {
        let nodes = [node(), node(), node()];
        // Endpoint 1 lags / is broken: no code at the Multicall3 address.
        nodes[0].multicall_deployed.store(false, Ordering::SeqCst);
        let pool = Arc::new(pool(&nodes, options()));
        let fetcher = MetadataFetcher::new(pool.clone(), fast_options())
            .expect_chain_id(1);
        let tokens = [(addr(1), TokenStandard::Erc20)];

        let outcome = fetcher.fetch(&tokens).await;
        assert_eq!(outcome.resolved[&addr(1)].symbol, "USDC");
        assert!(!fetcher.multicall_missing());
        // Resolved through another endpoint's Multicall3, not one by one.
        let direct: usize = nodes
            .iter()
            .map(|node| node.direct_calls.load(Ordering::SeqCst))
            .sum();
        assert_eq!(direct, 0);
        // The liar sits in cool-down.
        assert_eq!(pool.health().unwrap().endpoints_healthy, 2);

        // Later fetches do not pay for it again.
        let before = attempts(&nodes[0]);
        fetcher.fetch(&tokens).await;
        assert_eq!(attempts(&nodes[0]), before);
    }

    #[tokio::test(start_paused = true)]
    async fn multicall_absence_needs_two_endpoints_to_agree() {
        let nodes = [node(), node(), node()];
        for node in &nodes {
            node.multicall_deployed.store(false, Ordering::SeqCst);
        }
        let pool = Arc::new(pool(&nodes, options()));

        assert_eq!(
            pool.confirm_empty(MULTICALL3_ADDRESS, Bytes::new()).await,
            EmptyCheck::Confirmed
        );

        // Only one endpoint can answer: not enough to remember it.
        nodes[1].offline.store(true, Ordering::SeqCst);
        nodes[2].offline.store(true, Ordering::SeqCst);
        assert_eq!(
            pool.confirm_empty(MULTICALL3_ADDRESS, Bytes::new()).await,
            EmptyCheck::Undecided
        );

        // The fetcher still resolves (individually), and asks again later
        // instead of deciding for good.
        let fetcher = MetadataFetcher::new(pool.clone(), fast_options())
            .expect_chain_id(1);
        let outcome =
            fetcher.fetch(&[(addr(1), TokenStandard::Erc20)]).await;
        assert_eq!(outcome.resolved[&addr(1)].symbol, "USDC");
        assert!(fetcher.multicall_missing());
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(!fetcher.multicall_missing());

        // A single configured endpoint is its own authority.
        let single = self::pool(&nodes[..1], options());
        assert_eq!(
            single.confirm_empty(MULTICALL3_ADDRESS, Bytes::new()).await,
            EmptyCheck::Confirmed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn replace_discovered_keeps_configured_endpoints_and_state() {
        let nodes = [node(), node(), node()];
        let discovered = |n: usize| EndpointSpec {
            discovered: true,
            ..spec(n + 1, nodes[n].clone())
        };
        let pool = MultiEndpointCaller::new(
            1,
            vec![spec(1, nodes[0].clone()), discovered(1)],
            options(),
        );
        pool.verify_all().await;
        assert_eq!(nodes[1].chain_id_calls.load(Ordering::SeqCst), 1);

        pool.replace_discovered(vec![discovered(1), discovered(2)]);
        assert_eq!(pool.len(), 3);
        pool.verify_all().await;
        // Endpoint 2 kept its verification, endpoint 3 is new.
        assert_eq!(nodes[1].chain_id_calls.load(Ordering::SeqCst), 1);
        assert_eq!(nodes[2].chain_id_calls.load(Ordering::SeqCst), 1);

        pool.replace_discovered(Vec::new());
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn invalid_urls_are_errors_that_do_not_echo_the_url() {
        let error = MultiEndpointCaller::from_urls(
            1,
            &["https://ok.example/v2/key", "not a url SuPerSecret"],
            Duration::from_secs(1),
            options(),
        )
        .err()
        .map(|error| format!("{error:#}"))
        .unwrap_or_default();

        assert!(error.contains("#2"), "{error}");
        assert!(!error.contains("SuPerSecret"), "{error}");
    }

    #[tokio::test]
    async fn labels_hide_configured_hosts_and_show_discovered_ones() {
        let timeout = Duration::from_secs(1);
        let configured = url_endpoint(
            "https://eth-mainnet.g.alchemy.com/v2/SuPerSecretKey123",
            1,
            false,
            timeout,
        )
        .unwrap();
        assert_eq!(configured.label, "#1");

        let public =
            url_endpoint("https://rpc.example.org/", 2, true, timeout)
                .unwrap();
        assert_eq!(public.label, "#2 (rpc.example.org)");
        assert_eq!(public.key, "https://rpc.example.org");
    }

    // ---- trust: discovered endpoints are untrusted --------------------

    fn public_spec(n: usize, caller: Arc<dyn EthCaller>) -> EndpointSpec {
        EndpointSpec { discovered: true, ..spec(n, caller) }
    }

    fn public_pool(nodes: &[Arc<FakeChain>]) -> Arc<MultiEndpointCaller> {
        let specs = nodes
            .iter()
            .enumerate()
            .map(|(n, node)| public_spec(n + 1, node.clone()))
            .collect();
        Arc::new(MultiEndpointCaller::new(1, specs, options()))
    }

    fn fetcher(pool: &Arc<MultiEndpointCaller>) -> MetadataFetcher {
        MetadataFetcher::new(pool.clone(), fast_options())
            .expect_chain_id(1)
    }

    fn aggregates(nodes: &[Arc<FakeChain>]) -> usize {
        nodes
            .iter()
            .map(|node| node.multicall_calls.load(Ordering::SeqCst))
            .sum()
    }

    fn direct(nodes: &[Arc<FakeChain>]) -> usize {
        nodes
            .iter()
            .map(|node| node.direct_calls.load(Ordering::SeqCst))
            .sum()
    }

    const NEW_TOKEN: u64 = 42;

    /// A node that synced up to block 100 and stopped: right chain id,
    /// Multicall3 deployed, answers instantly, knows no recent contract.
    fn stale_node() -> Arc<FakeChain> {
        let chain = node();
        chain.height.store(100, Ordering::SeqCst);
        chain
    }

    fn fresh_node() -> Arc<FakeChain> {
        let chain = node();
        chain.height.store(5_000, Ordering::SeqCst);
        chain
            .add(addr(NEW_TOKEN), FakeToken::erc20("New Token", "NEW", 9));
        chain
    }

    #[test]
    fn providers_are_told_apart_by_their_registrable_domain() {
        assert_eq!(provider_of("https://rpc.ankr.com/eth"), "ankr.com");
        assert_eq!(provider_of("https://eth.ANKR.com."), "ankr.com");
        assert_eq!(provider_of("https://ankr.com"), "ankr.com");
        assert_eq!(
            provider_of("https://a.b.c.example.org/x"),
            "example.org"
        );
        assert_eq!(provider_of("http://10.0.0.7:8545"), "10.0.0.7");
        assert_eq!(provider_of("http://localhost:8545"), "localhost");
        assert_eq!(provider_of("node-1"), "node-1");

        // Same provider twice is one opinion.
        let nodes = [node(), node(), node()];
        let specs = vec![
            EndpointSpec {
                provider: "ankr.com".into(),
                ..public_spec(1, nodes[0].clone())
            },
            EndpointSpec {
                provider: "ankr.com".into(),
                ..public_spec(2, nodes[1].clone())
            },
            public_spec(3, nodes[2].clone()),
        ];
        let pool = MultiEndpointCaller::new(1, specs, options());
        assert_eq!(pool.source_count(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn agreement_costs_exactly_one_extra_aggregate_per_chunk() {
        let nodes = [fresh_node(), fresh_node()];
        let mut tokens = Vec::new();
        for n in 1_000..1_120u64 {
            for node in &nodes {
                node.add(addr(n), FakeToken::erc20("Token", "TKN", 18));
            }
            tokens.push((addr(n), TokenStandard::Erc20));
        }
        let pool = public_pool(&nodes);

        let outcome = fetcher(&pool).fetch(&tokens).await;

        assert_eq!(outcome.resolved.len(), 120);
        assert!(outcome.unconfirmed.is_empty());
        assert_eq!(outcome.resolved[&addr(1_077)].decimals, 18);
        // 3 chunks, each asked of two providers, nothing else.
        assert_eq!(aggregates(&nodes), 6);
        assert_eq!(direct(&nodes), 0);
        assert_eq!(nodes[0].multicall_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_single_public_endpoint_yields_nothing_rather_than_a_guess()
    {
        let nodes = [fresh_node()];
        nodes[0].add(addr(9), FakeToken::reverting());
        let pool = public_pool(&nodes);
        let fetcher = MetadataFetcher::new(
            pool.clone(),
            crate::tokens::multicall::FetchOptions {
                breaker_cooldown: Duration::from_secs(30),
                breaker_max_cooldown: Duration::from_secs(300),
                ..fast_options()
            },
        )
        .expect_chain_id(1);
        let tokens = [
            (addr(NEW_TOKEN), TokenStandard::Erc20),
            (addr(9), TokenStandard::Erc20),
            (addr(777), TokenStandard::Erc20),
        ];

        let outcome = fetcher.fetch(&tokens).await;

        // Not a row, not a blank row, not a strike: nothing.
        assert!(outcome.resolved.is_empty());
        assert!(outcome.empty.is_empty());
        assert_eq!(outcome.unconfirmed.len(), 3);
        // And no hammering of the one endpoint that works meanwhile.
        assert!(!fetcher.is_available());
        let calls = aggregates(&nodes) + direct(&nodes);
        assert!(fetcher.fetch(&tokens).await.unconfirmed.is_empty());
        assert_eq!(aggregates(&nodes) + direct(&nodes), calls);

        // A second provider shows up (discovery): business as usual.
        let second = fresh_node();
        second.add(addr(9), FakeToken::reverting());
        pool.replace_discovered(vec![
            public_spec(1, nodes[0].clone()),
            public_spec(2, second),
        ]);
        tokio::time::advance(Duration::from_secs(31)).await;
        let outcome = fetcher.fetch(&tokens).await;
        assert_eq!(outcome.resolved[&addr(NEW_TOKEN)].symbol, "NEW");
        assert_eq!(outcome.resolved[&addr(9)].symbol, "");
        assert_eq!(outcome.empty, vec![addr(777)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_public_node_never_produces_a_blank_row() {
        use crate::tokens::{TokenResolver, TokenResolverOptions};
        use std::collections::HashMap;

        // The stale node is listed first and answers first.
        let nodes = [stale_node(), fresh_node()];
        let pool = public_pool(&nodes);
        let resolver = TokenResolver::from_parts(
            1,
            Some(fetcher(&pool)),
            None,
            &TokenResolverOptions {
                empty_ttl: Duration::from_secs(600),
                fetch: fast_options(),
                ..TokenResolverOptions::default()
            },
        );
        let tokens =
            HashMap::from([(addr(NEW_TOKEN), TokenStandard::Erc20)]);

        // "No code" from one node against a real answer from the other,
        // nobody to break the tie: no row, and above all no strike
        // towards a blank row, however often it is asked.
        for _ in 0..5 {
            assert!(resolver.resolve_new(&tokens).await.is_empty());
            tokio::time::advance(Duration::from_secs(700)).await;
        }
        assert_eq!(resolver.stats().negative, 0);
        assert_eq!(resolver.stats().codeless, 0);
        assert!(resolver.stats().unconfirmed >= 5);

        // Telling the backend where the indexer is exposes the node
        // (it is 4900 blocks behind what is being indexed).
        resolver.set_head(5_000);
        assert!(resolver.resolve_new(&tokens).await.is_empty());
        assert_eq!(pool.health().unwrap().endpoints_healthy, 1);

        // A third provider: the two fresh ones agree, the row is right.
        pool.replace_discovered(vec![
            public_spec(1, nodes[0].clone()),
            public_spec(2, nodes[1].clone()),
            public_spec(3, fresh_node()),
        ]);
        tokio::time::advance(Duration::from_secs(700)).await;
        let rows = resolver.resolve_new(&tokens).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].symbol.as_str(), rows[0].decimals),
            ("NEW", 9)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stale_nodes_are_recognized_by_the_height_of_their_answers() {
        // No indexer head this time: the aggregate's own
        // getBlockNumber() is compared between providers.
        let nodes = [stale_node(), fresh_node(), fresh_node()];
        let pool = public_pool(&nodes);
        let fetcher = fetcher(&pool);
        let tokens = [(addr(NEW_TOKEN), TokenStandard::Erc20)];

        // First contact: nothing to compare with yet, so the stale
        // answer is heard, contradicted twice, and outvoted.
        let outcome = fetcher.fetch(&tokens).await;
        assert_eq!(outcome.resolved[&addr(NEW_TOKEN)].symbol, "NEW");
        assert!(outcome.empty.is_empty());

        // From then on its answers are not even considered: 4900 blocks
        // behind the second best provider.
        tokio::time::advance(Duration::from_secs(400)).await;
        let before = aggregates(&nodes[1..]);
        let outcome = fetcher.fetch(&tokens).await;
        assert_eq!(outcome.resolved[&addr(NEW_TOKEN)].symbol, "NEW");
        // Two fresh opinions, no tie-break needed.
        assert_eq!(aggregates(&nodes[1..]), before + 2);
        assert_eq!(pool.health().unwrap().endpoints_healthy, 2);

        // It catches up: welcome back.
        nodes[0].height.store(5_001, Ordering::SeqCst);
        nodes[0]
            .add(addr(NEW_TOKEN), FakeToken::erc20("New Token", "NEW", 9));
        tokio::time::advance(Duration::from_secs(400)).await;
        // (Everybody is asked, so it certainly gets its chance.)
        pool.confirm_empty(addr(1), name_call()).await;
        assert_eq!(pool.health().unwrap().endpoints_healthy, 3);
        let outcome = fetcher.fetch(&tokens).await;
        assert_eq!(outcome.resolved[&addr(NEW_TOKEN)].symbol, "NEW");
    }

    #[tokio::test(start_paused = true)]
    async fn one_absurd_height_does_not_disqualify_the_honest_nodes() {
        let nodes = [fresh_node(), fresh_node(), fresh_node()];
        nodes[0].height.store(u64::MAX / 2, Ordering::SeqCst);
        let pool = public_pool(&nodes);
        let fetcher = fetcher(&pool);
        let tokens = [(addr(NEW_TOKEN), TokenStandard::Erc20)];

        for _ in 0..3 {
            let outcome = fetcher.fetch(&tokens).await;
            assert_eq!(outcome.resolved[&addr(NEW_TOKEN)].symbol, "NEW");
            tokio::time::advance(Duration::from_secs(120)).await;
        }
        assert_eq!(pool.health().unwrap().endpoints_healthy, 3);

        // The indexer's own head and configured endpoints do move the
        // bar on their own.
        nodes[0].height.store(5_000, Ordering::SeqCst);
        pool.head_hint(9_000);
        let first = pool
            .call_routed(addr(1), name_call(), &Route::default())
            .await;
        assert!(first.is_err(), "every node is 4000 blocks behind");
    }

    /// Answers instantly, and wrongly: everything reverts, or decimals
    /// are off by twelve.
    fn lying_node(decimals: Option<u8>) -> Arc<FakeChain> {
        let chain = node();
        chain.height.store(5_000, Ordering::SeqCst);
        chain.add(
            addr(NEW_TOKEN),
            match decimals {
                Some(decimals) => {
                    FakeToken::erc20("New Token", "NEW", decimals)
                }
                None => FakeToken::reverting(),
            },
        );
        chain
    }

    #[tokio::test(start_paused = true)]
    async fn a_lying_fast_endpoint_is_outvoted_and_demoted() {
        for lie in [None, Some(21)] {
            let chains = [lying_node(lie), fresh_node(), fresh_node()];
            let specs = chains
                .iter()
                .zip([1u64, 40, 80])
                .enumerate()
                .map(|(n, (chain, latency))| {
                    public_spec(
                        n + 1,
                        Arc::new(Slow {
                            chain: chain.clone(),
                            latency: Duration::from_millis(latency),
                        }),
                    )
                })
                .collect();
            let pool =
                Arc::new(MultiEndpointCaller::new(1, specs, options()));
            let fetcher = fetcher(&pool);
            let tokens = [(addr(NEW_TOKEN), TokenStandard::Erc20)];

            // Whoever is asked first, the two honest providers win.
            for round in 0..3 {
                let outcome = fetcher.fetch(&tokens).await;
                let token = &outcome.resolved[&addr(NEW_TOKEN)];
                assert_eq!(
                    (token.symbol.as_str(), token.decimals),
                    ("NEW", 9),
                    "{lie:?} round {round}"
                );
                // Sit out the liar's cool-down: it is the fastest, so
                // it is asked (and caught) again.
                tokio::time::advance(Duration::from_secs(301)).await;
            }

            // Three lost votes: banned, not merely cooling down.
            let health = pool.health().unwrap();
            assert_eq!(health.endpoints_distrusted, 1, "{lie:?}");
            assert_eq!(pool.source_count(), 2);
            let lies = attempts(&chains[0]);
            let outcome = fetcher.fetch(&tokens).await;
            assert_eq!(outcome.resolved[&addr(NEW_TOKEN)].decimals, 9);
            assert_eq!(attempts(&chains[0]), lies);

            // Not for life.
            tokio::time::advance(Duration::from_secs(3_601)).await;
            assert_eq!(pool.health().unwrap().endpoints_distrusted, 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_liar_and_one_honest_endpoint_produce_no_row() {
        for lie in [None, Some(21)] {
            let nodes = [lying_node(lie), fresh_node()];
            let pool = public_pool(&nodes);
            let outcome = fetcher(&pool)
                .fetch(&[(addr(NEW_TOKEN), TokenStandard::Erc20)])
                .await;

            assert!(outcome.resolved.is_empty(), "{lie:?}");
            assert!(outcome.empty.is_empty());
            assert_eq!(outcome.unconfirmed, vec![addr(NEW_TOKEN)]);
            // Nobody can tell who lied: nobody is punished.
            assert_eq!(pool.health().unwrap().endpoints_distrusted, 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn configured_endpoints_are_preferred_and_believed_on_positives()
    {
        let mine = fresh_node();
        let public = [fresh_node(), fresh_node()];
        for node in public.iter().chain([&mine]) {
            node.add(addr(9), FakeToken::reverting());
        }
        // The configured endpoint is by far the slowest: still first.
        let specs = vec![
            public_spec(1, public[0].clone()),
            spec(
                2,
                Arc::new(Slow {
                    chain: mine.clone(),
                    latency: Duration::from_millis(900),
                }),
            ),
            public_spec(3, public[1].clone()),
        ];
        let pool = Arc::new(MultiEndpointCaller::new(1, specs, options()));
        let fetcher = fetcher(&pool);

        // Positive answers: one aggregate, on the configured endpoint.
        let outcome = fetcher
            .fetch(&[
                (addr(1), TokenStandard::Erc20),
                (addr(NEW_TOKEN), TokenStandard::Erc20),
            ])
            .await;
        assert_eq!(outcome.resolved.len(), 2);
        assert_eq!(mine.multicall_calls.load(Ordering::SeqCst), 1);
        assert_eq!(aggregates(&public), 0);

        // Negative answers (a reverting token, an address without code)
        // always get a second opinion, here from a public endpoint.
        let outcome = fetcher
            .fetch(&[
                (addr(1), TokenStandard::Erc20),
                (addr(9), TokenStandard::Erc20),
                (addr(777), TokenStandard::Erc20),
            ])
            .await;
        assert_eq!(outcome.resolved[&addr(1)].symbol, "USDC");
        assert_eq!(outcome.resolved[&addr(9)].symbol, "");
        assert_eq!(outcome.empty, vec![addr(777)]);
        assert!(aggregates(&public) + direct(&public) > 0);

        // A configured endpoint that is wrong about a negative is
        // overruled by two public ones agreeing.
        let tokens = [(addr(555), TokenStandard::Erc20)];
        for node in &public {
            node.add(addr(555), FakeToken::erc20("Late", "LATE", 6));
        }
        let outcome = fetcher.fetch(&tokens).await;
        assert_eq!(outcome.resolved[&addr(555)].symbol, "LATE");

        // With the public ones gone, negatives wait; positives do not.
        for node in &public {
            node.offline.store(true, Ordering::SeqCst);
        }
        tokio::time::advance(Duration::from_secs(301)).await;
        let outcome = fetcher
            .fetch(&[
                (addr(1), TokenStandard::Erc20),
                (addr(9), TokenStandard::Erc20),
            ])
            .await;
        assert_eq!(outcome.resolved.len(), 1);
        assert_eq!(outcome.unconfirmed, vec![addr(9)]);
        // The RPC as a whole is fine: no pause over a missing second
        // opinion while the configured endpoint delivers.
        assert!(fetcher.is_available());
    }

    #[tokio::test(start_paused = true)]
    async fn several_configured_endpoints_confirm_each_others_negatives() {
        let nodes = [fresh_node(), fresh_node()];
        nodes[0].add(addr(9), FakeToken::reverting());
        // Endpoint 2 knows better.
        nodes[1].add(addr(9), FakeToken::erc20("Real", "REAL", 18));
        let pool = Arc::new(pool(&nodes, options()));

        let outcome =
            fetcher(&pool).fetch(&[(addr(9), TokenStandard::Erc20)]).await;
        assert!(outcome.resolved.is_empty());
        assert_eq!(outcome.unconfirmed, vec![addr(9)]);

        // A single configured endpoint is its own authority, as before.
        let single = Arc::new(self::pool(&nodes[..1], options()));
        let outcome = fetcher(&single)
            .fetch(&[(addr(9), TokenStandard::Erc20)])
            .await;
        assert_eq!(outcome.resolved[&addr(9)].symbol, "");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_sub_call_is_asked_again_alone_before_it_is_stored() {
        let chain = fresh_node();
        // [0] getBlockNumber [1] name [2] symbol [3] decimals: the gas
        // runs out right before decimals().
        chain.fail_subcalls_from.store(3, Ordering::SeqCst);
        let pool = Arc::new(pool(std::slice::from_ref(&chain), options()));

        let outcome = fetcher(&pool)
            .fetch(&[(addr(NEW_TOKEN), TokenStandard::Erc20)])
            .await;

        // Not "NEW with 0 decimals".
        let token = &outcome.resolved[&addr(NEW_TOKEN)];
        assert_eq!((token.symbol.as_str(), token.decimals), ("NEW", 9));
        assert_eq!(chain.direct_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn quiet_failures_open_no_breaker() {
        let nodes = [fresh_node(), fresh_node()];
        let pool = Arc::new(pool(&nodes, options()));
        let fetcher = MetadataFetcher::new(
            pool.clone(),
            crate::tokens::multicall::FetchOptions {
                breaker_cooldown: Duration::from_secs(30),
                breaker_max_cooldown: Duration::from_secs(300),
                ..fast_options()
            },
        )
        .expect_chain_id(1);
        pool.verify_all().await;
        let tokens = [(addr(NEW_TOKEN), TokenStandard::Erc20)];

        for node in &nodes {
            node.offline.store(true, Ordering::SeqCst);
        }

        // A known troublemaker failing says nothing about the RPC.
        let outcome = fetcher.fetch_with(&tokens, true).await;
        assert!(outcome.resolved.is_empty());
        assert!(fetcher.is_available());
        assert_eq!(pool.health().unwrap().endpoints_healthy, 2);

        // Anybody else failing does.
        fetcher.fetch(&tokens).await;
        assert!(!fetcher.is_available());
        assert_eq!(pool.health().unwrap().endpoints_healthy, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn call_confirmed_applies_the_same_rules_to_any_eth_call() {
        use crate::tokens::multicall::call_confirmed;

        // Two honest public endpoints: confirmed, at the cost of a call.
        let nodes = [fresh_node(), fresh_node()];
        let public = public_pool(&nodes);
        let answer =
            call_confirmed(&*public, addr(NEW_TOKEN), name_call()).await;
        assert!(answer.is_ok_and(|data| !data.is_empty()));
        assert_eq!(attempts(&nodes[0]) + attempts(&nodes[1]), 2);

        // Agreed execution failures are execution failures.
        assert!(matches!(
            call_confirmed(&*public, addr(9), name_call()).await,
            Err(CallError::Execution(_))
        ));

        // One public endpoint, or two that disagree: nothing to store.
        let alone = public_pool(&nodes[..1]);
        assert!(matches!(
            call_confirmed(&*alone, addr(NEW_TOKEN), name_call()).await,
            Err(CallError::Transient(_))
        ));
        let disagreeing = public_pool(&[lying_node(None), fresh_node()]);
        assert!(matches!(
            call_confirmed(&*disagreeing, addr(NEW_TOKEN), name_call())
                .await,
            Err(CallError::Transient(_))
        ));

        // A third one settles it, and the liar is reported.
        let liar = lying_node(None);
        let settled =
            public_pool(&[liar.clone(), fresh_node(), fresh_node()]);
        let answer =
            call_confirmed(&*settled, addr(NEW_TOKEN), name_call()).await;
        assert!(answer.is_ok_and(|data| !data.is_empty()));
        assert_eq!(settled.health().unwrap().endpoints_healthy, 2);

        // A configured endpoint: one call, believed.
        let mine = [fresh_node()];
        let configured = pool(&mine, options());
        let answer =
            call_confirmed(&configured, addr(NEW_TOKEN), name_call())
                .await;
        assert!(answer.is_ok_and(|data| !data.is_empty()));
        assert_eq!(attempts(&mine[0]), 1);
        // ...and a plain single node caller is its own authority too.
        let plain = fresh_node();
        assert!(call_confirmed(&*plain, addr(NEW_TOKEN), name_call())
            .await
            .is_ok());
    }
}
