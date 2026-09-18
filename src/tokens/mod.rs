//! Token metadata resolution with a layered cache.
//!
//! Lookup order for every token seen by the indexer:
//!
//! 1. bounded in-process LRU of tokens known to be stored,
//! 2. Redis / Dragonfly (one pipelined round trip per batch),
//! 3. Multicall3 `aggregate3` over JSON-RPC (individual `eth_call`s when the
//!    chain has no Multicall3).
//!
//! Only definitive answers are ever cached: a token whose calls revert or
//! return garbage is stored (blank) so it is never fetched again, but an
//! address without code (the node may lag behind the indexed head) or an
//! unreachable RPC produce no row at all and are retried later. A dead or
//! hanging RPC trips a circuit breaker instead of stalling the indexer, and
//! the RPC must report the indexed chain id.
//!
//! Writes are two-phase so the cache can never claim a token that is not in
//! ClickHouse: [`TokenResolver::resolve_new`] only *claims* the tokens it
//! returns (in memory), and [`TokenResolver::mark_stored`] persists them to
//! Redis once the writer has durably inserted the rows.
//!
//! Redis keys are `evm-indexer:token:{chain_id}:{0xaddress-lowercase}` and
//! hold a compact JSON `{"name","symbol","decimals","type"}` without TTL.
//! When the ClickHouse `tokens` table is reset, those keys must be flushed
//! too.

pub mod cache;
pub mod decode;
pub mod multicall;
pub mod redact;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use alloy::primitives::Address;
use anyhow::bail;
use log::{debug, info, warn};

use crate::db::models::token::DatabaseToken;

use self::{
    cache::{KnownTokens, RedisTokenCache, TokenCache},
    multicall::{AlloyCaller, ChainCheck, FetchOptions, MetadataFetcher},
};

/// Token standard hint, derived by the caller from the transfer event that
/// revealed the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TokenStandard {
    Erc20,
    Erc721,
    Erc1155,
}

impl TokenStandard {
    /// Value of the ClickHouse `tokens.type` column.
    pub const fn as_str(&self) -> &'static str {
        match self {
            TokenStandard::Erc20 => "ERC20",
            TokenStandard::Erc721 => "ERC721",
            TokenStandard::Erc1155 => "ERC1155",
        }
    }
}

/// Tunables of the [`TokenResolver`]. `Default` is what
/// [`TokenResolver::new`] uses.
#[derive(Debug, Clone)]
pub struct TokenResolverOptions {
    /// Maximum number of addresses kept in the in-process LRU
    /// (~100 bytes each).
    pub memory_capacity: usize,
    /// How long a token returned by `resolve_new` stays claimed while
    /// waiting for `mark_stored`, before it may be fetched again.
    pub in_flight_ttl: Duration,
    /// Maximum number of rows kept in memory waiting for Redis to come back
    /// so they can be persisted.
    pub max_pending_writes: usize,
    /// Addresses without code are not refetched for this long (memory
    /// only, never persisted).
    pub empty_ttl: Duration,
    /// Maximum number of code-less addresses remembered.
    pub empty_capacity: usize,
    pub fetch: FetchOptions,
}

impl Default for TokenResolverOptions {
    fn default() -> Self {
        Self {
            memory_capacity: 500_000,
            in_flight_ttl: Duration::from_secs(600),
            max_pending_writes: 100_000,
            empty_ttl: cache::DEFAULT_EMPTY_TTL,
            empty_capacity: cache::DEFAULT_EMPTY_CAPACITY,
            fetch: FetchOptions::default(),
        }
    }
}

struct Inner {
    chain_id: u64,
    fetcher: Option<MetadataFetcher>,
    cache: Option<Arc<dyn TokenCache>>,
    known: Mutex<KnownTokens>,
    /// Rows stored in ClickHouse whose Redis write failed.
    pending_writes: Mutex<VecDeque<DatabaseToken>>,
    max_pending_writes: usize,
}

/// Resolves metadata of newly seen tokens. Cheap to clone and safe to use
/// concurrently from several tasks.
#[derive(Clone)]
pub struct TokenResolver {
    inner: Arc<Inner>,
}

/// Releases the claims of a `resolve_new` call that did not end up
/// producing a row (transport failure, cancelled future, ...).
struct ClaimGuard<'a> {
    inner: &'a Inner,
    unresolved: HashSet<Address>,
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        if !self.unresolved.is_empty() {
            self.inner.known().release(&self.unresolved);
        }
    }
}

impl Inner {
    fn known(&self) -> MutexGuard<'_, KnownTokens> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pending_writes(&self) -> MutexGuard<'_, VecDeque<DatabaseToken>> {
        self.pending_writes.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl TokenResolver {
    /// Creates a resolver.
    ///
    /// * `rpc_url: None` disables token metadata (`resolve_new` returns
    ///   nothing).
    ///   When set, the node must serve `chain_id` (`eth_chainId`): a
    ///   mismatch is an error. If the node is unreachable at startup this
    ///   is only a warning and the check is repeated before the first fetch.
    /// * `redis_url: None` keeps the cache in memory only. An unreachable
    ///   Redis is not an error (it is connected lazily), an invalid URL is.
    pub async fn new(
        chain_id: u64,
        rpc_url: Option<&str>,
        redis_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        Self::with_options(
            chain_id,
            rpc_url,
            redis_url,
            TokenResolverOptions::default(),
        )
        .await
    }

    pub async fn with_options(
        chain_id: u64,
        rpc_url: Option<&str>,
        redis_url: Option<&str>,
        options: TokenResolverOptions,
    ) -> anyhow::Result<Self> {
        let fetcher = match rpc_url {
            Some(url) => {
                let caller =
                    AlloyCaller::new(url, options.fetch.call_timeout)?;
                let fetcher = MetadataFetcher::new(
                    Arc::new(caller),
                    options.fetch.clone(),
                )
                .expect_chain_id(chain_id);

                match fetcher.check_chain_id().await {
                    ChainCheck::Verified => {}
                    ChainCheck::Mismatch(actual) => bail!(
                        "the token metadata RPC serves chain {actual} but                          chain {chain_id} is being indexed"
                    ),
                    ChainCheck::Unavailable(error) => warn!(
                        "Unable to verify the chain id of the token                          metadata RPC, it will be checked again before                          the first fetch: {error}"
                    ),
                }

                Some(fetcher)
            }
            None => {
                info!(
                    "No RPC url configured: token metadata resolution is \
                     disabled"
                );
                None
            }
        };

        let cache: Option<Arc<dyn TokenCache>> = match redis_url {
            // Without a fetcher nothing is ever looked up or stored.
            Some(url) if fetcher.is_some() => Some(Arc::new(
                RedisTokenCache::connect(chain_id, url).await?,
            )),
            Some(_) => None,
            None => {
                if fetcher.is_some() {
                    info!(
                        "No redis url configured: token cache is \
                         memory-only"
                    );
                }
                None
            }
        };

        Ok(Self::from_parts(chain_id, fetcher, cache, &options))
    }

    /// Assembles a resolver from explicit backends (tests, custom caches).
    pub fn from_parts(
        chain_id: u64,
        fetcher: Option<MetadataFetcher>,
        cache: Option<Arc<dyn TokenCache>>,
        options: &TokenResolverOptions,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                chain_id,
                fetcher,
                cache,
                known: Mutex::new(
                    KnownTokens::new(
                        options.memory_capacity,
                        options.in_flight_ttl,
                    )
                    .with_empty_cache(
                        options.empty_capacity,
                        options.empty_ttl,
                    ),
                ),
                pending_writes: Mutex::new(VecDeque::new()),
                max_pending_writes: options.max_pending_writes,
            }),
        }
    }

    /// Returns metadata rows for tokens not seen before (rows to insert
    /// into ClickHouse `tokens`).
    ///
    /// Does NOT mark them as known in Redis: call
    /// [`mark_stored`](Self::mark_stored) once they are durably inserted.
    /// Until then the returned tokens are tracked as in flight so that
    /// concurrent / subsequent calls do not fetch them again.
    ///
    /// Tokens whose calls revert or return garbage still produce a row
    /// (empty strings, hinted type) so they are never fetched again. Tokens
    /// that could not be fetched because the RPC is unreachable (or its
    /// circuit breaker is open) and addresses that have no code on the node
    /// produce no row and are retried when they are seen again.
    ///
    /// With no `rpc_url` configured returns an empty `Vec`.
    pub async fn resolve_new(
        &self,
        tokens: &HashMap<Address, TokenStandard>,
    ) -> Vec<DatabaseToken> {
        let inner = &*self.inner;

        let Some(fetcher) = &inner.fetcher else {
            return Vec::new();
        };

        if tokens.is_empty() {
            return Vec::new();
        }

        // 1. Memory: atomically claim what nobody knows / works on.
        let claimed =
            inner.known().claim(tokens.keys().copied(), Instant::now());

        if claimed.is_empty() {
            return Vec::new();
        }

        let mut guard = ClaimGuard {
            inner,
            unresolved: claimed.iter().copied().collect(),
        };

        // 2. Redis: one pipelined lookup for the whole batch.
        let mut missing = claimed;

        if let Some(cache) = &inner.cache {
            match cache.contains_many(&missing).await {
                Ok(found) if found.len() == missing.len() => {
                    let (hits, misses): (Vec<_>, Vec<_>) = missing
                        .iter()
                        .copied()
                        .zip(found)
                        .partition(|(_, found)| *found);

                    let hits: Vec<Address> =
                        hits.into_iter().map(|(a, _)| a).collect();
                    missing = misses.into_iter().map(|(a, _)| a).collect();

                    inner.known().confirm(&hits);
                    for address in &hits {
                        guard.unresolved.remove(address);
                    }
                }
                Ok(found) => debug!(
                    "Token cache returned {} answers for {} tokens, \
                     ignoring it",
                    found.len(),
                    missing.len()
                ),
                // Already logged (rate limited) by the cache itself.
                Err(error) => {
                    debug!("Token cache lookup failed: {error}")
                }
            }
        }

        // Dead RPC / wrong chain: give the claims back right away.
        if missing.is_empty() || !fetcher.is_available() {
            return Vec::new();
        }

        // 3. RPC.
        let mut to_fetch: Vec<(Address, TokenStandard)> = missing
            .iter()
            .filter_map(|address| {
                tokens.get(address).map(|standard| (*address, *standard))
            })
            .collect();
        // Deterministic chunks (HashMap order is random).
        to_fetch.sort_unstable();

        let outcome = fetcher.fetch(&to_fetch).await;
        let mut fetched = outcome.resolved;

        // No code at the address as far as the node knows: not definitive
        // (lagging node), so no row and nothing persisted. Skipped for a
        // while in memory, then fetched again when seen.
        if !outcome.empty.is_empty() {
            debug!(
                "{} token addresses have no code on the RPC node yet, \
                 they will be retried later",
                outcome.empty.len()
            );
            inner.known().mark_empty(&outcome.empty, Instant::now());
            for address in &outcome.empty {
                guard.unresolved.remove(address);
            }
        }

        let mut rows = Vec::with_capacity(fetched.len());
        for (address, standard) in to_fetch {
            let Some(metadata) = fetched.remove(&address) else {
                // Transport failure: claim is released by the guard.
                continue;
            };

            // The row now owns the claim until `mark_stored`.
            guard.unresolved.remove(&address);

            rows.push(DatabaseToken {
                address,
                name: metadata.name,
                symbol: metadata.symbol,
                decimals: match standard {
                    TokenStandard::Erc20 => metadata.decimals,
                    _ => 0,
                },
                r#type: standard.as_str().to_string(),
                chain: inner.chain_id,
            });
        }

        // The outage itself is reported (once) by the fetcher's breaker.
        if !guard.unresolved.is_empty() {
            debug!(
                "Unable to fetch metadata for {} tokens, they will be \
                 retried when seen again",
                guard.unresolved.len()
            );
        }

        rows
    }

    /// Called by the writer after the rows were durably inserted into
    /// ClickHouse; persists them to Redis and marks them as known.
    ///
    /// Never fails: if Redis is unavailable the rows are kept (bounded) and
    /// written on a later call, and the tokens are known in memory anyway.
    pub async fn mark_stored(&self, tokens: &[DatabaseToken]) {
        let inner = &*self.inner;

        if tokens.is_empty() {
            return;
        }

        inner.known().confirm(tokens.iter().map(|token| &token.address));

        let Some(cache) = &inner.cache else {
            return;
        };

        // Rows left over from a previous Redis outage go first.
        let mut batch: Vec<DatabaseToken> =
            inner.pending_writes().drain(..).collect();
        batch.extend_from_slice(tokens);

        if let Err(error) = cache.store_many(&batch).await {
            debug!("Token cache write failed: {error}");

            let mut pending = inner.pending_writes();
            pending.extend(batch);

            let overflow =
                pending.len().saturating_sub(inner.max_pending_writes);
            if overflow > 0 {
                pending.drain(..overflow);
                warn!(
                    "Token cache is unavailable, dropped {overflow} \
                     pending token cache writes (they will be fetched \
                     again after a restart)"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        multicall::testing::{fast_options, FakeChain, FakeToken},
        *,
    };
    use futures::future::BoxFuture;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn addr(n: u64) -> Address {
        Address::left_padding_from(&n.to_be_bytes())
    }

    /// In-memory stand-in for Redis.
    #[derive(Default)]
    struct FakeCache {
        chain_id: u64,
        entries: Mutex<HashMap<String, String>>,
        down: AtomicBool,
        lookups: AtomicUsize,
        writes: AtomicUsize,
    }

    impl FakeCache {
        fn new(chain_id: u64) -> Arc<Self> {
            Arc::new(Self { chain_id, ..Self::default() })
        }

        fn get(&self, address: &Address) -> Option<cache::CachedToken> {
            let entries = self.entries.lock().unwrap();
            let json =
                entries.get(&cache::redis_key(self.chain_id, address))?;
            Some(cache::CachedToken::from_json(json).unwrap())
        }

        fn len(&self) -> usize {
            self.entries.lock().unwrap().len()
        }
    }

    impl TokenCache for FakeCache {
        fn contains_many<'a>(
            &'a self,
            addresses: &'a [Address],
        ) -> BoxFuture<'a, anyhow::Result<Vec<bool>>> {
            Box::pin(async move {
                tokio::task::yield_now().await;
                self.lookups.fetch_add(1, Ordering::SeqCst);
                if self.down.load(Ordering::SeqCst) {
                    anyhow::bail!("connection refused");
                }
                let entries = self.entries.lock().unwrap();
                Ok(addresses
                    .iter()
                    .map(|a| {
                        entries.contains_key(&cache::redis_key(
                            self.chain_id,
                            a,
                        ))
                    })
                    .collect())
            })
        }

        fn store_many<'a>(
            &'a self,
            tokens: &'a [DatabaseToken],
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                tokio::task::yield_now().await;
                self.writes.fetch_add(1, Ordering::SeqCst);
                if self.down.load(Ordering::SeqCst) {
                    anyhow::bail!("connection refused");
                }
                let mut entries = self.entries.lock().unwrap();
                for token in tokens {
                    entries.insert(
                        cache::redis_key(token.chain, &token.address),
                        cache::CachedToken::from(token).to_json()?,
                    );
                }
                Ok(())
            })
        }
    }

    fn options() -> TokenResolverOptions {
        TokenResolverOptions {
            fetch: fast_options(),
            ..TokenResolverOptions::default()
        }
    }

    fn resolver(
        chain: &Arc<FakeChain>,
        cache: Option<&Arc<FakeCache>>,
    ) -> TokenResolver {
        TokenResolver::from_parts(
            1,
            Some(MetadataFetcher::new(chain.clone(), fast_options())),
            cache.map(|c| c.clone() as Arc<dyn TokenCache>),
            &options(),
        )
    }

    fn batch(
        tokens: &[(u64, TokenStandard)],
    ) -> HashMap<Address, TokenStandard> {
        tokens.iter().map(|(n, s)| (addr(*n), *s)).collect()
    }

    fn rpc_calls(chain: &FakeChain) -> usize {
        chain.multicall_calls.load(Ordering::SeqCst)
            + chain.direct_calls.load(Ordering::SeqCst)
    }

    #[test]
    fn type_hint_maps_to_the_type_column() {
        assert_eq!(TokenStandard::Erc20.as_str(), "ERC20");
        assert_eq!(TokenStandard::Erc721.as_str(), "ERC721");
        assert_eq!(TokenStandard::Erc1155.as_str(), "ERC1155");
    }

    #[tokio::test]
    async fn disabled_without_rpc_url() {
        let resolver = TokenResolver::new(1, None, None).await.unwrap();
        let rows = resolver
            .resolve_new(&batch(&[(1, TokenStandard::Erc20)]))
            .await;
        assert!(rows.is_empty());
        resolver.mark_stored(&[]).await;

        // Redis is not even contacted when metadata is disabled.
        let resolver =
            TokenResolver::new(1, None, Some("redis://127.0.0.1:1/"))
                .await
                .unwrap();
        assert!(resolver.inner.cache.is_none());
    }

    #[tokio::test]
    async fn invalid_urls_are_errors() {
        assert!(TokenResolver::new(1, Some("not a url"), None)
            .await
            .is_err());
        assert!(TokenResolver::new(
            1,
            Some("http://127.0.0.1:1"),
            Some("not a url")
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn resolves_rows_with_the_hinted_type() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("USD Coin", "USDC", 6));
        chain.add(addr(2), FakeToken::nft("Bored Apes", "BAYC"));
        chain.add(addr(3), FakeToken::reverting());
        // An NFT answering decimals() must still get 0.
        chain.add(addr(4), FakeToken::erc20("Weird NFT", "WNFT", 18));

        let resolver = resolver(&chain, None);
        let mut rows = resolver
            .resolve_new(&batch(&[
                (1, TokenStandard::Erc20),
                (2, TokenStandard::Erc721),
                (3, TokenStandard::Erc1155),
                (4, TokenStandard::Erc721),
            ]))
            .await;
        rows.sort_by_key(|row| row.address);

        assert_eq!(rows.len(), 4);
        assert_eq!(
            (rows[0].name.as_str(), rows[0].symbol.as_str()),
            ("USD Coin", "USDC")
        );
        assert_eq!(
            (rows[0].decimals, rows[0].r#type.as_str()),
            (6, "ERC20")
        );
        assert_eq!(rows[0].chain, 1);
        assert_eq!(
            (rows[1].name.as_str(), rows[1].r#type.as_str()),
            ("Bored Apes", "ERC721")
        );
        assert_eq!(rows[1].decimals, 0);
        assert_eq!(
            (rows[2].name.as_str(), rows[2].symbol.as_str()),
            ("", "")
        );
        assert_eq!(rows[2].r#type, "ERC1155");
        assert_eq!(
            (rows[3].decimals, rows[3].r#type.as_str()),
            (0, "ERC721")
        );
    }

    #[tokio::test]
    async fn two_phase_write_only_touches_the_cache_in_mark_stored() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("USD Coin", "USDC", 6));
        let cache = FakeCache::new(1);
        let resolver = resolver(&chain, Some(&cache));
        let tokens = batch(&[(1, TokenStandard::Erc20)]);

        let rows = resolver.resolve_new(&tokens).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(cache.len(), 0, "resolve_new must not write the cache");
        assert_eq!(cache.writes.load(Ordering::SeqCst), 0);

        // In flight: not fetched (nor returned) again.
        let calls = rpc_calls(&chain);
        assert!(resolver.resolve_new(&tokens).await.is_empty());
        assert_eq!(rpc_calls(&chain), calls);

        resolver.mark_stored(&rows).await;
        assert_eq!(
            cache.get(&addr(1)),
            Some(cache::CachedToken {
                name: "USD Coin".into(),
                symbol: "USDC".into(),
                decimals: 6,
                r#type: "ERC20".into(),
            })
        );

        // Known in memory: neither Redis nor the RPC are asked again.
        let lookups = cache.lookups.load(Ordering::SeqCst);
        assert!(resolver.resolve_new(&tokens).await.is_empty());
        assert_eq!(cache.lookups.load(Ordering::SeqCst), lookups);
        assert_eq!(rpc_calls(&chain), calls);
    }

    #[tokio::test]
    async fn redis_hits_survive_a_restart_without_rpc_calls() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("USD Coin", "USDC", 6));
        chain.add(addr(2), FakeToken::erc20("Tether", "USDT", 6));
        let cache = FakeCache::new(1);
        let tokens =
            batch(&[(1, TokenStandard::Erc20), (2, TokenStandard::Erc20)]);

        let first = resolver(&chain, Some(&cache));
        let rows = first.resolve_new(&tokens).await;
        first.mark_stored(&rows[..1]).await;
        let stored = rows[0].address;
        let calls = rpc_calls(&chain);

        // "Restart": fresh memory, same Redis. Only the token that was
        // never confirmed is fetched again.
        let second = resolver(&chain, Some(&cache));
        let rows = second.resolve_new(&tokens).await;
        assert_eq!(rows.len(), 1);
        assert_ne!(rows[0].address, stored);
        assert_eq!(rpc_calls(&chain), calls + 1);
        assert_eq!(cache.lookups.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn crash_before_mark_stored_leaves_no_trace_in_the_cache() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("USD Coin", "USDC", 6));
        let cache = FakeCache::new(1);
        let tokens = batch(&[(1, TokenStandard::Erc20)]);

        let crashed = resolver(&chain, Some(&cache));
        assert_eq!(crashed.resolve_new(&tokens).await.len(), 1);
        drop(crashed);

        let restarted = resolver(&chain, Some(&cache));
        assert_eq!(restarted.resolve_new(&tokens).await.len(), 1);
    }

    #[tokio::test]
    async fn negative_results_are_cached_and_never_refetched() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::reverting());
        let cache = FakeCache::new(1);
        let tokens = batch(&[(1, TokenStandard::Erc20)]);

        let first = resolver(&chain, Some(&cache));
        let rows = first.resolve_new(&tokens).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "");
        assert_eq!(rows[0].r#type, "ERC20");
        first.mark_stored(&rows).await;

        let calls = rpc_calls(&chain);
        assert!(first.resolve_new(&tokens).await.is_empty());
        let restarted = resolver(&chain, Some(&cache));
        assert!(restarted.resolve_new(&tokens).await.is_empty());
        assert_eq!(rpc_calls(&chain), calls);
    }

    #[tokio::test]
    async fn transport_failures_are_not_negatively_cached() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("USD Coin", "USDC", 6));
        chain.offline.store(true, Ordering::SeqCst);
        let cache = FakeCache::new(1);
        let resolver = resolver(&chain, Some(&cache));
        let tokens = batch(&[(1, TokenStandard::Erc20)]);

        assert!(resolver.resolve_new(&tokens).await.is_empty());
        assert_eq!(cache.len(), 0);
        {
            let known = resolver.inner.known();
            assert!(!known.is_known(&addr(1)));
            assert!(!known.is_in_flight(&addr(1)));
        }

        chain.offline.store(false, Ordering::SeqCst);
        let rows = resolver.resolve_new(&tokens).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].symbol, "USDC");
    }

    #[tokio::test]
    async fn codeless_addresses_produce_no_row_and_are_never_persisted() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("USD Coin", "USDC", 6));
        // addr(2) is deployed in a block the node has not seen yet.
        let cache = FakeCache::new(1);
        let resolver = TokenResolver::from_parts(
            1,
            Some(MetadataFetcher::new(chain.clone(), fast_options())),
            Some(cache.clone() as Arc<dyn TokenCache>),
            &TokenResolverOptions {
                empty_ttl: Duration::from_millis(30),
                ..options()
            },
        );
        let tokens = batch(&[
            (1, TokenStandard::Erc20),
            (2, TokenStandard::Erc20),
            (3, TokenStandard::Erc1155),
        ]);

        let rows = resolver.resolve_new(&tokens).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].address, addr(1));
        resolver.mark_stored(&rows).await;
        assert_eq!(cache.len(), 1);
        {
            let known = resolver.inner.known();
            assert!(!known.is_known(&addr(2)));
            assert!(!known.is_in_flight(&addr(2)));
            assert_eq!(known.empty_len(), 2);
        }

        // Within the TTL the dead addresses cost no RPC call...
        let calls = rpc_calls(&chain);
        assert!(resolver.resolve_new(&tokens).await.is_empty());
        assert_eq!(rpc_calls(&chain), calls);

        // ...afterwards the node caught up and the token is resolved.
        chain.add(addr(2), FakeToken::erc20("Tether", "USDT", 6));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let rows = resolver.resolve_new(&tokens).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].symbol, "USDT");
        resolver.mark_stored(&rows).await;
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&addr(3)).is_none());
    }

    #[tokio::test]
    async fn open_rpc_breaker_releases_claims_immediately() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("USD Coin", "USDC", 6));
        chain.offline.store(true, Ordering::SeqCst);
        let fetch = FetchOptions {
            breaker_cooldown: Duration::from_millis(60),
            breaker_max_cooldown: Duration::from_millis(60),
            ..fast_options()
        };
        let resolver = TokenResolver::from_parts(
            1,
            Some(MetadataFetcher::new(chain.clone(), fetch)),
            None,
            &options(),
        );
        let tokens = batch(&[(1, TokenStandard::Erc20)]);

        assert!(resolver.resolve_new(&tokens).await.is_empty());
        let attempts = chain.attempts.load(Ordering::SeqCst);

        // Breaker open: no RPC, no lingering claim, even if it is back.
        chain.offline.store(false, Ordering::SeqCst);
        for _ in 0..5 {
            assert!(resolver.resolve_new(&tokens).await.is_empty());
            assert!(!resolver.inner.known().is_in_flight(&addr(1)));
        }
        assert_eq!(chain.attempts.load(Ordering::SeqCst), attempts);

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(resolver.resolve_new(&tokens).await.len(), 1);
    }

    #[tokio::test]
    async fn wrong_chain_rpc_is_rejected_lazily_too() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("USD Coin", "USDC", 6));
        let resolver = TokenResolver::from_parts(
            56,
            Some(
                MetadataFetcher::new(chain.clone(), fast_options())
                    .expect_chain_id(56),
            ),
            None,
            &options(),
        );

        let rows = resolver
            .resolve_new(&batch(&[(1, TokenStandard::Erc20)]))
            .await;
        assert!(rows.is_empty());
        assert_eq!(chain.attempts.load(Ordering::SeqCst), 0);
        assert!(!resolver.inner.known().is_in_flight(&addr(1)));
    }

    #[tokio::test]
    async fn concurrent_batches_fetch_every_token_once() {
        let chain = FakeChain::new();
        let mut all = Vec::new();
        for n in 0..200u64 {
            chain.add(addr(n), FakeToken::erc20("Token", "TKN", 18));
            all.push((n, TokenStandard::Erc20));
        }
        let cache = FakeCache::new(1);
        let resolver = resolver(&chain, Some(&cache));

        // 8 tasks with heavily overlapping batches.
        let mut tasks = Vec::new();
        for task in 0..8usize {
            let resolver = resolver.clone();
            let tokens = batch(&all[task * 10..task * 10 + 130]);
            tasks.push(tokio::spawn(async move {
                let rows = resolver.resolve_new(&tokens).await;
                resolver.mark_stored(&rows).await;
                rows
            }));
        }

        let mut seen = HashSet::new();
        for task in tasks {
            for row in task.await.unwrap() {
                assert!(seen.insert(row.address), "duplicate row");
            }
        }

        assert_eq!(seen.len(), 200);
        assert_eq!(cache.len(), 200);
        for n in 0..200u64 {
            assert_eq!(chain.token_calls(&addr(n)), 3, "token {n}");
        }
        assert_eq!(resolver.inner.known().in_flight_len(), 0);
    }

    #[tokio::test]
    async fn cancelled_resolve_releases_its_claims() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("USD Coin", "USDC", 6));
        let resolver = resolver(&chain, None);
        let tokens = batch(&[(1, TokenStandard::Erc20)]);

        {
            let future = resolver.resolve_new(&tokens);
            tokio::pin!(future);
            // Poll once: the claim is taken, the fetch is pending.
            assert!(futures::poll!(future.as_mut()).is_pending());
            assert!(resolver.inner.known().is_in_flight(&addr(1)));
        }

        assert!(!resolver.inner.known().is_in_flight(&addr(1)));
        assert_eq!(resolver.resolve_new(&tokens).await.len(), 1);
    }

    #[tokio::test]
    async fn stale_claims_expire_when_the_writer_never_confirms() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("USD Coin", "USDC", 6));
        let resolver = TokenResolver::from_parts(
            1,
            Some(MetadataFetcher::new(chain.clone(), fast_options())),
            None,
            &TokenResolverOptions {
                in_flight_ttl: Duration::from_millis(20),
                ..options()
            },
        );
        let tokens = batch(&[(1, TokenStandard::Erc20)]);

        assert_eq!(resolver.resolve_new(&tokens).await.len(), 1);
        assert!(resolver.resolve_new(&tokens).await.is_empty());
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(resolver.resolve_new(&tokens).await.len(), 1);
    }

    #[tokio::test]
    async fn redis_outage_degrades_to_memory_and_catches_up() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("USD Coin", "USDC", 6));
        chain.add(addr(2), FakeToken::erc20("Tether", "USDT", 6));
        let cache = FakeCache::new(1);
        cache.down.store(true, Ordering::SeqCst);
        let resolver = resolver(&chain, Some(&cache));

        // Lookup fails -> everything is a miss, indexing continues.
        let first = batch(&[(1, TokenStandard::Erc20)]);
        let rows = resolver.resolve_new(&first).await;
        assert_eq!(rows.len(), 1);
        resolver.mark_stored(&rows).await;
        assert_eq!(cache.len(), 0);

        // Memory still prevents refetching.
        let calls = rpc_calls(&chain);
        assert!(resolver.resolve_new(&first).await.is_empty());
        assert_eq!(rpc_calls(&chain), calls);

        // Redis is back: the next write also flushes the backlog.
        cache.down.store(false, Ordering::SeqCst);
        let rows = resolver
            .resolve_new(&batch(&[(2, TokenStandard::Erc20)]))
            .await;
        resolver.mark_stored(&rows).await;
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&addr(1)).unwrap().symbol, "USDC");
        assert!(resolver.inner.pending_writes().is_empty());
    }

    #[tokio::test]
    async fn pending_writes_are_bounded() {
        let chain = FakeChain::new();
        for n in 0..50u64 {
            chain.add(addr(n), FakeToken::reverting());
        }
        let cache = FakeCache::new(1);
        cache.down.store(true, Ordering::SeqCst);
        let resolver = TokenResolver::from_parts(
            1,
            Some(MetadataFetcher::new(chain.clone(), fast_options())),
            Some(cache.clone() as Arc<dyn TokenCache>),
            &TokenResolverOptions { max_pending_writes: 10, ..options() },
        );

        for n in 0..50u64 {
            let rows = resolver
                .resolve_new(&batch(&[(n, TokenStandard::Erc20)]))
                .await;
            resolver.mark_stored(&rows).await;
        }

        assert_eq!(resolver.inner.pending_writes().len(), 10);
    }

    #[tokio::test]
    async fn memory_is_bounded_and_falls_back_to_redis() {
        let chain = FakeChain::new();
        for n in 0..100u64 {
            chain.add(addr(n), FakeToken::reverting());
        }
        let cache = FakeCache::new(1);
        let resolver = TokenResolver::from_parts(
            1,
            Some(MetadataFetcher::new(chain.clone(), fast_options())),
            Some(cache.clone() as Arc<dyn TokenCache>),
            &TokenResolverOptions { memory_capacity: 10, ..options() },
        );

        let all: Vec<_> =
            (0..100u64).map(|n| (n, TokenStandard::Erc20)).collect();
        let rows = resolver.resolve_new(&batch(&all)).await;
        assert_eq!(rows.len(), 100);
        resolver.mark_stored(&rows).await;
        assert_eq!(resolver.inner.known().known_len(), 10);

        // Evicted from memory but found in Redis: no RPC, no rows.
        let calls = rpc_calls(&chain);
        assert!(resolver.resolve_new(&batch(&all)).await.is_empty());
        assert_eq!(rpc_calls(&chain), calls);
    }

    /// Smoke test against a real Ethereum mainnet node:
    /// `TOKEN_TEST_RPC_URL=https://... cargo test -- --ignored live_rpc`
    #[tokio::test]
    #[ignore = "needs TOKEN_TEST_RPC_URL (ethereum mainnet)"]
    async fn live_rpc_smoke() {
        use alloy::primitives::address;

        let Ok(url) = std::env::var("TOKEN_TEST_RPC_URL") else {
            eprintln!("TOKEN_TEST_RPC_URL not set, skipping");
            return;
        };

        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let mkr = address!("9f8F72aA9304c8B593d555F12eF6589cC3A579A2");
        let bayc = address!("BC4CA0EdA7647A8aB7C2061c2E118A18a936f13D");
        let eoa = address!("00000000000000000000000000000000000000f1");

        let resolver =
            TokenResolver::new(1, Some(&url), None).await.unwrap();
        let rows = resolver
            .resolve_new(&HashMap::from([
                (weth, TokenStandard::Erc20),
                (mkr, TokenStandard::Erc20),
                (bayc, TokenStandard::Erc721),
                (eoa, TokenStandard::Erc1155),
            ]))
            .await;

        let row = |address: Address| {
            rows.iter().find(|row| row.address == address).unwrap()
        };
        // The code-less address yields no row at all.
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.address != eoa));
        assert_eq!(row(weth).name, "Wrapped Ether");
        assert_eq!(row(weth).symbol, "WETH");
        assert_eq!(row(weth).decimals, 18);
        // bytes32 name/symbol.
        assert_eq!(row(mkr).name, "Maker");
        assert_eq!(row(mkr).symbol, "MKR");
        assert_eq!(row(mkr).decimals, 18);
        assert_eq!(row(bayc).symbol, "BAYC");
        assert_eq!(row(bayc).r#type, "ERC721");
        assert_eq!(row(bayc).decimals, 0);

        // Wrong chain id for this node: refused at startup.
        let error = TokenResolver::new(56, Some(&url), None)
            .await
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        assert!(error.contains("serves chain 1"), "{error}");
    }

    /// Integration tests against a real Redis / Dragonfly.
    ///
    /// `TOKEN_CACHE_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test -- \
    /// --ignored`
    mod redis_integration {
        use super::*;
        use crate::tokens::cache::{
            redis_key, CachedToken, RedisTokenCache,
        };

        fn redis_url() -> String {
            std::env::var("TOKEN_CACHE_TEST_REDIS_URL")
                .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
        }

        fn unique_chain_id() -> u64 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos() as u64;
            9_000_000_000 + nanos
        }

        #[tokio::test]
        #[ignore = "needs a running redis"]
        async fn redis_round_trip_and_restart() {
            let chain_id = unique_chain_id();
            let chain = FakeChain::new();
            let mut all = Vec::new();
            for n in 0..2_500u64 {
                chain.add(addr(n), FakeToken::erc20("Token", "TKN", 18));
                all.push((n, TokenStandard::Erc20));
            }
            chain.add(addr(5_000), FakeToken::reverting());
            all.push((5_000, TokenStandard::Erc721));
            let tokens = batch(&all);

            let build = || async {
                let cache =
                    RedisTokenCache::connect(chain_id, &redis_url())
                        .await
                        .unwrap();
                TokenResolver::from_parts(
                    chain_id,
                    Some(MetadataFetcher::new(
                        chain.clone(),
                        fast_options(),
                    )),
                    Some(Arc::new(cache)),
                    &options(),
                )
            };

            let first = build().await;
            let rows = first.resolve_new(&tokens).await;
            assert_eq!(rows.len(), 2_501);

            // Nothing written before mark_stored.
            let client = redis::Client::open(redis_url()).unwrap();
            let mut conn =
                client.get_multiplexed_async_connection().await.unwrap();
            let key = redis_key(chain_id, &addr(7));
            let value: Option<String> = redis::cmd("GET")
                .arg(&key)
                .query_async(&mut conn)
                .await
                .unwrap();
            assert_eq!(value, None);

            first.mark_stored(&rows).await;

            let value: String = redis::cmd("GET")
                .arg(&key)
                .query_async(&mut conn)
                .await
                .unwrap();
            assert_eq!(
                value,
                r#"{"name":"Token","symbol":"TKN","decimals":18,"type":"ERC20"}"#
            );
            let ttl: i64 = redis::cmd("TTL")
                .arg(&key)
                .query_async(&mut conn)
                .await
                .unwrap();
            assert_eq!(ttl, -1, "successes must not expire");

            let negative: String = redis::cmd("GET")
                .arg(redis_key(chain_id, &addr(5_000)))
                .query_async(&mut conn)
                .await
                .unwrap();
            assert_eq!(
                CachedToken::from_json(&negative).unwrap().r#type,
                "ERC721"
            );

            // Restart: everything comes from Redis, no RPC.
            let calls = rpc_calls(&chain);
            let second = build().await;
            assert!(second.resolve_new(&tokens).await.is_empty());
            assert_eq!(rpc_calls(&chain), calls);

            // Cleanup.
            let mut pipe = redis::pipe();
            for (n, _) in &all {
                pipe.cmd("DEL")
                    .arg(redis_key(chain_id, &addr(*n)))
                    .ignore();
            }
            pipe.query_async::<()>(&mut conn).await.unwrap();
        }

        #[tokio::test]
        #[ignore = "network timing (connects to a closed local port)"]
        async fn unreachable_redis_never_stalls_or_fails() {
            let chain = FakeChain::new();
            chain.add(addr(1), FakeToken::erc20("USD Coin", "USDC", 6));

            let started = Instant::now();
            let cache =
                RedisTokenCache::connect(1, "redis://127.0.0.1:1/")
                    .await
                    .unwrap();
            let resolver = TokenResolver::from_parts(
                1,
                Some(MetadataFetcher::new(chain.clone(), fast_options())),
                Some(Arc::new(cache)),
                &options(),
            );

            let tokens = batch(&[(1, TokenStandard::Erc20)]);
            let rows = resolver.resolve_new(&tokens).await;
            assert_eq!(rows.len(), 1);
            resolver.mark_stored(&rows).await;
            assert!(resolver.resolve_new(&tokens).await.is_empty());
            assert!(started.elapsed() < Duration::from_secs(10));
        }
    }
}
