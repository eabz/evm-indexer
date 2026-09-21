//! Builds the RPC backend of token metadata from the `--rpc` argument,
//! including `--rpc auto` (the default): zero-config discovery of public
//! endpoints from the chain registry at <https://chainid.network>.
//!
//! Discovery is best effort by design and never on anybody's critical
//! path: it runs in a background task (startup does not wait for it), an
//! unreachable registry or a chain without public endpoints only means a
//! caller without endpoints for now, and public endpoints that die are
//! replaced the same way.
//!
//! Discovered endpoints are **untrusted** (see [`super::endpoints`]): the
//! list is a community file anybody can get a URL into. They are only
//! ever contacted over https on public addresses, without following
//! redirects, and nothing they say is stored unless a second provider
//! says the same.
//!
//! The registry is a community site too, so it is treated politely: the
//! list is revalidated with `If-None-Match`, shared between indexer
//! processes through Redis when there is one, and a discovery that finds
//! nothing backs off exponentially (with jitter) up to hours.

use std::{
    collections::{hash_map::RandomState, HashMap, HashSet},
    hash::{BuildHasher, Hasher},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context};
use futures::{future::BoxFuture, stream, StreamExt};
use log::{debug, info, warn};
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use serde::Deserialize;
use tokio::time::Instant;

use super::{
    endpoints::{
        log_summary, provider_of, spec_for_url, EndpointOptions,
        MultiEndpointCaller,
    },
    http,
    multicall::{EthCaller, FetchOptions, HttpCaller},
    redact::{redact_urls, Redactor},
};

/// The registry behind chainlist.org (ethereum-lists/chains).
pub const CHAIN_REGISTRY_URL: &str = "https://chainid.network/chains.json";

/// The `--rpc` value that turns discovery on.
pub const AUTO: &str = "auto";

/// `--rpc` value that disables every RPC backed feature.
pub const NONE: &str = "none";

/// Tunables of [`discover_with`] / [`build_caller_with`].
#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
    /// Endpoints kept after probing.
    pub max_endpoints: usize,
    /// Registry entries probed at most (in registry order).
    pub max_probed: usize,
    pub probe_timeout: Duration,
    pub probe_concurrency: usize,
    /// How often the health of the discovered endpoints is looked at,
    /// and the first retry delay of a discovery that found nothing.
    pub refresh_check_interval: Duration,
    /// Minimum time between two discoveries when no endpoint works.
    pub refresh_min_interval_empty: Duration,
    /// Minimum time between two discoveries when some endpoints work.
    pub refresh_min_interval: Duration,
    /// A discovery that fails or finds nothing is retried after
    /// `refresh_check_interval`, doubled every time up to this.
    pub max_backoff: Duration,
    /// Spread the retries of many processes (+/- 25%).
    pub jitter: bool,
    /// How long a registry listing shared through Redis is reused.
    pub shared_ttl: Duration,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            max_endpoints: 8,
            max_probed: 24,
            probe_timeout: Duration::from_secs(5),
            probe_concurrency: 8,
            refresh_check_interval: Duration::from_secs(60),
            refresh_min_interval_empty: Duration::from_secs(300),
            refresh_min_interval: Duration::from_secs(1_800),
            max_backoff: Duration::from_secs(6 * 3_600),
            jitter: true,
            shared_ttl: Duration::from_secs(6 * 3_600),
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

/// Where the public RPC URLs of a chain come from (the network, Redis,
/// or a test).
pub trait ChainRegistry: Send + Sync + 'static {
    /// The usable URLs listed for `chain_id` (see [`select_rpc_urls`]),
    /// in listing order. An empty list is not an error.
    fn rpc_urls(
        &self,
        chain_id: u64,
    ) -> BoxFuture<'_, anyhow::Result<Vec<String>>>;
}

/// Turns a URL into a caller (HTTP, or a fake in tests). The flag tells
/// whether the URL was discovered (untrusted) or configured.
pub type Connector = dyn Fn(&str, bool) -> anyhow::Result<Arc<dyn EthCaller>>
    + Send
    + Sync
    + 'static;

/// [`ChainRegistry`] over HTTPS: no redirects, a timeout, a response size
/// cap, and `If-None-Match` so an unchanged list costs a `304`.
pub struct HttpChainRegistry {
    pub url: String,
    pub timeout: Duration,
    pub max_bytes: usize,
    /// Only connect to public addresses (off for tests on localhost).
    pub public_only: bool,
    /// chain id -> (etag, urls) of the last download.
    cache: Mutex<HashMap<u64, (String, Vec<String>)>>,
}

impl Default for HttpChainRegistry {
    fn default() -> Self {
        Self::new(CHAIN_REGISTRY_URL)
    }
}

impl HttpChainRegistry {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            timeout: Duration::from_secs(20),
            // ~1.2 MiB at the time of writing.
            max_bytes: 32 * 1024 * 1024,
            public_only: true,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cache(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<u64, (String, Vec<String>)>>
    {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl ChainRegistry for HttpChainRegistry {
    fn rpc_urls(
        &self,
        chain_id: u64,
    ) -> BoxFuture<'_, anyhow::Result<Vec<String>>> {
        Box::pin(async move {
            let cached = self.cache().get(&chain_id).cloned();

            if self.public_only {
                // IP literals never reach the (public only) resolver.
                let public = url::Url::parse(&self.url).is_ok_and(|url| {
                    url.scheme() == "https"
                        && matches!(url.host(), Some(url::Host::Domain(_)))
                });
                if !public {
                    bail!("the chain registry url must be https://<host>");
                }
            }

            let download = async {
                let mut request =
                    http::client(self.public_only)?.get(&self.url);
                if let Some((etag, _)) = &cached {
                    request = request
                        .header(reqwest::header::IF_NONE_MATCH, etag);
                }

                let mut response = request
                    .send()
                    .await
                    .map_err(|e| anyhow!(redact_urls(&e.to_string())))?;

                let status = response.status();
                if status == reqwest::StatusCode::NOT_MODIFIED {
                    return Ok(None);
                }
                if !status.is_success() {
                    // Redirects included: they are never followed.
                    bail!("the chain registry answered {status}");
                }

                let etag = response
                    .headers()
                    .get(reqwest::header::ETAG)
                    .and_then(|etag| etag.to_str().ok())
                    .map(str::to_string);

                let body =
                    http::read_capped(&mut response, self.max_bytes)
                        .await
                        .map_err(|e| anyhow!(redact_urls(&e)))?;

                Ok(Some((etag, body)))
            };

            // One deadline for the whole download, not per chunk.
            let downloaded = tokio::time::timeout(self.timeout, download)
                .await
                .map_err(|_| {
                    anyhow!(
                        "the chain registry did not answer within {:?}",
                        self.timeout
                    )
                })??;

            match (downloaded, cached) {
                (None, Some((_, urls))) => Ok(urls),
                (None, None) => {
                    bail!("the chain registry answered 304 to a plain GET")
                }
                (Some((etag, body)), _) => {
                    let urls = select_rpc_urls(&body, chain_id)?;
                    if let Some(etag) = etag {
                        self.cache()
                            .insert(chain_id, (etag, urls.clone()));
                    }
                    Ok(urls)
                }
            }
        })
    }
}

/// A small string store shared between indexer processes (Redis).
pub trait SharedStore: Send + Sync + 'static {
    fn get<'a>(
        &'a self,
        key: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<Option<String>>>;

    fn set_ex<'a>(
        &'a self,
        key: &'a str,
        value: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, anyhow::Result<()>>;
}

/// [`SharedStore`] over Redis / Dragonfly (`GET`, `SET .. EX`).
pub struct RedisStore {
    connection: ConnectionManager,
    redactor: Redactor,
}

const REDIS_OP_TIMEOUT: Duration = Duration::from_secs(3);

impl RedisStore {
    /// No I/O: connects on first use. Only an invalid URL is an error.
    /// Must be called from within a tokio runtime.
    pub fn lazy(url: &str) -> anyhow::Result<Self> {
        let redactor = Redactor::for_url(url);

        let client = redis::Client::open(url)
            .map_err(|error| anyhow!(redactor.redact(&error.to_string())))
            .context("invalid redis url")?;

        let config = ConnectionManagerConfig::new()
            .set_number_of_retries(1)
            .set_connection_timeout(Some(Duration::from_secs(2)))
            .set_response_timeout(Some(Duration::from_secs(2)));

        let connection =
            ConnectionManager::new_lazy_with_config(client, config)
                .map_err(|error| {
                    anyhow!(redactor.redact(&error.to_string()))
                })?;

        Ok(Self { connection, redactor })
    }

    async fn guarded<T>(
        &self,
        operation: impl std::future::Future<Output = redis::RedisResult<T>>,
    ) -> anyhow::Result<T> {
        match tokio::time::timeout(REDIS_OP_TIMEOUT, operation).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                Err(anyhow!(self.redactor.redact(&error.to_string())))
            }
            Err(_) => Err(anyhow!("redis operation timed out")),
        }
    }
}

impl SharedStore for RedisStore {
    fn get<'a>(
        &'a self,
        key: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<Option<String>>> {
        Box::pin(async move {
            let mut conn = self.connection.clone();
            self.guarded(async move {
                redis::cmd("GET").arg(key).query_async(&mut conn).await
            })
            .await
        })
    }

    fn set_ex<'a>(
        &'a self,
        key: &'a str,
        value: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let mut conn = self.connection.clone();
            self.guarded(async move {
                redis::cmd("SET")
                    .arg(key)
                    .arg(value)
                    .arg("EX")
                    .arg(ttl.as_secs().max(1))
                    .query_async::<()>(&mut conn)
                    .await
            })
            .await
        })
    }
}

/// Redis key of the shared registry listing of a chain.
pub fn shared_registry_key(chain_id: u64) -> String {
    format!("evm-indexer:rpc-registry:{chain_id}")
}

/// A [`ChainRegistry`] that asks a [`SharedStore`] first, so that a fleet
/// of indexers downloads the registry once per `ttl` instead of once per
/// process. The store failing only means asking the registry directly.
pub struct SharedRegistry {
    pub inner: Arc<dyn ChainRegistry>,
    pub store: Arc<dyn SharedStore>,
    pub ttl: Duration,
}

impl ChainRegistry for SharedRegistry {
    fn rpc_urls(
        &self,
        chain_id: u64,
    ) -> BoxFuture<'_, anyhow::Result<Vec<String>>> {
        Box::pin(async move {
            let key = shared_registry_key(chain_id);

            match self.store.get(&key).await {
                Ok(Some(json)) => {
                    // Same filter again: the store is not the judge of
                    // what is safe to connect to.
                    let urls: Vec<String> =
                        serde_json::from_str::<Vec<String>>(&json)
                            .unwrap_or_default()
                            .iter()
                            .filter_map(|url| public_https_url(url))
                            .collect();
                    if !urls.is_empty() {
                        return Ok(urls);
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    debug!("Shared RPC registry lookup failed: {error}")
                }
            }

            let urls = self.inner.rpc_urls(chain_id).await?;

            if !urls.is_empty() {
                let json = serde_json::to_string(&urls)?;
                if let Err(error) =
                    self.store.set_ex(&key, &json, self.ttl).await
                {
                    debug!("Shared RPC registry write failed: {error}");
                }
            }

            Ok(urls)
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
/// Kept: `https://` URLs with a public looking domain name. Dropped:
/// templates that need an API key (`${INFURA_API_KEY}` and friends),
/// websockets, plain http, URLs with credentials, IP literals, localhost
/// and internal-only names (`.internal`, `.lan`, `.local`...).
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

/// Names that only mean something inside somebody's network.
const INTERNAL_SUFFIXES: &[&str] = &[
    "localhost",
    "localdomain",
    "local",
    "internal",
    "intranet",
    "private",
    "lan",
    "corp",
    "home",
    "home.arpa",
    "test",
    "invalid",
    "example",
    "onion",
];

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

    let mut url = url::Url::parse(raw).ok()?;

    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }

    let domain = match url.host()? {
        // `localhost.` is `localhost`.
        url::Host::Domain(domain) => {
            domain.trim_end_matches('.').to_ascii_lowercase()
        }
        _ => return None,
    };

    let internal = INTERNAL_SUFFIXES.iter().any(|suffix| {
        domain == *suffix || domain.ends_with(&format!(".{suffix}"))
    });

    if internal || !domain.contains('.') || domain.starts_with('.') {
        return None;
    }

    url.set_host(Some(&domain)).ok()?;

    Some(url.as_str().trim_end_matches('/').to_string())
}

/// Asks every URL for its chain id (concurrently, one attempt each) and
/// returns the ones serving `chain_id`, fastest first, one per provider.
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
            let caller = connect(&url, true).ok()?;
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

    // One endpoint per provider (the fastest): answers are only stored
    // when two providers agree, so two URLs of one provider are no use.
    let mut providers = HashSet::new();
    alive
        .into_iter()
        .map(|(_, url)| url)
        .filter(|url| providers.insert(provider_of(url)))
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
    let listed = registry
        .rpc_urls(chain_id)
        .await
        .context("fetch the chain registry")?;

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
    Arc::new(move |url: &str, discovered: bool| {
        let caller = if discovered {
            HttpCaller::public(url, call_timeout)?
        } else {
            HttpCaller::new(url, call_timeout)?
        };
        Ok(Arc::new(caller) as Arc<dyn EthCaller>)
    })
}

/// Public HTTPS JSON-RPC endpoints currently serving `chain_id`, fastest
/// first, one per provider, at most 8. They are public: safe to log.
/// They are also untrusted: see [`super::endpoints`].
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
/// * `None` (or blank): the default, same as `"auto"` (docs/design.md
///   section 4: DEX analytics are on by default and need token decimals);
/// * `"auto"`: public endpoints discovered for `chain_id` in the
///   background (this function does not wait for them) and kept fresh.
///   May be mixed: `"https://mine,auto"` (recommended for production: the
///   configured endpoint is trusted and preferred, the public ones are
///   the second opinion and the fallback);
/// * `"a,b,c"`: those endpoints with failover (blanks ignored);
/// * `"none"`: `Ok(None)`, RPC features are explicitly disabled.
///
/// Errors are configuration mistakes only: an invalid URL, or every
/// configured endpoint answering with another chain id. Endpoints that
/// cannot be reached, and a discovery that finds nothing, are not errors.
///
/// Prefer [`build_caller_shared`] when a Redis URL is configured.
pub async fn build_caller(
    chain_id: u64,
    rpc_arg: Option<&str>,
) -> anyhow::Result<Option<Arc<dyn EthCaller>>> {
    build_caller_shared(chain_id, rpc_arg, None).await
}

/// [`build_caller`] that shares the registry listing between indexer
/// processes through Redis, so a fleet does not hammer the registry.
/// An invalid `redis_url` is an error, an unreachable Redis is not.
pub async fn build_caller_shared(
    chain_id: u64,
    rpc_arg: Option<&str>,
    redis_url: Option<&str>,
) -> anyhow::Result<Option<Arc<dyn EthCaller>>> {
    let options = CallerOptions::default();
    let mut registry: Arc<dyn ChainRegistry> =
        Arc::new(HttpChainRegistry::default());

    if let Some(url) = redis_url {
        registry = Arc::new(SharedRegistry {
            inner: registry,
            store: Arc::new(RedisStore::lazy(url)?),
            ttl: options.discovery.shared_ttl,
        });
    }

    build_caller_with(chain_id, rpc_arg, options, registry, None).await
}

/// [`build_caller`] with explicit tunables and backends (`connect: None`
/// is HTTP).
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

    // Unset / blank is the default: discover public endpoints.
    let entries = if entries.is_empty() { vec![AUTO] } else { entries };

    if entries.iter().any(|e| e.eq_ignore_ascii_case(NONE)) {
        if entries.len() > 1 {
            bail!(
                "`{NONE}` disables RPC features and cannot be combined with \
                 other --rpc entries"
            );
        }
        info!(
            "RPC features disabled (--rpc {NONE}): no token or pool \
             metadata"
        );
        return Ok(None);
    }

    let auto = entries.iter().any(|e| e.eq_ignore_ascii_case(AUTO));
    let call_timeout = options.call_timeout();
    let connect = connect.unwrap_or_else(|| http_connector(call_timeout));

    let mut specs = Vec::new();
    for url in entries.iter().filter(|e| !e.eq_ignore_ascii_case(AUTO)) {
        let position = specs.len() + 1;
        let caller = connect(url, false)
            .with_context(|| format!("rpc endpoint #{position}"))?;
        // A configured URL may embed an API key: its label is `#N`.
        specs.push(spec_for_url(url, position, false, caller));
    }
    let configured = specs.len();

    let caller = Arc::new(MultiEndpointCaller::new(
        chain_id,
        specs,
        options.endpoints.clone(),
    ));
    caller.set_refreshable(auto);

    if configured > 0 {
        let summary = caller.verify_all().await;
        log_summary(summary, caller.len());

        if summary.wrong_chain == caller.len() {
            bail!(
                "every configured RPC endpoint serves another chain than \
                 {chain_id}, the one being indexed"
            );
        }
    }

    if auto {
        if configured == 0 {
            warn!(
                "No RPC endpoint configured: token metadata comes from \
                 public endpoints, which are untrusted. Nothing is stored \
                 unless two independent providers agree; configure your \
                 own endpoint (`--rpc https://...,auto`) for speed and \
                 reliability"
            );
        }

        // In the background: startup never waits for a community site.
        Discovery {
            chain_id,
            registry,
            connect,
            options,
            configured,
            warned: AtomicBool::new(false),
        }
        .keep_fresh(Arc::downgrade(&caller));
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

/// `duration` +/- 25%, so a fleet restarted together spreads out.
fn jittered(duration: Duration) -> Duration {
    // Randomly keyed hasher: the only randomness std offers.
    let random = RandomState::new().build_hasher().finish();
    let permille = 750 + (random % 501) as u32;
    duration.saturating_mul(permille) / 1_000
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

        if self.configured == 0 && urls.len() < 2 {
            warn!(
                "Only one public RPC provider is known for chain {}: \
                 answers of public endpoints are only stored when two \
                 independent providers agree, so NO token metadata will \
                 be stored until there is a second one. Configure your \
                 own endpoint: `--rpc https://...,auto`",
                self.chain_id
            );
        }

        let specs = urls
            .iter()
            .enumerate()
            .filter_map(|(index, url)| {
                let position = self.configured + index + 1;
                let caller = (self.connect)(url, true).ok()?;
                Some(spec_for_url(url, position, true, caller))
            })
            .collect();

        caller.replace_discovered(specs);
        true
    }

    /// Background task: the first discovery, then replacing the public
    /// endpoints when they stop working. Ends by itself when the caller
    /// is dropped.
    fn keep_fresh(self, caller: Weak<MultiEndpointCaller>) {
        tokio::spawn(async move {
            let options = &self.options.discovery;
            let mut last_refresh: Option<Instant> = None;
            let mut failures = 0u32;

            loop {
                let Some(strong) = caller.upgrade() else {
                    return;
                };
                let Some(health) = strong.health() else {
                    return;
                };

                let since = last_refresh
                    .map(|at| at.elapsed())
                    .unwrap_or(Duration::MAX);
                let none_left = health.endpoints_healthy == 0
                    && since >= options.refresh_min_interval_empty;
                let mostly_dead = health.endpoints_healthy * 2
                    < health.endpoints_total
                    && since >= options.refresh_min_interval;

                // After a failure the (growing) delay below is the
                // schedule; after a success the minimum intervals are.
                if last_refresh.is_none()
                    || failures > 0
                    || none_left
                    || mostly_dead
                {
                    if self.refresh(&strong).await {
                        failures = 0;
                        last_refresh = Some(Instant::now());
                    } else {
                        failures = failures.saturating_add(1);
                    }
                }
                drop(strong);

                let delay = if failures == 0 {
                    options.refresh_check_interval
                } else {
                    let doubled = options
                        .refresh_check_interval
                        .saturating_mul(
                            2u32.saturating_pow(failures.min(16) - 1),
                        )
                        .min(options.max_backoff);
                    if options.jitter {
                        jittered(doubled)
                    } else {
                        doubled
                    }
                };

                tokio::time::sleep(delay).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::multicall::Route;
    use crate::tokens::multicall::{
        testing::{FakeChain, FakeToken},
        CallError,
    };
    use alloy::primitives::{Address, Bytes};
    use std::{collections::HashMap, sync::Mutex};

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

    #[test]
    fn internal_and_odd_hosts_are_not_public() {
        let rejected = [
            "https://localhost.",
            "https://localhost.:8545/",
            "https://node.localhost",
            "https://rpc.internal",
            "https://rpc.corp.internal/path",
            "https://clickhouse.lan:8123",
            "https://node.corp",
            "https://router.home.arpa",
            "https://printer.local",
            "https://printer.local.",
            "https://rpc.intranet",
            "https://node.test",
            "https://abcdef.onion",
            "https://169.254.169.254/latest/meta-data",
            "https://[::1]:8545",
            "https://[fd00::1]",
            "https://0x7f000001",
            "https://2130706433",
            "https://.example.org",
            "http://rpc.example.org",
            "wss://rpc.example.org",
            "https://rpc.example.org/${KEY}",
        ];
        for url in rejected {
            assert_eq!(public_https_url(url), None, "{url}");
        }

        // The trailing dot of a fully qualified name is dropped, so it
        // cannot be used to dodge the checks (or the dedupe) either.
        assert_eq!(
            public_https_url("https://RPC.Example.org./v1/").as_deref(),
            Some("https://rpc.example.org/v1")
        );
        assert_eq!(
            public_https_url("https://internal.example.org").as_deref(),
            Some("https://internal.example.org")
        );
    }

    struct FixtureRegistry {
        json: String,
        down: AtomicBool,
        /// Makes `rpc_urls` take this long.
        delay: Mutex<Duration>,
        fetched_at: Mutex<Vec<Instant>>,
    }

    impl FixtureRegistry {
        fn new(json: &str) -> Arc<Self> {
            Arc::new(Self {
                json: json.to_string(),
                down: AtomicBool::new(false),
                delay: Mutex::new(Duration::ZERO),
                fetched_at: Mutex::new(Vec::new()),
            })
        }

        fn fetches(&self) -> usize {
            self.fetched_at.lock().unwrap().len()
        }

        /// Seconds between consecutive fetches.
        fn gaps(&self) -> Vec<u64> {
            let at = self.fetched_at.lock().unwrap();
            at.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect()
        }
    }

    impl ChainRegistry for FixtureRegistry {
        fn rpc_urls(
            &self,
            chain_id: u64,
        ) -> BoxFuture<'_, anyhow::Result<Vec<String>>> {
            Box::pin(async move {
                self.fetched_at.lock().unwrap().push(Instant::now());
                let delay = *self.delay.lock().unwrap();
                tokio::time::sleep(delay).await;
                if self.down.load(Ordering::SeqCst) {
                    bail!("dns error for https://chainid.network/secret");
                }
                select_rpc_urls(self.json.as_bytes(), chain_id)
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
        /// url -> connected as a discovered (untrusted) endpoint.
        connected_as_discovered: Mutex<HashMap<String, bool>>,
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
            Arc::new(move |url: &str, discovered: bool| {
                if url.contains(' ') {
                    bail!("invalid url");
                }
                internet
                    .connected_as_discovered
                    .lock()
                    .unwrap()
                    .insert(url.to_string(), discovered);
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

    fn exact_schedule() -> CallerOptions {
        CallerOptions {
            discovery: DiscoveryOptions {
                jitter: false,
                ..DiscoveryOptions::default()
            },
            ..CallerOptions::default()
        }
    }

    async fn settle(seconds: u64) {
        tokio::time::sleep(Duration::from_secs(seconds)).await;
    }

    fn name_call() -> Bytes {
        Bytes::from(vec![0x06, 0xfd, 0xde, 0x03])
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_orders_by_latency_and_drops_dead_and_wrong_chain() {
        let internet = Arc::new(Internet::default());
        internet.add("https://api.mycryptoapi.com/eth", 1, 300);
        internet.add("https://cloudflare-eth.com", 1, 40);
        // Same provider under another path: one per provider is kept.
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
        // Probed as what they are: untrusted.
        assert!(internet
            .connected_as_discovered
            .lock()
            .unwrap()
            .values()
            .all(|discovered| *discovered));

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

        // `none` is the only way to switch RPC features off.
        assert!(build(Some("none")).await.unwrap().is_none());
        assert!(build(Some(" NONE ")).await.unwrap().is_none());
        // ...and it cannot be combined with endpoints.
        let error = build(Some("none,https://b.example.org"))
            .await
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default();
        assert!(error.contains("cannot be combined"), "{error}");

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
        // Configured endpoints are connected as trusted ones.
        assert!(internet
            .connected_as_discovered
            .lock()
            .unwrap()
            .values()
            .all(|discovered| !*discovered));

        // No discovery unless asked for.
        settle(3_600).await;
        assert_eq!(registry.fetches(), 0);

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
    async fn an_unset_or_blank_rpc_argument_means_auto() {
        for arg in [None, Some(""), Some(" , ,")] {
            let internet = Arc::new(Internet::default());
            let registry = FixtureRegistry::new(FIXTURE);
            let caller = build_caller_with(
                1,
                arg,
                CallerOptions::default(),
                registry.clone(),
                Some(internet.connector()),
            )
            .await
            .unwrap();

            // A caller exists and discovery runs, exactly as for "auto".
            assert!(caller.is_some(), "{arg:?}");
            settle(1).await;
            assert!(registry.fetches() >= 1, "{arg:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn configured_endpoints_on_the_wrong_chain() {
        let internet = Arc::new(Internet::default());
        internet.add("https://bsc.example.org", 56, 10);
        internet.add("https://eth.example.net", 1, 10);
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

        // Some of them: set aside, the rest works.
        let caller =
            build("https://bsc.example.org,https://eth.example.net")
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
    async fn startup_does_not_wait_for_discovery() {
        let internet = Arc::new(Internet::default());
        internet.add("https://cloudflare-eth.com", 1, 40);
        let registry = FixtureRegistry::new(FIXTURE);
        // A registry that takes its time.
        *registry.delay.lock().unwrap() = Duration::from_secs(15);

        let started = Instant::now();
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

        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(caller.health().unwrap().endpoints_total, 0);
        assert!(matches!(
            caller.chain_id().await,
            Err(CallError::Transient(_))
        ));

        settle(30).await;
        assert_eq!(caller.health().unwrap().endpoints_total, 1);
        assert_eq!(caller.chain_id().await, Ok(1));
    }

    #[tokio::test(start_paused = true)]
    async fn auto_mixes_with_configured_endpoints_which_stay_trusted() {
        let internet = Arc::new(Internet::default());
        let mine = internet.add("https://mine.example.org/KEY", 1, 500);
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
        settle(5).await;

        let health = caller.health().unwrap();
        assert_eq!(health.endpoints_total, 3);
        assert_eq!(health.endpoints_healthy, 3);
        assert_eq!(caller.source_count(), 3);

        let flags =
            internet.connected_as_discovered.lock().unwrap().clone();
        assert!(!flags["https://mine.example.org/KEY"]);
        assert!(flags["https://cloudflare-eth.com"]);

        // The configured endpoint answers although it is the slowest,
        // and says so.
        let route = Route::default();
        for _ in 0..5 {
            let routed = caller
                .call_routed(Address::repeat_byte(1), name_call(), &route)
                .await
                .unwrap();
            assert!(routed.source.trusted);
        }
        assert_eq!(mine.attempts.load(Ordering::SeqCst), 5);

        // The public ones are the second opinion, and are not trusted.
        let first = caller
            .call_routed(Address::repeat_byte(1), name_call(), &route)
            .await
            .unwrap();
        let elsewhere = Route {
            exclude_groups: vec![first.source.group],
            ..Route::default()
        };
        let second = caller
            .call_routed(Address::repeat_byte(1), name_call(), &elsewhere)
            .await
            .unwrap();
        assert!(!second.source.trusted);
        assert_ne!(second.source.group, first.source.group);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_discovery_backs_off_up_to_hours() {
        let internet = Arc::new(Internet::default());
        let registry = FixtureRegistry::new(FIXTURE);
        registry.down.store(true, Ordering::SeqCst);

        let caller = build_caller_with(
            1,
            Some("auto"),
            exact_schedule(),
            registry.clone(),
            Some(internet.connector()),
        )
        .await
        .unwrap()
        .unwrap();

        settle(48 * 3_600).await;
        let gaps = registry.gaps();
        assert_eq!(
            gaps[..10],
            [60, 120, 240, 480, 960, 1_920, 3_840, 7_680, 15_360, 21_600]
        );
        assert!(gaps[10..].iter().all(|gap| *gap == 21_600), "{gaps:?}");
        // 48 hours of a dead registry: a dozen requests or so, not 2880.
        assert!(registry.fetches() <= 17, "{}", registry.fetches());

        // Listed but nothing answers (a chain whose public endpoints are
        // all dead) backs off just the same.
        let empty = FixtureRegistry::new(FIXTURE);
        let _caller = build_caller_with(
            10,
            Some("auto"),
            exact_schedule(),
            empty.clone(),
            Some(internet.connector()),
        )
        .await
        .unwrap();
        settle(3_600).await;
        assert_eq!(empty.gaps(), [60, 120, 240, 480, 960]);

        // With jitter every delay is within 25% of the schedule, and a
        // fleet does not move in lock step.
        let jittery = FixtureRegistry::new(FIXTURE);
        jittery.down.store(true, Ordering::SeqCst);
        let _caller = build_caller_with(
            1,
            Some("auto"),
            CallerOptions::default(),
            jittery.clone(),
            Some(internet.connector()),
        )
        .await
        .unwrap();
        settle(3_600).await;
        let expected = [60u64, 120, 240, 480, 960];
        let gaps = jittery.gaps();
        assert!(gaps.len() >= 4, "{gaps:?}");
        for (gap, expected) in gaps.iter().zip(expected) {
            assert!(
                *gap * 100 >= expected * 74
                    && *gap * 100 <= expected * 126,
                "{gaps:?}"
            );
        }

        drop(caller);
    }

    #[tokio::test(start_paused = true)]
    async fn auto_heals_in_the_background_and_replaces_dead_endpoints() {
        let internet = Arc::new(Internet::default());
        let registry = FixtureRegistry::new(FIXTURE);
        registry.down.store(true, Ordering::SeqCst);

        // Registry unreachable: a caller without endpoints, not an error.
        let caller = build_caller_with(
            1,
            Some("auto"),
            exact_schedule(),
            registry.clone(),
            Some(internet.connector()),
        )
        .await
        .unwrap()
        .unwrap();
        settle(1).await;
        assert_eq!(caller.health().unwrap().endpoints_total, 0);

        // The registry comes back, an endpoint exists: found at the next
        // attempt, without anybody asking.
        registry.down.store(false, Ordering::SeqCst);
        let first = internet.add("https://cloudflare-eth.com", 1, 40);
        settle(61).await;
        assert_eq!(caller.health().unwrap().endpoints_total, 1);
        assert_eq!(caller.chain_id().await, Ok(1));

        // That endpoint dies and another one appears: replaced, but not
        // before the minimum interval (5 minutes) since the last listing.
        first.offline.store(true, Ordering::SeqCst);
        internet.add("https://api.mycryptoapi.com/eth", 1, 40);
        let call = || caller.call(Address::repeat_byte(1), name_call());
        assert!(call().await.is_err());
        assert_eq!(caller.health().unwrap().endpoints_healthy, 0);
        let fetches = registry.fetches();
        settle(120).await;
        assert_eq!(registry.fetches(), fetches);
        settle(300).await;
        assert_eq!(registry.fetches(), fetches + 1);
        assert!(call().await.is_ok());

        // Dropping the caller ends the background task.
        let fetches = registry.fetches();
        first.offline.store(true, Ordering::SeqCst);
        drop(caller);
        settle(24 * 3_600).await;
        assert_eq!(registry.fetches(), fetches);
    }

    #[derive(Default)]
    struct FakeStore {
        entries: Mutex<HashMap<String, (String, Duration)>>,
        down: AtomicBool,
    }

    impl SharedStore for FakeStore {
        fn get<'a>(
            &'a self,
            key: &'a str,
        ) -> BoxFuture<'a, anyhow::Result<Option<String>>> {
            Box::pin(async move {
                if self.down.load(Ordering::SeqCst) {
                    bail!("connection refused");
                }
                let entries = self.entries.lock().unwrap();
                Ok(entries.get(key).map(|(value, _)| value.clone()))
            })
        }

        fn set_ex<'a>(
            &'a self,
            key: &'a str,
            value: &'a str,
            ttl: Duration,
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                if self.down.load(Ordering::SeqCst) {
                    bail!("connection refused");
                }
                self.entries
                    .lock()
                    .unwrap()
                    .insert(key.to_string(), (value.to_string(), ttl));
                Ok(())
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_registry_listing_is_shared_between_processes() {
        let store = Arc::new(FakeStore::default());
        let site = FixtureRegistry::new(FIXTURE);
        let process = || SharedRegistry {
            inner: site.clone(),
            store: store.clone(),
            ttl: Duration::from_secs(6 * 3_600),
        };

        // Fifty processes, one download.
        let mut listings = Vec::new();
        for _ in 0..50 {
            listings.push(process().rpc_urls(1).await.unwrap());
        }
        assert_eq!(site.fetches(), 1);
        assert!(listings.iter().all(|urls| urls == &listings[0]));
        assert_eq!(listings[0].len(), 6);
        {
            let entries = store.entries.lock().unwrap();
            let (_, ttl) = &entries[&shared_registry_key(1)];
            assert_eq!(*ttl, Duration::from_secs(6 * 3_600));
        }
        // Per chain.
        assert_eq!(process().rpc_urls(10).await.unwrap().len(), 3);
        assert_eq!(site.fetches(), 2);

        // What the store says is filtered like the registry itself.
        store.entries.lock().unwrap().insert(
            shared_registry_key(1),
            (
                r#"["https://169.254.169.254/","http://clickhouse:8123",
                   "https://rpc.internal","https://ok.example.org"]"#
                    .to_string(),
                Duration::ZERO,
            ),
        );
        assert_eq!(
            process().rpc_urls(1).await.unwrap(),
            vec!["https://ok.example.org"]
        );

        // Garbage or an unreachable store: straight to the registry.
        store
            .entries
            .lock()
            .unwrap()
            .insert(shared_registry_key(1), ("{".into(), Duration::ZERO));
        assert_eq!(process().rpc_urls(1).await.unwrap().len(), 6);
        store.down.store(true, Ordering::SeqCst);
        assert_eq!(process().rpc_urls(1).await.unwrap().len(), 6);
        assert_eq!(site.fetches(), 4);

        // An empty listing is never shared (it would hide a recovery).
        store.down.store(false, Ordering::SeqCst);
        assert!(process().rpc_urls(777).await.unwrap().is_empty());
        assert!(!store
            .entries
            .lock()
            .unwrap()
            .contains_key(&shared_registry_key(777)));
    }

    #[tokio::test]
    async fn the_registry_is_revalidated_with_its_etag() {
        use crate::tokens::http::testing::{response, serve};

        let server = serve(|number, head| {
            if number == 0 {
                assert!(!head.contains("if-none-match"), "{head}");
                response("200 OK", "etag: \"v1\"\r\n", FIXTURE.as_bytes())
            } else if head.contains("if-none-match: \"v1\"") {
                response("304 Not Modified", "etag: \"v1\"\r\n", b"")
            } else {
                response("500 Oops", "", b"the etag was not sent")
            }
        })
        .await;

        let mut registry = HttpChainRegistry::new(&server.url);
        registry.public_only = false;

        let first = registry.rpc_urls(1).await.unwrap();
        assert_eq!(first.len(), 6);
        // Unchanged: a 304 without a body, same listing.
        assert_eq!(registry.rpc_urls(1).await.unwrap(), first);
        assert_eq!(registry.rpc_urls(1).await.unwrap(), first);
        assert_eq!(server.requests.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn the_registry_fetch_follows_no_redirect_and_is_capped() {
        use crate::tokens::http::testing::{response, serve};

        let internal =
            serve(|_, _| response("200 OK", "", FIXTURE.as_bytes())).await;
        let location =
            format!("location: {}/chains.json\r\n", internal.url);
        let redirecting = serve(move |_, _| {
            response("307 Temporary Redirect", &location, b"")
        })
        .await;

        let mut registry = HttpChainRegistry::new(&redirecting.url);
        registry.public_only = false;
        let error = registry.rpc_urls(1).await.unwrap_err().to_string();
        assert!(error.contains("307"), "{error}");
        assert_eq!(internal.requests.load(Ordering::SeqCst), 0);

        let mut registry = HttpChainRegistry::new(&internal.url);
        registry.public_only = false;
        registry.max_bytes = 512;
        let error = registry.rpc_urls(1).await.unwrap_err().to_string();
        assert!(error.contains("larger than"), "{error}");

        // The real thing refuses to even connect to a private address.
        let registry = HttpChainRegistry::new(&internal.url);
        assert!(registry.rpc_urls(1).await.is_err());
        assert_eq!(internal.requests.load(Ordering::SeqCst), 1);
    }

    /// Live sanity check of the registry filter and the probes:
    /// `TOKEN_TEST_DISCOVER_CHAINS=1,10 cargo test -- --ignored \
    /// live_discovery --nocapture`
    #[tokio::test]
    #[ignore = "needs the network (chainid.network + public RPCs)"]
    async fn live_discovery() {
        let chains = std::env::var("TOKEN_TEST_DISCOVER_CHAINS")
            .unwrap_or_else(|_| "1,10".to_string());
        let registry = HttpChainRegistry::default();

        for chain_id in chains.split(',').filter_map(|c| c.parse().ok()) {
            let listed = registry.rpc_urls(chain_id).await.unwrap();
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

    /// `--rpc auto` end to end against Ethereum mainnet: every row is
    /// backed by two independent public providers.
    #[tokio::test]
    #[ignore = "needs the network (chainid.network + public RPCs)"]
    async fn live_auto_resolves_weth() {
        use crate::tokens::{
            TokenResolver, TokenResolverOptions, TokenStandard,
        };
        use alloy::primitives::address;

        let caller = build_caller(1, Some("auto")).await.unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
        let health = caller.as_ref().and_then(|c| c.health()).unwrap();
        eprintln!("{health:?}");
        assert!(health.endpoints_healthy > 1);

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
