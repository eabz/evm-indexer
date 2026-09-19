//! Builds the RPC backend of token metadata from the `--rpc` argument,
//! including `--rpc auto`: zero-config discovery of public endpoints from
//! the chain registry at <https://chainid.network>.
//!
//! Discovery is best effort by design. Nothing here can fail the indexer
//! at runtime: an unreachable registry or a chain without public endpoints
//! yields a caller without endpoints that keeps looking for some in the
//! background, and public endpoints that die are replaced the same way.

use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context};
use futures::{future::BoxFuture, stream, StreamExt};
use log::{debug, info, warn};
use serde::Deserialize;
use tokio::time::Instant;

use super::{
    endpoints::{
        log_summary, EndpointOptions, EndpointSpec, MultiEndpointCaller,
    },
    multicall::{AlloyCaller, EthCaller, FetchOptions},
    redact::redact_urls,
};

/// The registry behind chainlist.org (ethereum-lists/chains).
pub const CHAIN_REGISTRY_URL: &str = "https://chainid.network/chains.json";

/// The `--rpc` value that turns discovery on.
pub const AUTO: &str = "auto";

/// Tunables of [`discover_with`] / [`build_caller_with`].
#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
    /// Endpoints kept after probing.
    pub max_endpoints: usize,
    /// Registry entries probed at most (in registry order).
    pub max_probed: usize,
    pub probe_timeout: Duration,
    pub probe_concurrency: usize,
    /// How often the health of the discovered endpoints is looked at.
    pub refresh_check_interval: Duration,
    /// Minimum time between two discoveries when some endpoints still
    /// work (with none left it is `refresh_check_interval`).
    pub refresh_min_interval: Duration,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            max_endpoints: 8,
            max_probed: 24,
            probe_timeout: Duration::from_secs(5),
            probe_concurrency: 8,
            refresh_check_interval: Duration::from_secs(60),
            refresh_min_interval: Duration::from_secs(1_800),
        }
    }
}

/// Everything [`build_caller_with`] can be tuned with.
#[derive(Debug, Clone, Default)]
pub struct CallerOptions {
    pub endpoints: EndpointOptions,
    pub discovery: DiscoveryOptions,
    /// Timeout of a single JSON-RPC request. `None`: the default of
    /// [`FetchOptions`].
    pub call_timeout: Option<Duration>,
}

impl CallerOptions {
    fn call_timeout(&self) -> Duration {
        self.call_timeout
            .unwrap_or_else(|| FetchOptions::default().call_timeout)
    }
}

/// Where the chain registry JSON comes from (the network, or a test).
pub trait ChainRegistry: Send + Sync + 'static {
    fn fetch(&self) -> BoxFuture<'_, anyhow::Result<Vec<u8>>>;
}

/// Turns a URL into a caller (alloy over HTTP, or a fake in tests).
pub type Connector = dyn Fn(&str) -> anyhow::Result<Arc<dyn EthCaller>>
    + Send
    + Sync
    + 'static;

/// [`ChainRegistry`] over HTTPS with a timeout and a response size cap.
pub struct HttpChainRegistry {
    pub url: String,
    pub timeout: Duration,
    pub max_bytes: usize,
}

impl Default for HttpChainRegistry {
    fn default() -> Self {
        Self {
            url: CHAIN_REGISTRY_URL.to_string(),
            timeout: Duration::from_secs(20),
            // ~4 MiB at the time of writing.
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

impl ChainRegistry for HttpChainRegistry {
    fn fetch(&self) -> BoxFuture<'_, anyhow::Result<Vec<u8>>> {
        Box::pin(async move {
            let download = async {
                let client = reqwest::Client::builder()
                    .connect_timeout(self.timeout)
                    .build()
                    .map_err(|e| anyhow!(redact_urls(&e.to_string())))?;

                let mut response = client
                    .get(&self.url)
                    .send()
                    .await
                    .and_then(|response| response.error_for_status())
                    .map_err(|e| anyhow!(redact_urls(&e.to_string())))?;

                if response
                    .content_length()
                    .is_some_and(|length| length > self.max_bytes as u64)
                {
                    bail!("the chain registry response is too large");
                }

                let mut body = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|e| anyhow!(redact_urls(&e.to_string())))?
                {
                    if body.len() + chunk.len() > self.max_bytes {
                        bail!("the chain registry response is too large");
                    }
                    body.extend_from_slice(&chunk);
                }

                Ok(body)
            };

            // One deadline for the whole download, not per chunk.
            tokio::time::timeout(self.timeout, download).await.map_err(
                |_| {
                    anyhow!(
                        "the chain registry did not answer within {:?}",
                        self.timeout
                    )
                },
            )?
        })
    }
}

/// Only what is needed, and lenient: an entry of an unexpected shape
/// anywhere in the registry must not break discovery for every chain.
#[derive(Deserialize)]
struct RegistryEntry {
    #[serde(rename = "chainId", default)]
    chain_id: serde_json::Value,
    #[serde(default)]
    rpc: serde_json::Value,
}

/// The usable public RPC URLs the registry lists for `chain_id`, in
/// registry order and without duplicates.
///
/// Kept: `https://` URLs with a domain name. Dropped: templates that need
/// an API key (`${INFURA_API_KEY}` and friends), websockets, plain http,
/// URLs with credentials, IP literals and localhost.
pub fn select_rpc_urls(
    registry_json: &[u8],
    chain_id: u64,
) -> anyhow::Result<Vec<String>> {
    let entries: Vec<RegistryEntry> =
        serde_json::from_slice(registry_json)
            .context("the chain registry is not the expected JSON")?;

    let mut seen = HashSet::new();
    let mut urls = Vec::new();

    for entry in entries {
        if entry.chain_id.as_u64() != Some(chain_id) {
            continue;
        }

        let serde_json::Value::Array(rpc) = entry.rpc else {
            continue;
        };

        for item in rpc {
            // Plain strings today; tolerate `{ "url": "..." }` objects.
            let raw = match &item {
                serde_json::Value::String(url) => Some(url.as_str()),
                serde_json::Value::Object(object) => {
                    object.get("url").and_then(|url| url.as_str())
                }
                _ => None,
            };

            let Some(url) = raw.and_then(public_https_url) else {
                continue;
            };

            if seen.insert(url.to_ascii_lowercase()) {
                urls.push(url);
            }
        }
    }

    Ok(urls)
}

/// Normalizes `raw` when it is an endpoint anybody can use as is.
fn public_https_url(raw: &str) -> Option<String> {
    let raw = raw.trim();

    // Templates (`${KEY}`, `{key}`, `<key>`) and anything odd.
    if raw.is_empty()
        || raw.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(c, '$' | '{' | '}' | '<' | '>' | '`' | '"')
        })
    {
        return None;
    }

    let url = url::Url::parse(raw).ok()?;

    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }

    match url.host()? {
        url::Host::Domain(domain)
            if domain != "localhost"
                && !domain.ends_with(".localhost")
                && !domain.ends_with(".local")
                && domain.contains('.') => {}
        _ => return None,
    }

    Some(url.as_str().trim_end_matches('/').to_string())
}

/// Asks every URL for its chain id (concurrently, one attempt each) and
/// returns the ones serving `chain_id`, fastest first.
pub async fn probe_rpc_urls(
    chain_id: u64,
    urls: Vec<String>,
    connect: &Connector,
    options: &DiscoveryOptions,
) -> Vec<String> {
    let probes: Vec<_> = urls
        .into_iter()
        .take(options.max_probed)
        .map(|url| async move {
            let caller = connect(&url).ok()?;
            let started = Instant::now();
            let answer = tokio::time::timeout(
                options.probe_timeout,
                caller.chain_id(),
            )
            .await;

            match answer {
                Ok(Ok(actual)) if actual == chain_id => {
                    Some((started.elapsed(), url))
                }
                Ok(Ok(actual)) => {
                    debug!(
                        "Public RPC {url} serves chain {actual}, not \
                         {chain_id}: ignored"
                    );
                    None
                }
                _ => {
                    debug!("Public RPC {url} did not answer: ignored");
                    None
                }
            }
        })
        .collect();

    let mut alive: Vec<(Duration, String)> = stream::iter(probes)
        .buffer_unordered(options.probe_concurrency.max(1))
        .filter_map(|result| async move { result })
        .collect()
        .await;

    // Latency, then URL so equal latencies (tests) are deterministic.
    alive.sort();

    // One endpoint per host (the fastest): `host/fast`, `host/private`...
    // are one provider, and failover needs independent ones.
    let mut hosts = HashSet::new();
    alive
        .into_iter()
        .map(|(_, url)| url)
        .filter(|url| {
            let host = url::Url::parse(url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_string));
            host.is_some_and(|host| hosts.insert(host))
        })
        .take(options.max_endpoints)
        .collect()
}

/// Discovery over explicit backends, see [`discover_public_rpcs`].
pub async fn discover_with(
    chain_id: u64,
    registry: &dyn ChainRegistry,
    connect: &Connector,
    options: &DiscoveryOptions,
) -> anyhow::Result<Vec<String>> {
    let json =
        registry.fetch().await.context("fetch the chain registry")?;
    let listed = select_rpc_urls(&json, chain_id)?;
    drop(json);

    if listed.is_empty() {
        bail!(
            "the chain registry lists no public RPC for chain {chain_id}"
        );
    }

    let total = listed.len();
    let alive = probe_rpc_urls(chain_id, listed, connect, options).await;

    if alive.is_empty() {
        bail!(
            "none of the {total} public RPCs listed for chain {chain_id} \
             answered on that chain"
        );
    }

    Ok(alive)
}

fn http_connector(call_timeout: Duration) -> Arc<Connector> {
    Arc::new(move |url: &str| {
        Ok(Arc::new(AlloyCaller::new(url, call_timeout)?)
            as Arc<dyn EthCaller>)
    })
}

/// Public HTTPS JSON-RPC endpoints currently serving `chain_id`, fastest
/// first, at most 8. They are public: safe to log.
pub async fn discover_public_rpcs(
    chain_id: u64,
) -> anyhow::Result<Vec<String>> {
    let options = CallerOptions::default();

    discover_with(
        chain_id,
        &HttpChainRegistry::default(),
        &*http_connector(options.call_timeout()),
        &options.discovery,
    )
    .await
}

/// The RPC backend for the `--rpc` argument, shared by the token worker
/// and the DEX pool resolver:
///
/// * `None` (or blank): `Ok(None)`, RPC features are disabled;
/// * `"a,b,c"`: those endpoints with failover (blanks ignored);
/// * `"auto"`: public endpoints discovered for `chain_id`, kept fresh in
///   the background. May be mixed: `"https://mine,auto"`.
///
/// Errors are configuration mistakes only: an invalid URL, or every
/// configured endpoint answering with another chain id. Endpoints that
/// cannot be reached, and a discovery that finds nothing, are not errors.
pub async fn build_caller(
    chain_id: u64,
    rpc_arg: Option<&str>,
) -> anyhow::Result<Option<Arc<dyn EthCaller>>> {
    build_caller_with(
        chain_id,
        rpc_arg,
        CallerOptions::default(),
        Arc::new(HttpChainRegistry::default()),
        None,
    )
    .await
}

/// [`build_caller`] with explicit tunables and backends (`connect: None`
/// is alloy over HTTP).
pub async fn build_caller_with(
    chain_id: u64,
    rpc_arg: Option<&str>,
    options: CallerOptions,
    registry: Arc<dyn ChainRegistry>,
    connect: Option<Arc<Connector>>,
) -> anyhow::Result<Option<Arc<dyn EthCaller>>> {
    let entries: Vec<&str> = rpc_arg
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();

    if entries.is_empty() {
        return Ok(None);
    }

    let auto = entries.iter().any(|e| e.eq_ignore_ascii_case(AUTO));
    let call_timeout = options.call_timeout();
    let connect = connect.unwrap_or_else(|| http_connector(call_timeout));

    let mut specs = Vec::new();
    for url in entries.iter().filter(|e| !e.eq_ignore_ascii_case(AUTO)) {
        let position = specs.len() + 1;
        specs.push(EndpointSpec {
            key: url.trim_end_matches('/').to_string(),
            // A configured URL may embed an API key: never shown.
            label: format!("#{position}"),
            caller: connect(url)
                .with_context(|| format!("rpc endpoint #{position}"))?,
            discovered: false,
        });
    }
    let configured = specs.len();

    let caller = Arc::new(MultiEndpointCaller::new(
        chain_id,
        specs,
        options.endpoints.clone(),
    ));
    caller.set_refreshable(auto);

    if auto {
        let discovery = Discovery {
            chain_id,
            registry,
            connect,
            options,
            configured,
            warned: AtomicBool::new(false),
        };
        discovery.refresh(&caller).await;
        discovery.keep_fresh(Arc::downgrade(&caller));
    }

    let summary = caller.verify_all().await;
    log_summary(summary, caller.len());

    if !auto && summary.wrong_chain == caller.len() {
        bail!(
            "every configured RPC endpoint serves another chain than \
             {chain_id}, the one being indexed"
        );
    }

    Ok(Some(caller))
}

struct Discovery {
    chain_id: u64,
    registry: Arc<dyn ChainRegistry>,
    connect: Arc<Connector>,
    options: CallerOptions,
    /// Number of configured endpoints (they come first in the labels).
    configured: usize,
    /// A failing discovery is reported once, not on every attempt.
    warned: AtomicBool,
}

impl Discovery {
    /// Runs a discovery and installs what it found. `false` when nothing
    /// was found (the current endpoints are kept).
    async fn refresh(&self, caller: &MultiEndpointCaller) -> bool {
        let found = discover_with(
            self.chain_id,
            &*self.registry,
            &*self.connect,
            &self.options.discovery,
        )
        .await;

        let urls = match found {
            Ok(urls) => urls,
            Err(error) => {
                let error = redact_urls(&format!("{error:#}"));
                if !self.warned.swap(true, Ordering::Relaxed) {
                    warn!(
                        "Unable to discover public RPC endpoints for \
                         chain {}, trying again later: {error}",
                        self.chain_id
                    );
                } else {
                    debug!(
                        "Still unable to discover public RPC endpoints \
                         for chain {}: {error}",
                        self.chain_id
                    );
                }
                return false;
            }
        };
        self.warned.store(false, Ordering::Relaxed);

        info!(
            "Discovered {} public RPC endpoints for chain {}: {}",
            urls.len(),
            self.chain_id,
            urls.join(", ")
        );

        let specs = urls
            .iter()
            .enumerate()
            .filter_map(|(index, url)| {
                let position = self.configured + index + 1;
                let host = url::Url::parse(url)
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_string))
                    .unwrap_or_default();

                Some(EndpointSpec {
                    key: url.clone(),
                    label: format!("#{position} ({host})"),
                    caller: (self.connect)(url).ok()?,
                    discovered: true,
                })
            })
            .collect();

        caller.replace_discovered(specs);
        true
    }

    /// Background task replacing the public endpoints when they stop
    /// working. Ends by itself when the caller is dropped.
    fn keep_fresh(self, caller: Weak<MultiEndpointCaller>) {
        tokio::spawn(async move {
            let mut last_refresh = Instant::now();

            loop {
                tokio::time::sleep(
                    self.options.discovery.refresh_check_interval,
                )
                .await;

                let Some(caller) = caller.upgrade() else {
                    return;
                };

                let Some(health) = caller.health() else {
                    return;
                };

                let none_left = health.endpoints_healthy == 0;
                let mostly_dead = health.endpoints_healthy * 2
                    < health.endpoints_total
                    && last_refresh.elapsed()
                        >= self.options.discovery.refresh_min_interval;

                if (none_left || mostly_dead)
                    && self.refresh(&caller).await
                {
                    last_refresh = Instant::now();
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::multicall::{
        testing::{FakeChain, FakeToken},
        CallError,
    };
    use alloy::primitives::{Address, Bytes};
    use std::{
        collections::HashMap,
        sync::{atomic::AtomicUsize, Mutex},
    };

    /// Shaped like the real registry, including its traps.
    const FIXTURE: &str = r#"[
      {
        "name": "Ethereum Mainnet",
        "chain": "ETH",
        "rpc": [
          "https://mainnet.infura.io/v3/${INFURA_API_KEY}",
          "wss://mainnet.infura.io/ws/v3/${INFURA_API_KEY}",
          "https://api.mycryptoapi.com/eth",
          "https://cloudflare-eth.com",
          "https://cloudflare-eth.com/",
          "https://cloudflare-eth.com/v1/mainnet",
          "https://CLOUDFLARE-eth.com",
          "wss://ethereum-rpc.publicnode.com",
          "http://plain-http.example.org",
          "https://ethereum-rpc.publicnode.com",
          "https://user:password@private.example.org",
          "https://rpc.example.org/{API_KEY}",
          "https://rpc.example.org/<key>",
          "https://127.0.0.1:8545",
          "https://localhost:8545",
          "https://intranet",
          "not a url",
          "",
          42,
          { "url": "https://object-style.example.org/rpc", "tracking": "none" },
          "https://eth.llamarpc.com/path?query=1"
        ],
        "features": [{ "name": "EIP155" }],
        "nativeCurrency": { "name": "Ether", "symbol": "ETH", "decimals": 18 },
        "chainId": 1,
        "networkId": 1
      },
      { "name": "Broken entry", "chainId": "0x1", "rpc": "https://nope.example.org" },
      { "name": "No rpc field", "chainId": 5 },
      {
        "name": "OP Mainnet",
        "rpc": ["https://mainnet.optimism.io", "https://optimism-rpc.publicnode.com"],
        "chainId": 10
      },
      {
        "name": "Second entry for chain 10",
        "rpc": ["https://mainnet.optimism.io/", "https://op.example.org"],
        "chainId": 10
      }
    ]"#;

    #[test]
    fn selects_only_public_https_endpoints() {
        assert_eq!(
            select_rpc_urls(FIXTURE.as_bytes(), 1).unwrap(),
            vec![
                "https://api.mycryptoapi.com/eth",
                "https://cloudflare-eth.com",
                "https://cloudflare-eth.com/v1/mainnet",
                "https://ethereum-rpc.publicnode.com",
                "https://object-style.example.org/rpc",
                "https://eth.llamarpc.com/path?query=1",
            ]
        );

        // Several registry entries for one chain are merged and deduped.
        assert_eq!(
            select_rpc_urls(FIXTURE.as_bytes(), 10).unwrap(),
            vec![
                "https://mainnet.optimism.io",
                "https://optimism-rpc.publicnode.com",
                "https://op.example.org",
            ]
        );

        // Unknown chain / chain without endpoints: nothing, not an error.
        assert!(select_rpc_urls(FIXTURE.as_bytes(), 5)
            .unwrap()
            .is_empty());
        assert!(select_rpc_urls(FIXTURE.as_bytes(), 777)
            .unwrap()
            .is_empty());

        // Garbage is an error, not a panic.
        assert!(select_rpc_urls(b"<html>rate limited</html>", 1).is_err());
        assert!(select_rpc_urls(b"{}", 1).is_err());
    }

    struct FixtureRegistry {
        json: String,
        down: AtomicBool,
        fetches: AtomicUsize,
    }

    impl FixtureRegistry {
        fn new(json: &str) -> Arc<Self> {
            Arc::new(Self {
                json: json.to_string(),
                down: AtomicBool::new(false),
                fetches: AtomicUsize::new(0),
            })
        }
    }

    impl ChainRegistry for FixtureRegistry {
        fn fetch(&self) -> BoxFuture<'_, anyhow::Result<Vec<u8>>> {
            Box::pin(async move {
                self.fetches.fetch_add(1, Ordering::SeqCst);
                if self.down.load(Ordering::SeqCst) {
                    bail!("dns error for https://chainid.network/secret");
                }
                Ok(self.json.clone().into_bytes())
            })
        }
    }

    /// A fake node with a latency on `eth_chainId`.
    struct Node {
        chain: Arc<FakeChain>,
        latency: Duration,
    }

    impl EthCaller for Node {
        fn call(
            &self,
            to: Address,
            data: Bytes,
        ) -> BoxFuture<'_, Result<Bytes, CallError>> {
            self.chain.call(to, data)
        }

        fn chain_id(&self) -> BoxFuture<'_, Result<u64, CallError>> {
            Box::pin(async move {
                tokio::time::sleep(self.latency).await;
                self.chain.chain_id().await
            })
        }
    }

    /// The "internet": url -> node. Unknown URLs never answer.
    #[derive(Default)]
    struct Internet {
        nodes: Mutex<HashMap<String, (Arc<FakeChain>, Duration)>>,
    }

    impl Internet {
        fn add(
            &self,
            url: &str,
            chain_id: u64,
            latency_ms: u64,
        ) -> Arc<FakeChain> {
            let chain = FakeChain::new();
            chain.chain_id.store(chain_id, Ordering::SeqCst);
            chain.add(
                Address::repeat_byte(1),
                FakeToken::erc20("USD Coin", "USDC", 6),
            );
            self.nodes.lock().unwrap().insert(
                url.to_string(),
                (chain.clone(), Duration::from_millis(latency_ms)),
            );
            chain
        }

        fn connector(self: &Arc<Self>) -> Arc<Connector> {
            let internet = self.clone();
            Arc::new(move |url: &str| {
                if url.contains(' ') {
                    bail!("invalid url");
                }
                let nodes = internet.nodes.lock().unwrap();
                let (chain, latency) = match nodes.get(url) {
                    Some((chain, latency)) => (chain.clone(), *latency),
                    None => {
                        let dead = FakeChain::new();
                        dead.offline.store(true, Ordering::SeqCst);
                        (dead, Duration::ZERO)
                    }
                };
                Ok(Arc::new(Node { chain, latency }) as Arc<dyn EthCaller>)
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_orders_by_latency_and_drops_dead_and_wrong_chain() {
        let internet = Arc::new(Internet::default());
        internet.add("https://api.mycryptoapi.com/eth", 1, 300);
        internet.add("https://cloudflare-eth.com", 1, 40);
        // Same provider under another path: one per host is kept.
        internet.add("https://cloudflare-eth.com/v1/mainnet", 1, 90);
        // Listed for chain 1 but actually serving another chain.
        internet.add("https://ethereum-rpc.publicnode.com", 56, 5);
        internet.add("https://eth.llamarpc.com/path?query=1", 1, 120);
        // object-style.example.org is dead.

        let registry = FixtureRegistry::new(FIXTURE);
        let found = discover_with(
            1,
            &*registry,
            &*internet.connector(),
            &DiscoveryOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            found,
            vec![
                "https://cloudflare-eth.com",
                "https://eth.llamarpc.com/path?query=1",
                "https://api.mycryptoapi.com/eth",
            ]
        );

        // Capped.
        let capped = discover_with(
            1,
            &*registry,
            &*internet.connector(),
            &DiscoveryOptions {
                max_endpoints: 2,
                ..DiscoveryOptions::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0], "https://cloudflare-eth.com");

        // Nothing listed / nothing alive are errors of the discovery...
        let connect = internet.connector();
        let options = DiscoveryOptions::default();
        assert!(discover_with(777, &*registry, &*connect, &options)
            .await
            .is_err());
        assert!(discover_with(10, &*registry, &*connect, &options)
            .await
            .is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn build_caller_parses_the_rpc_argument() {
        let internet = Arc::new(Internet::default());
        let a = internet.add("https://a.example.org/v2/KEY", 1, 10);
        let b = internet.add("https://b.example.org", 1, 10);
        let registry = FixtureRegistry::new(FIXTURE);
        let build = |arg: Option<&'static str>| {
            build_caller_with(
                1,
                arg,
                CallerOptions::default(),
                registry.clone(),
                Some(internet.connector()),
            )
        };

        assert!(build(None).await.unwrap().is_none());
        assert!(build(Some("")).await.unwrap().is_none());
        assert!(build(Some(" , ,")).await.unwrap().is_none());

        let caller = build(Some(
            " https://a.example.org/v2/KEY , ,https://b.example.org,\
             https://b.example.org/ ",
        ))
        .await
        .unwrap()
        .unwrap();
        let health = caller.health().unwrap();
        assert_eq!(health.endpoints_total, 2);
        assert_eq!(health.endpoints_healthy, 2);
        assert_eq!(a.chain_id_calls.load(Ordering::SeqCst), 1);
        assert_eq!(b.chain_id_calls.load(Ordering::SeqCst), 1);
        assert_eq!(caller.chain_id().await, Ok(1));
        // No discovery unless asked for.
        assert_eq!(registry.fetches.load(Ordering::SeqCst), 0);

        // Invalid URL: a configuration error that does not echo it.
        let error =
            build(Some("https://a.example.org/v2/KEY,bad url KEY2"))
                .await
                .err()
                .map(|error| format!("{error:#}"))
                .unwrap_or_default();
        assert!(error.contains("#2"), "{error}");
        assert!(!error.contains("KEY2"), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn configured_endpoints_on_the_wrong_chain() {
        let internet = Arc::new(Internet::default());
        internet.add("https://bsc.example.org", 56, 10);
        internet.add("https://eth.example.org", 1, 10);
        let registry = FixtureRegistry::new(FIXTURE);
        let build = |arg: &'static str| {
            build_caller_with(
                1,
                Some(arg),
                CallerOptions::default(),
                registry.clone(),
                Some(internet.connector()),
            )
        };

        // All of them: refuse to start (same as a single wrong RPC did).
        assert!(build("https://bsc.example.org").await.is_err());

        // Some of them: disabled, the rest works.
        let caller =
            build("https://bsc.example.org,https://eth.example.org")
                .await
                .unwrap()
                .unwrap();
        let health = caller.health().unwrap();
        assert_eq!(health.endpoints_wrong_chain, 1);
        assert_eq!(health.endpoints_healthy, 1);

        // Unreachable is not wrong: it may come back.
        let caller =
            build("https://down.example.org").await.unwrap().unwrap();
        assert_eq!(caller.health().unwrap().endpoints_wrong_chain, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn auto_discovers_and_mixes_with_configured_endpoints() {
        let internet = Arc::new(Internet::default());
        internet.add("https://mine.example.org/KEY", 1, 10);
        internet.add("https://cloudflare-eth.com", 1, 40);
        internet.add("https://eth.llamarpc.com/path?query=1", 1, 120);
        let registry = FixtureRegistry::new(FIXTURE);

        let caller = build_caller_with(
            1,
            Some("https://mine.example.org/KEY,AUTO"),
            CallerOptions::default(),
            registry.clone(),
            Some(internet.connector()),
        )
        .await
        .unwrap()
        .unwrap();

        let health = caller.health().unwrap();
        assert_eq!(health.endpoints_total, 3);
        assert_eq!(health.endpoints_healthy, 3);
        assert!(caller
            .call(
                Address::repeat_byte(1),
                Bytes::from(vec![6, 0xfd, 0xde, 3])
            )
            .await
            .is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn auto_never_fails_startup_and_heals_in_the_background() {
        let internet = Arc::new(Internet::default());
        let registry = FixtureRegistry::new(FIXTURE);
        registry.down.store(true, Ordering::SeqCst);

        // Registry unreachable: a caller without endpoints, not an error.
        let caller = build_caller_with(
            1,
            Some("auto"),
            CallerOptions::default(),
            registry.clone(),
            Some(internet.connector()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(caller.health().unwrap().endpoints_total, 0);
        assert!(matches!(
            caller.chain_id().await,
            Err(CallError::Transient(_))
        ));

        // The registry comes back, an endpoint exists: found within a
        // refresh check, without anybody asking.
        registry.down.store(false, Ordering::SeqCst);
        let first = internet.add("https://cloudflare-eth.com", 1, 40);
        tokio::time::sleep(Duration::from_secs(61)).await;
        assert_eq!(caller.health().unwrap().endpoints_total, 1);
        assert_eq!(caller.chain_id().await, Ok(1));

        // That endpoint dies and another one appears: replaced.
        first.offline.store(true, Ordering::SeqCst);
        internet.add("https://api.mycryptoapi.com/eth", 1, 40);
        let call = || {
            caller.call(
                Address::repeat_byte(1),
                Bytes::from(vec![6, 0xfd, 0xde, 3]),
            )
        };
        assert!(call().await.is_err());
        assert_eq!(caller.health().unwrap().endpoints_healthy, 0);
        tokio::time::sleep(Duration::from_secs(61)).await;
        assert!(call().await.is_ok());

        // Dropping the caller ends the background task.
        let fetches = registry.fetches.load(Ordering::SeqCst);
        first.offline.store(true, Ordering::SeqCst);
        drop(caller);
        tokio::time::sleep(Duration::from_secs(600)).await;
        assert_eq!(registry.fetches.load(Ordering::SeqCst), fetches);
    }

    /// Live sanity check of the registry filter and the probes:
    /// `TOKEN_TEST_DISCOVER_CHAINS=1,10 cargo test -- --ignored \
    /// live_discovery --nocapture`
    #[tokio::test]
    #[ignore = "needs the network (chainid.network + public RPCs)"]
    async fn live_discovery() {
        let chains = std::env::var("TOKEN_TEST_DISCOVER_CHAINS")
            .unwrap_or_else(|_| "1,10".to_string());

        let json = HttpChainRegistry::default().fetch().await.unwrap();
        eprintln!("registry: {} bytes", json.len());

        for chain_id in chains.split(',').filter_map(|c| c.parse().ok()) {
            let listed = select_rpc_urls(&json, chain_id).unwrap();
            let found = discover_public_rpcs(chain_id).await.unwrap();
            eprintln!(
                "chain {chain_id}: {} listed after filtering, kept {}:",
                listed.len(),
                found.len()
            );
            for url in &found {
                eprintln!("  {url}");
            }
            assert!(!found.is_empty());
            assert!(found.len() <= 8);
            assert!(found
                .iter()
                .all(|url| url.starts_with("https://")
                    && !url.contains('$')));
        }
    }

    /// `--rpc auto` end to end against Ethereum mainnet.
    #[tokio::test]
    #[ignore = "needs the network (chainid.network + public RPCs)"]
    async fn live_auto_resolves_weth() {
        use crate::tokens::{
            TokenResolver, TokenResolverOptions, TokenStandard,
        };
        use alloy::primitives::address;

        let caller = build_caller(1, Some("auto")).await.unwrap();
        let health = caller.as_ref().and_then(|c| c.health()).unwrap();
        eprintln!("{health:?}");
        assert!(health.endpoints_healthy > 0);

        let resolver = TokenResolver::from_caller(
            1,
            caller,
            None,
            &TokenResolverOptions::default(),
        )
        .unwrap();

        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let bayc = address!("BC4CA0EdA7647A8aB7C2061c2E118A18a936f13D");
        let rows = resolver
            .resolve_new(&HashMap::from([
                (weth, TokenStandard::Erc20),
                (bayc, TokenStandard::Erc721),
            ]))
            .await;
        eprintln!("{rows:?}");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.symbol == "WETH"));
        assert!(rows.iter().any(|row| row.symbol == "BAYC"));
    }
}
