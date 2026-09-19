//! Token caches: the bounded in-process "known tokens" set and the optional
//! Redis (or Dragonfly) backed persistent cache.

use std::{
    collections::HashMap,
    future::Future,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use alloy::primitives::Address;
use anyhow::{anyhow, Context};
use futures::future::BoxFuture;
use log::{info, warn};
use lru::LruCache;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use serde::{Deserialize, Serialize};

use super::redact::Redactor;
use crate::db::models::token::DatabaseToken;

/// Namespace prefix of every key written by the indexer.
pub const REDIS_KEY_PREFIX: &str = "evm-indexer:token";

/// Number of commands sent per Redis pipeline.
const REDIS_PIPELINE_CHUNK: usize = 1_000;

/// Upper bound for any single Redis round trip.
const REDIS_OP_TIMEOUT: Duration = Duration::from_secs(3);

/// After a Redis failure the cache is bypassed for this long, so a dead
/// Redis costs at most one timeout every cool-down instead of one per batch.
const REDIS_COOLDOWN: Duration = Duration::from_secs(5);

/// Redis key of a token: `evm-indexer:token:{chain_id}:{0xaddress}` with
/// the address in lowercase hex.
pub fn redis_key(chain_id: u64, address: &Address) -> String {
    format!("{REDIS_KEY_PREFIX}:{chain_id}:{address:#x}")
}

/// JSON value stored in Redis for every token, readable by an API layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedToken {
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
    pub r#type: String,
}

impl CachedToken {
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    pub fn from_json(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }

    pub fn into_database_token(
        self,
        address: Address,
        chain: u64,
    ) -> DatabaseToken {
        DatabaseToken {
            address,
            name: self.name,
            symbol: self.symbol,
            decimals: self.decimals,
            r#type: self.r#type,
            chain,
        }
    }
}

impl From<&DatabaseToken> for CachedToken {
    fn from(token: &DatabaseToken) -> Self {
        Self {
            name: token.name.clone(),
            symbol: token.symbol.clone(),
            decimals: token.decimals,
            r#type: token.r#type.clone(),
        }
    }
}

/// Persistent token cache shared between indexer restarts / instances.
///
/// Implementations must never panic; every failure is reported as an error
/// and the resolver degrades to memory-only behaviour.
pub trait TokenCache: Send + Sync + 'static {
    /// For every address, whether the token is already stored. The result
    /// has the same length and order as `addresses`.
    fn contains_many<'a>(
        &'a self,
        addresses: &'a [Address],
    ) -> BoxFuture<'a, anyhow::Result<Vec<bool>>>;

    /// Persists the metadata of tokens that are durably stored in the DB.
    fn store_many<'a>(
        &'a self,
        tokens: &'a [DatabaseToken],
    ) -> BoxFuture<'a, anyhow::Result<()>>;
}

/// Redis / Dragonfly implementation of [`TokenCache`].
///
/// Uses only `EXISTS`, `SET` and `PING`, all pipelined, so it works on any
/// wire compatible server.
pub struct RedisTokenCache {
    chain_id: u64,
    connection: ConnectionManager,
    down_until: Mutex<Option<Instant>>,
    is_down: AtomicBool,
    /// The URL may carry a password: errors go through this before they
    /// are returned or logged.
    redactor: Redactor,
}

impl RedisTokenCache {
    /// Creates the cache. Only an invalid URL is an error: an unreachable
    /// server is logged and (re)connected lazily by the connection manager.
    pub async fn connect(
        chain_id: u64,
        url: &str,
    ) -> anyhow::Result<Self> {
        let cache = Self::lazy(chain_id, url)?;
        cache.ping().await;
        Ok(cache)
    }

    /// Same as [`connect`](Self::connect) without any I/O: the server is
    /// contacted on first use (or by [`ping`](Self::ping)). Must be called
    /// from within a tokio runtime.
    pub fn lazy(chain_id: u64, url: &str) -> anyhow::Result<Self> {
        let redactor = Redactor::for_url(url);

        let client = redis::Client::open(url)
            .map_err(|error| anyhow!(redactor.redact(&error.to_string())))
            .context("invalid redis url for the token cache")?;

        // Keep the manager's internal retries short: a batch must never
        // wait long for Redis, the next batch simply tries again.
        let config = ConnectionManagerConfig::new()
            .set_number_of_retries(1)
            .set_min_delay(Duration::from_millis(100))
            .set_max_delay(Duration::from_millis(500))
            .set_connection_timeout(Some(Duration::from_secs(2)))
            .set_response_timeout(Some(Duration::from_secs(2)));

        let connection =
            ConnectionManager::new_lazy_with_config(client, config)
                .map_err(|error| {
                    anyhow!(redactor.redact(&error.to_string()))
                })
                .context(
                    "unable to create the redis connection manager",
                )?;

        Ok(Self {
            chain_id,
            connection,
            down_until: Mutex::new(None),
            is_down: AtomicBool::new(false),
            redactor,
        })
    }

    /// Checks the connection; a failure is logged (once) and otherwise
    /// harmless. Returns whether the server answered.
    pub async fn ping(&self) -> bool {
        let mut conn = self.connection.clone();
        let ping = self
            .guarded(async move {
                redis::cmd("PING").query_async::<String>(&mut conn).await
            })
            .await;

        if ping.is_ok() {
            info!("Token cache connected to redis");
        }

        ping.is_ok()
    }

    fn in_cooldown(&self) -> bool {
        let mut down_until =
            self.down_until.lock().unwrap_or_else(|e| e.into_inner());

        match *down_until {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                *down_until = None;
                false
            }
            None => false,
        }
    }

    /// Runs a Redis operation with a hard timeout and a circuit breaker.
    async fn guarded<T, F>(&self, operation: F) -> anyhow::Result<T>
    where
        F: Future<Output = redis::RedisResult<T>>,
    {
        if self.in_cooldown() {
            return Err(anyhow!("redis is cooling down after a failure"));
        }

        let result = match tokio::time::timeout(
            REDIS_OP_TIMEOUT,
            operation,
        )
        .await
        {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                Err(anyhow!(self.redactor.redact(&error.to_string())))
            }
            Err(_) => Err(anyhow!("redis operation timed out")),
        };

        match &result {
            Ok(_) => {
                if self.is_down.swap(false, Ordering::Relaxed) {
                    info!("Token cache redis connection recovered");
                }
            }
            Err(error) => {
                *self
                    .down_until
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) =
                    Some(Instant::now() + REDIS_COOLDOWN);

                if !self.is_down.swap(true, Ordering::Relaxed) {
                    warn!(
                        "Token cache redis unavailable, degrading to \
                         memory-only until it recovers: {error}"
                    );
                }
            }
        }

        result
    }
}

impl TokenCache for RedisTokenCache {
    fn contains_many<'a>(
        &'a self,
        addresses: &'a [Address],
    ) -> BoxFuture<'a, anyhow::Result<Vec<bool>>> {
        Box::pin(async move {
            let mut found = Vec::with_capacity(addresses.len());

            for chunk in addresses.chunks(REDIS_PIPELINE_CHUNK) {
                let mut pipe = redis::pipe();
                for address in chunk {
                    pipe.cmd("EXISTS")
                        .arg(redis_key(self.chain_id, address));
                }

                let mut conn = self.connection.clone();
                let exists: Vec<bool> = self
                    .guarded(
                        async move { pipe.query_async(&mut conn).await },
                    )
                    .await?;

                if exists.len() != chunk.len() {
                    return Err(anyhow!(
                        "redis returned {} replies for {} commands",
                        exists.len(),
                        chunk.len()
                    ));
                }

                found.extend(exists);
            }

            Ok(found)
        })
    }

    fn store_many<'a>(
        &'a self,
        tokens: &'a [DatabaseToken],
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            for chunk in tokens.chunks(REDIS_PIPELINE_CHUNK) {
                let mut pipe = redis::pipe();
                for token in chunk {
                    let value = CachedToken::from(token).to_json()?;
                    pipe.cmd("SET")
                        .arg(redis_key(token.chain, &token.address))
                        .arg(value)
                        .ignore();
                }

                let mut conn = self.connection.clone();
                self.guarded(async move {
                    pipe.query_async::<()>(&mut conn).await
                })
                .await?;
            }

            Ok(())
        })
    }
}

/// In-process view of which tokens need no work: a bounded LRU of tokens
/// known to be stored plus the set of tokens currently "in flight"
/// (claimed by a `resolve_new` call and not yet confirmed by
/// `mark_stored`).
///
/// It also remembers, for a short while and never persistently, the
/// addresses that had no code when they were fetched ("empty"): they are
/// not definitive (the node may lag behind the indexed head) but must not
/// be refetched on every sighting either.
///
/// Purely synchronous; callers wrap it in a mutex.
pub struct KnownTokens {
    known: LruCache<Address, ()>,
    in_flight: HashMap<Address, Instant>,
    in_flight_ttl: Duration,
    prune_at: usize,
    empty: LruCache<Address, Instant>,
    empty_ttl: Duration,
    /// How many fetches in a row found no code at an address.
    empty_strikes: LruCache<Address, u32>,
}

const MIN_PRUNE_AT: usize = 1_024;

/// Defaults of the "empty" negative cache.
pub const DEFAULT_EMPTY_CAPACITY: usize = 50_000;
pub const DEFAULT_EMPTY_TTL: Duration = Duration::from_secs(600);

impl KnownTokens {
    pub fn new(capacity: usize, in_flight_ttl: Duration) -> Self {
        let capacity =
            NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN);

        Self {
            known: LruCache::new(capacity),
            in_flight: HashMap::new(),
            in_flight_ttl,
            prune_at: MIN_PRUNE_AT,
            empty: LruCache::new(
                NonZeroUsize::new(DEFAULT_EMPTY_CAPACITY)
                    .unwrap_or(NonZeroUsize::MIN),
            ),
            empty_ttl: DEFAULT_EMPTY_TTL,
            empty_strikes: LruCache::new(
                NonZeroUsize::new(DEFAULT_EMPTY_CAPACITY)
                    .unwrap_or(NonZeroUsize::MIN),
            ),
        }
    }

    /// Overrides the size / TTL of the "empty" negative cache.
    pub fn with_empty_cache(
        mut self,
        capacity: usize,
        ttl: Duration,
    ) -> Self {
        let capacity =
            NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN);
        self.empty = LruCache::new(capacity);
        self.empty_strikes = LruCache::new(capacity);
        self.empty_ttl = ttl;
        self
    }

    /// Whether `claim` would hand this address out: it is neither known,
    /// nor being worked on, nor recently seen without code. Does not
    /// claim anything.
    pub fn needs_resolution(
        &mut self,
        address: &Address,
        now: Instant,
    ) -> bool {
        if self.known.get(address).is_some() {
            return false;
        }

        if self.empty.peek(address).is_some_and(|since| {
            now.saturating_duration_since(*since) < self.empty_ttl
        }) {
            return false;
        }

        !self.in_flight.get(address).is_some_and(|since| {
            now.saturating_duration_since(*since) < self.in_flight_ttl
        })
    }

    /// Forgets that tokens are stored (the database says they are not).
    pub fn forget<'a, I>(&mut self, addresses: I)
    where
        I: IntoIterator<Item = &'a Address>,
    {
        for address in addresses {
            self.known.pop(address);
            self.empty.pop(address);
        }
    }

    /// How many consecutive fetches found no code at `address`.
    pub fn empty_strikes(&self, address: &Address) -> u32 {
        self.empty_strikes.peek(address).copied().unwrap_or(0)
    }

    /// Atomically claims every address that is neither known nor already
    /// claimed by someone else, returning the claimed addresses.
    ///
    /// Claims expire after the configured TTL so that a writer that never
    /// confirms (e.g. a dropped batch) cannot block a token forever.
    pub fn claim<I>(&mut self, addresses: I, now: Instant) -> Vec<Address>
    where
        I: IntoIterator<Item = Address>,
    {
        self.prune(now);

        let mut claimed = Vec::new();

        for address in addresses {
            // `get` (not `contains`) so hot tokens stay most recently used.
            if self.known.get(&address).is_some() {
                continue;
            }

            // Recently seen without code: leave it alone until the TTL.
            if let Some(since) = self.empty.peek(&address).copied() {
                if now.saturating_duration_since(since) < self.empty_ttl {
                    continue;
                }
                self.empty.pop(&address);
            }

            let expired = match self.in_flight.get(&address) {
                Some(since) => {
                    now.saturating_duration_since(*since)
                        >= self.in_flight_ttl
                }
                None => true,
            };

            if expired {
                self.in_flight.insert(address, now);
                claimed.push(address);
            }
        }

        claimed
    }

    /// Gives claims back without marking the tokens as known.
    pub fn release<'a, I>(&mut self, addresses: I)
    where
        I: IntoIterator<Item = &'a Address>,
    {
        for address in addresses {
            self.in_flight.remove(address);
        }
    }

    /// Marks tokens as durably stored: removes the claim (if any) and
    /// inserts them in the LRU.
    pub fn confirm<'a, I>(&mut self, addresses: I)
    where
        I: IntoIterator<Item = &'a Address>,
    {
        for address in addresses {
            self.in_flight.remove(address);
            self.empty.pop(address);
            self.empty_strikes.pop(address);
            self.known.put(*address, ());
        }
    }

    /// Records addresses that had no code when fetched: the claim is
    /// released and they are skipped until the "empty" TTL elapses. Never
    /// marks them as known and never reaches the persistent cache.
    pub fn mark_empty<'a, I>(&mut self, addresses: I, now: Instant)
    where
        I: IntoIterator<Item = &'a Address>,
    {
        for address in addresses {
            self.in_flight.remove(address);
            self.empty.put(*address, now);
            let strikes = self.empty_strikes(address).saturating_add(1);
            self.empty_strikes.put(*address, strikes);
        }
    }

    pub fn empty_len(&self) -> usize {
        self.empty.len()
    }

    pub fn is_known(&self, address: &Address) -> bool {
        self.known.contains(address)
    }

    pub fn is_in_flight(&self, address: &Address) -> bool {
        self.in_flight.contains_key(address)
    }

    pub fn known_len(&self) -> usize {
        self.known.len()
    }

    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    /// Drops expired claims. Amortized O(1): only runs once the map has
    /// doubled since the last prune.
    fn prune(&mut self, now: Instant) {
        if self.in_flight.len() < self.prune_at {
            return;
        }

        let ttl = self.in_flight_ttl;
        self.in_flight.retain(|_, since| {
            now.saturating_duration_since(*since) < ttl
        });
        self.prune_at = (self.in_flight.len() * 2).max(MIN_PRUNE_AT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use std::sync::Arc;

    fn addr(n: u64) -> Address {
        Address::left_padding_from(&n.to_be_bytes())
    }

    const TTL: Duration = Duration::from_secs(60);

    #[test]
    fn redis_key_is_namespaced_and_lowercase() {
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        assert_eq!(
            redis_key(1, &weth),
            "evm-indexer:token:1:0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
        );
        assert_eq!(
            redis_key(143, &Address::ZERO),
            "evm-indexer:token:143:0x0000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn cached_token_json_round_trip() {
        let token = DatabaseToken {
            address: addr(1),
            name: "Wrapped \"Ether\" \u{1F980}".to_string(),
            symbol: "WETH".to_string(),
            decimals: 18,
            r#type: "ERC20".to_string(),
            chain: 1,
        };

        let json = CachedToken::from(&token).to_json().unwrap();
        // Compact and with the plain `type` key for API consumers.
        assert!(json.contains("\"type\":\"ERC20\""));
        assert!(json.contains("\"decimals\":18"));
        assert!(!json.contains(' ') || json.contains("Wrapped "));
        assert!(!json.contains("address"));

        let back = CachedToken::from_json(&json).unwrap();
        assert_eq!(back, CachedToken::from(&token));

        let row = back.into_database_token(token.address, token.chain);
        assert_eq!(row.address, token.address);
        assert_eq!(row.name, token.name);
        assert_eq!(row.symbol, token.symbol);
        assert_eq!(row.decimals, token.decimals);
        assert_eq!(row.r#type, token.r#type);
        assert_eq!(row.chain, token.chain);
    }

    #[test]
    fn cached_token_exact_json_shape() {
        let cached = CachedToken {
            name: "Maker".into(),
            symbol: "MKR".into(),
            decimals: 18,
            r#type: "ERC20".into(),
        };
        assert_eq!(
            cached.to_json().unwrap(),
            r#"{"name":"Maker","symbol":"MKR","decimals":18,"type":"ERC20"}"#
        );
    }

    #[test]
    fn lru_is_bounded_and_evicts_least_recently_used() {
        let mut tokens = KnownTokens::new(3, TTL);
        let now = Instant::now();

        tokens.confirm(&[addr(1), addr(2), addr(3)]);
        assert_eq!(tokens.known_len(), 3);

        // Touch 1 so that 2 becomes the eviction candidate.
        assert!(tokens.claim([addr(1)], now).is_empty());

        tokens.confirm(&[addr(4)]);
        assert_eq!(tokens.known_len(), 3);
        assert!(tokens.is_known(&addr(1)));
        assert!(!tokens.is_known(&addr(2)));
        assert!(tokens.is_known(&addr(3)));
        assert!(tokens.is_known(&addr(4)));

        for n in 10..10_000 {
            tokens.confirm(&[addr(n)]);
        }
        assert_eq!(tokens.known_len(), 3);
    }

    #[test]
    fn zero_capacity_does_not_panic() {
        let mut tokens = KnownTokens::new(0, TTL);
        tokens.confirm(&[addr(1), addr(2)]);
        assert_eq!(tokens.known_len(), 1);
    }

    #[test]
    fn claims_are_exclusive_until_released_or_confirmed() {
        let mut tokens = KnownTokens::new(16, TTL);
        let now = Instant::now();

        let first = tokens.claim([addr(1), addr(2)], now);
        assert_eq!(first, vec![addr(1), addr(2)]);

        // A concurrent batch only gets what nobody else is working on.
        let second = tokens.claim([addr(1), addr(2), addr(3)], now);
        assert_eq!(second, vec![addr(3)]);

        // Released (transport failure): can be claimed again.
        tokens.release(&[addr(1)]);
        assert!(!tokens.is_in_flight(&addr(1)));
        assert_eq!(tokens.claim([addr(1)], now), vec![addr(1)]);

        // Confirmed (stored): never claimed again.
        tokens.confirm(&[addr(2)]);
        assert!(!tokens.is_in_flight(&addr(2)));
        assert!(tokens.is_known(&addr(2)));
        assert!(tokens.claim([addr(2)], now).is_empty());
    }

    #[test]
    fn duplicate_addresses_in_one_claim_are_claimed_once() {
        let mut tokens = KnownTokens::new(16, TTL);
        let claimed =
            tokens.claim([addr(7), addr(7), addr(7)], Instant::now());
        assert_eq!(claimed, vec![addr(7)]);
    }

    #[test]
    fn stale_claims_expire() {
        let mut tokens = KnownTokens::new(16, TTL);
        let start = Instant::now();

        assert_eq!(tokens.claim([addr(1)], start), vec![addr(1)]);
        assert!(tokens.claim([addr(1)], start + TTL / 2).is_empty());
        assert_eq!(tokens.claim([addr(1)], start + TTL), vec![addr(1)]);
        // The re-claim restarted the clock.
        assert!(tokens.claim([addr(1)], start + TTL + TTL / 2).is_empty());
    }

    #[test]
    fn empty_addresses_are_skipped_until_their_ttl() {
        let empty_ttl = Duration::from_secs(10);
        let mut tokens =
            KnownTokens::new(16, TTL).with_empty_cache(2, empty_ttl);
        let start = Instant::now();

        assert_eq!(tokens.claim([addr(1)], start), vec![addr(1)]);
        tokens.mark_empty(&[addr(1)], start);

        // Claim released, not known, but not claimable for a while.
        assert!(!tokens.is_in_flight(&addr(1)));
        assert!(!tokens.is_known(&addr(1)));
        assert!(tokens.claim([addr(1)], start + empty_ttl / 2).is_empty());

        // After the TTL it is fetched again.
        assert_eq!(
            tokens.claim([addr(1)], start + empty_ttl),
            vec![addr(1)]
        );
        assert_eq!(tokens.empty_len(), 0);

        // Once it turns out to be a real token it is simply known.
        tokens.mark_empty(&[addr(1)], start);
        tokens.confirm(&[addr(1)]);
        assert_eq!(tokens.empty_len(), 0);
        assert!(tokens.is_known(&addr(1)));

        // Bounded.
        let many: Vec<_> = (100..200).map(addr).collect();
        tokens.mark_empty(&many, start);
        assert_eq!(tokens.empty_len(), 2);
    }

    #[test]
    fn expired_claims_are_pruned() {
        let mut tokens = KnownTokens::new(16, TTL);
        let start = Instant::now();

        let batch = (0..MIN_PRUNE_AT as u64).map(addr);
        assert_eq!(tokens.claim(batch, start).len(), MIN_PRUNE_AT);
        assert_eq!(tokens.in_flight_len(), MIN_PRUNE_AT);

        // Next claim after the TTL prunes all the stale entries.
        tokens.claim([addr(u64::MAX)], start + TTL * 2);
        assert_eq!(tokens.in_flight_len(), 1);
    }

    /// Minimal in-process RESP2 server (loopback only) so the real
    /// `redis` client code path (pipelines, reply parsing, reconnects) is
    /// exercised without an external Redis.
    mod resp_server {
        use std::{
            collections::HashMap,
            sync::{
                atomic::{AtomicUsize, Ordering},
                Arc, Mutex,
            },
        };
        use tokio::{
            io::{
                AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader,
            },
            net::{TcpListener, TcpStream},
            task::JoinHandle,
        };

        #[derive(Default)]
        pub struct State {
            pub entries: Mutex<HashMap<String, String>>,
            /// Number of socket reads that carried at least one command:
            /// a pipeline of N commands must count once, not N times.
            pub commands: AtomicUsize,
        }

        pub struct Server {
            pub url: String,
            pub state: Arc<State>,
            handle: JoinHandle<()>,
            connections: Arc<Mutex<Vec<JoinHandle<()>>>>,
        }

        impl Server {
            pub async fn start(state: Arc<State>, port: u16) -> Self {
                let listener =
                    TcpListener::bind(("127.0.0.1", port)).await.unwrap();
                let port = listener.local_addr().unwrap().port();
                let shared = state.clone();
                let connections: Arc<Mutex<Vec<JoinHandle<()>>>> =
                    Arc::default();
                let accepted = connections.clone();

                let handle = tokio::spawn(async move {
                    loop {
                        let Ok((socket, _)) = listener.accept().await
                        else {
                            return;
                        };
                        let task =
                            tokio::spawn(serve(socket, shared.clone()));
                        accepted.lock().unwrap().push(task);
                    }
                });

                Self {
                    url: format!("redis://127.0.0.1:{port}/"),
                    state,
                    handle,
                    connections,
                }
            }

            pub fn port(&self) -> u16 {
                self.url
                    .trim_end_matches('/')
                    .rsplit(':')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap()
            }

            /// Stops accepting connections and closes the existing ones.
            pub fn stop(self) {
                self.handle.abort();
                for connection in
                    self.connections.lock().unwrap().drain(..)
                {
                    connection.abort();
                }
            }
        }

        async fn read_command(
            reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
        ) -> Option<Vec<String>> {
            let mut line = String::new();
            if reader.read_line(&mut line).await.ok()? == 0 {
                return None;
            }
            let count: usize =
                line.trim().strip_prefix('*')?.parse().ok()?;

            let mut parts = Vec::with_capacity(count);
            for _ in 0..count {
                line.clear();
                reader.read_line(&mut line).await.ok()?;
                let len: usize =
                    line.trim().strip_prefix('$')?.parse().ok()?;
                let mut bulk = vec![0u8; len + 2];
                reader.read_exact(&mut bulk).await.ok()?;
                bulk.truncate(len);
                parts.push(String::from_utf8(bulk).ok()?);
            }
            Some(parts)
        }

        async fn serve(socket: TcpStream, state: Arc<State>) {
            let (read, mut write) = socket.into_split();
            let mut reader = BufReader::new(read);

            while let Some(command) = read_command(&mut reader).await {
                let name = command[0].to_ascii_uppercase();
                let reply = match name.as_str() {
                    "PING" => "+PONG\r\n".to_string(),
                    "EXISTS" => {
                        state.commands.fetch_add(1, Ordering::SeqCst);
                        let entries = state.entries.lock().unwrap();
                        format!(
                            ":{}\r\n",
                            entries.contains_key(&command[1]) as u8
                        )
                    }
                    "SET" => {
                        state.commands.fetch_add(1, Ordering::SeqCst);
                        state.entries.lock().unwrap().insert(
                            command[1].clone(),
                            command[2].clone(),
                        );
                        "+OK\r\n".to_string()
                    }
                    "CLIENT" | "SELECT" => "+OK\r\n".to_string(),
                    _ => "-ERR unknown command\r\n".to_string(),
                };
                if write.write_all(reply.as_bytes()).await.is_err() {
                    return;
                }
            }
        }
    }

    fn token(n: u64, chain: u64) -> DatabaseToken {
        DatabaseToken {
            address: addr(n),
            name: format!("Token {n}"),
            symbol: "TKN".to_string(),
            decimals: 18,
            r#type: "ERC20".to_string(),
            chain,
        }
    }

    #[tokio::test]
    async fn redis_cache_speaks_resp_with_pipelines() {
        let server =
            resp_server::Server::start(Default::default(), 0).await;
        let cache =
            RedisTokenCache::connect(5, &server.url).await.unwrap();

        // More than one pipeline worth of tokens.
        let tokens: Vec<_> = (0..2_500).map(|n| token(n, 5)).collect();
        let addresses: Vec<_> = tokens.iter().map(|t| t.address).collect();

        let found = cache.contains_many(&addresses).await.unwrap();
        assert_eq!(found.len(), 2_500);
        assert!(found.iter().all(|found| !found));

        cache.store_many(&tokens[..2_000]).await.unwrap();

        let found = cache.contains_many(&addresses).await.unwrap();
        assert!(found[..2_000].iter().all(|found| *found));
        assert!(found[2_000..].iter().all(|found| !found));

        let entries = server.state.entries.lock().unwrap();
        assert_eq!(entries.len(), 2_000);
        assert_eq!(
            entries[&redis_key(5, &addr(7))],
            r#"{"name":"Token 7","symbol":"TKN","decimals":18,"type":"ERC20"}"#
        );
    }

    #[tokio::test]
    async fn redis_cache_survives_outage_and_reconnects() {
        let state: Arc<resp_server::State> = Default::default();
        let server = resp_server::Server::start(state.clone(), 0).await;
        let port = server.port();
        let cache =
            RedisTokenCache::connect(5, &server.url).await.unwrap();
        cache.store_many(&[token(1, 5)]).await.unwrap();

        // Kill the server: operations fail fast, nothing panics.
        server.stop();
        // Let the abort land and the listener socket close.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let started = Instant::now();
        let mut failures = 0;
        for _ in 0..20 {
            if cache.contains_many(&[addr(1)]).await.is_err() {
                failures += 1;
            }
        }
        // A dead server never stalls the caller: the first failure opens
        // the circuit breaker and the rest fail without any network I/O.
        assert_eq!(failures, 20);
        assert!(cache.in_cooldown());
        assert!(started.elapsed() < Duration::from_secs(8));

        // Bring a server back on the same port and skip the cool-down.
        let server = resp_server::Server::start(state, port).await;
        let mut recovered = false;
        for _ in 0..50 {
            *cache.down_until.lock().unwrap() = None;
            if let Ok(found) = cache.contains_many(&[addr(1)]).await {
                assert_eq!(found, vec![true]);
                recovered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(recovered, "cache never reconnected");
        server.stop();
    }

    #[tokio::test]
    async fn redis_cache_unreachable_at_startup_is_not_an_error() {
        // Grab a free port and close it again so nothing listens there.
        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let started = Instant::now();
        let cache = RedisTokenCache::connect(
            1,
            &format!("redis://127.0.0.1:{port}/"),
        )
        .await
        .unwrap();

        assert!(cache.contains_many(&[addr(1)]).await.is_err());
        assert!(cache.store_many(&[token(1, 1)]).await.is_err());
        assert!(cache.in_cooldown());
        assert!(started.elapsed() < Duration::from_secs(8));

        assert!(RedisTokenCache::connect(1, "not a url").await.is_err());
    }

    #[tokio::test]
    async fn redis_errors_do_not_leak_the_password() {
        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let cache = RedisTokenCache::connect(
            1,
            &format!("redis://user:hunter2secret@127.0.0.1:{port}/0"),
        )
        .await
        .unwrap();

        *cache.down_until.lock().unwrap() = None;
        let error = cache.contains_many(&[addr(1)]).await.unwrap_err();
        assert!(!format!("{error:#}").contains("hunter2secret"));

        let error = RedisTokenCache::connect(
            1,
            "redis://user:hunter2secret@:bad-port/0",
        )
        .await
        .err()
        .map(|error| format!("{error:#}"))
        .unwrap_or_default();
        assert!(!error.contains("hunter2secret"), "{error}");
    }
}
