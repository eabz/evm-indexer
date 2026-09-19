//! One [`EthCaller`] over several JSON-RPC endpoints, so that no single
//! node can stall or disable token metadata.
//!
//! * every endpoint has its own circuit breaker (doubling cool-down) and is
//!   only used once it proved (`eth_chainId`) that it serves the indexed
//!   chain; an endpoint on another chain is disabled for good with a single
//!   error log while the others keep working;
//! * a call goes to the healthiest endpoint first (breaker closed, lowest
//!   EWMA latency; endpoints that were never measured go first so they all
//!   get explored) and rotates to the next one on a transient failure,
//!   within the same call;
//! * [`CallError::Execution`] is returned as is: the node executed the call
//!   and the EVM failed, which is a property of the contract and would be
//!   the same everywhere;
//! * when every breaker is open the endpoint closest to the end of its
//!   cool-down is probed anyway, so with a single endpoint this behaves
//!   exactly like a plain caller and the pause policy stays where it was:
//!   in the fetcher's own breaker.

use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, RwLock,
    },
    time::Duration,
};

use alloy::primitives::{Address, Bytes};
use anyhow::Context;
use futures::future::{join_all, BoxFuture};
use log::{debug, error, info};
use tokio::time::Instant;

use super::{
    breaker::CircuitBreaker,
    multicall::{
        AlloyCaller, CallError, CallerHealth, EmptyCheck, EthCaller,
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
}

impl Default for EndpointOptions {
    fn default() -> Self {
        Self {
            breaker_cooldown: Duration::from_secs(30),
            breaker_max_cooldown: Duration::from_secs(300),
            max_attempts_per_call: 3,
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
    pub caller: Arc<dyn EthCaller>,
    /// Found by `--rpc auto` (replaceable) rather than configured.
    pub discovered: bool,
}

const CHAIN_UNCHECKED: u8 = 0;
const CHAIN_VERIFIED: u8 = 1;
const CHAIN_MISMATCH: u8 = 2;

struct Endpoint {
    key: String,
    label: String,
    caller: Arc<dyn EthCaller>,
    discovered: bool,
    chain_state: AtomicU8,
    observed_chain_id: AtomicU64,
    breaker: CircuitBreaker,
    /// EWMA of the request latency in microseconds, 0 = never measured.
    latency_micros: AtomicU64,
    down: AtomicBool,
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

impl Endpoint {
    fn new(spec: EndpointSpec, options: &EndpointOptions) -> Self {
        Self {
            key: spec.key,
            label: spec.label,
            caller: spec.caller,
            discovered: spec.discovered,
            chain_state: AtomicU8::new(CHAIN_UNCHECKED),
            observed_chain_id: AtomicU64::new(0),
            breaker: CircuitBreaker::new(
                options.breaker_cooldown,
                options.breaker_max_cooldown,
            ),
            latency_micros: AtomicU64::new(0),
            down: AtomicBool::new(false),
        }
    }

    fn wrong_chain(&self) -> bool {
        self.chain_state.load(Ordering::Relaxed) == CHAIN_MISMATCH
    }

    fn verified(&self) -> bool {
        self.chain_state.load(Ordering::Relaxed) == CHAIN_VERIFIED
    }

    /// On the right chain (as far as known) and its last request worked.
    /// An endpoint whose cool-down ended stays unhealthy until it proves
    /// itself again.
    fn healthy(&self) -> bool {
        !self.wrong_chain() && !self.down.load(Ordering::Relaxed)
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
        match self.chain_state.load(Ordering::Relaxed) {
            CHAIN_VERIFIED => return Verification::Verified,
            CHAIN_MISMATCH => return Verification::WrongChain,
            _ => {}
        }

        // Latency is only measured on `eth_call`s (comparable requests).
        match self.caller.chain_id().await {
            Ok(actual) if actual == expected => {
                self.chain_state.store(CHAIN_VERIFIED, Ordering::Relaxed);
                Verification::Verified
            }
            Ok(actual) => {
                self.observed_chain_id.store(actual, Ordering::Relaxed);
                if self.chain_state.swap(CHAIN_MISMATCH, Ordering::Relaxed)
                    != CHAIN_MISMATCH
                {
                    error!(
                        "RPC endpoint {} serves chain {actual} but chain \
                         {expected} is being indexed: it is disabled",
                        self.label
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

    async fn execute(
        &self,
        expected_chain_id: u64,
        to: Address,
        data: Bytes,
    ) -> Attempt {
        match self.verify(expected_chain_id).await {
            Verification::Verified => {}
            Verification::WrongChain => return Attempt::WrongChain,
            Verification::Unavailable(error) => {
                return Attempt::Failed(error)
            }
        }

        let started = Instant::now();
        let result = self.caller.call(to, data).await;
        self.record_latency(started);

        match result {
            Ok(returned) => {
                self.succeeded();
                Attempt::Returned(returned)
            }
            // The node is alive and executed the call.
            Err(CallError::Execution(error)) => {
                self.succeeded();
                Attempt::Execution(error)
            }
            Err(CallError::Transient(error)) => {
                self.failed(&error);
                Attempt::Failed(error)
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
        }
    }

    /// Endpoints over HTTP(S) JSON-RPC URLs. Only an invalid URL is an
    /// error (it names the position of the entry, never the URL itself).
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
    /// the state (chain check, breaker, latency) of the discovered
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

            let kept = current.iter().find(|endpoint| {
                endpoint.key == spec.key && !endpoint.wrong_chain()
            });

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

    /// Usable endpoints, best first: closed breakers by latency, then the
    /// open breaker that closes the soonest as a last resort.
    fn candidates(&self) -> Vec<Arc<Endpoint>> {
        let endpoints = self.endpoints();

        let (mut closed, open): (Vec<_>, Vec<_>) = endpoints
            .iter()
            .filter(|endpoint| !endpoint.wrong_chain())
            .cloned()
            .partition(|endpoint| !endpoint.breaker.is_open());

        // Stable: ties keep the configured order.
        closed.sort_by_key(|endpoint| {
            endpoint.latency_micros.load(Ordering::Relaxed)
        });

        let last_resort = open
            .into_iter()
            .min_by_key(|endpoint| endpoint.breaker.open_until());
        closed.extend(last_resort);

        closed
    }

    fn unavailable(&self) -> CallError {
        CallError::Transient(format!(
            "no RPC endpoint is available for chain {}",
            self.expected_chain_id
        ))
    }
}

impl EthCaller for MultiEndpointCaller {
    fn call(
        &self,
        to: Address,
        data: Bytes,
    ) -> BoxFuture<'_, Result<Bytes, CallError>> {
        Box::pin(async move {
            let mut attempts = 0usize;
            let mut last_error = None;

            for endpoint in self.candidates() {
                if attempts >= self.options.max_attempts_per_call.max(1) {
                    break;
                }

                match endpoint
                    .execute(self.expected_chain_id, to, data.clone())
                    .await
                {
                    Attempt::Returned(returned) => return Ok(returned),
                    // Not failed over: every node would say the same.
                    Attempt::Execution(error) => {
                        return Err(CallError::Execution(error))
                    }
                    Attempt::Failed(error) => {
                        attempts += 1;
                        last_error =
                            Some(format!("{}: {error}", endpoint.label));
                    }
                    Attempt::WrongChain => {}
                }
            }

            Err(match last_error {
                Some(error) => CallError::Transient(error),
                None => self.unavailable(),
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
                _ => Err(self.unavailable()),
            }
        })
    }

    /// Multicall3 (or any contract) being absent is a property of the
    /// chain, not of the endpoint that said so. Every usable endpoint is
    /// asked the same call:
    ///
    /// * any endpoint returning data refutes it (that data is handed
    ///   back, and the endpoints that answered empty are put in cool-down:
    ///   they lag behind or are broken);
    /// * it is confirmed by two endpoints agreeing with nobody
    ///   disagreeing (one when there is only one endpoint);
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
                .filter(|endpoint| !endpoint.wrong_chain())
                .cloned()
                .collect();

            let answers = join_all(endpoints.iter().map(|endpoint| {
                endpoint.execute(self.expected_chain_id, to, data.clone())
            }))
            .await;

            let mut empty = Vec::new();
            let mut refuted = None;
            let mut usable = endpoints.len();

            for (endpoint, answer) in endpoints.iter().zip(answers) {
                match answer {
                    Attempt::Returned(returned) if returned.is_empty() => {
                        empty.push(endpoint)
                    }
                    Attempt::Returned(returned) => {
                        refuted.get_or_insert(returned);
                    }
                    Attempt::WrongChain => usable -= 1,
                    Attempt::Execution(_) | Attempt::Failed(_) => {}
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

            if !empty.is_empty() && empty.len() >= usable.min(2) {
                EmptyCheck::Confirmed
            } else {
                EmptyCheck::Undecided
            }
        })
    }

    fn health(&self) -> Option<CallerHealth> {
        let endpoints = self.endpoints();

        Some(CallerHealth {
            endpoints_total: endpoints.len(),
            endpoints_healthy: endpoints
                .iter()
                .filter(|endpoint| endpoint.healthy())
                .count(),
            endpoints_wrong_chain: endpoints
                .iter()
                .filter(|endpoint| endpoint.wrong_chain())
                .count(),
        })
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

    let caller = AlloyCaller::new(url, call_timeout)
        .with_context(|| format!("rpc endpoint #{position}"))?;

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

    Ok(EndpointSpec {
        key: url.trim_end_matches('/').to_string(),
        label,
        caller: Arc::new(caller),
        discovered,
    })
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
        assert!(pool.candidates().iter().all(|e| !e.breaker.is_open()));
        assert_eq!(pool.candidates().len(), 2);
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

        // Not even time heals it.
        tokio::time::advance(Duration::from_secs(3_600)).await;
        assert!(pool.call(addr(1), name_call()).await.is_ok());
        assert_eq!(nodes[0].chain_id_calls.load(Ordering::SeqCst), 1);
        assert_eq!(attempts(&nodes[0]), 0);
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
}
