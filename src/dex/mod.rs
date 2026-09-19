//! Chain agnostic, DEX agnostic swap / liquidity / pool analytics.
//!
//! **Decode by event family from `logs`, never by router / factory
//! registry.** A Uniswap V2 fork on a chain nobody has heard of works on
//! day one: what identifies a swap is the `topic0` AND the exact shape of
//! the event, not who emitted it. `protocol` is therefore the event FAMILY
//! (`uniswap_v2`, `solidly`, `uniswap_v3`, `uniswap_v4`, `balancer_v2`,
//! `curve`), not a brand. See `README.md` in this directory for the table
//! / view catalogue, how to populate `quote_tokens`, and the known gaps.
//!
//! # Conventions
//!
//! * **Signed amounts are pool relative: positive = INTO the pool**,
//!   negative = out of the pool (the Uniswap V3 convention). V2 / Solidly
//!   `amountIn - amountOut` pairs are netted into it, Uniswap V4 deltas
//!   (caller relative) are negated into it. Mints are positive, burns
//!   negative.
//! * Two token families fill `amount0` / `amount1`; the multi asset
//!   families fill `amount_in` / `amount_out` plus `token_in` /
//!   `token_out` (Balancer, carried by the event) or `coin_in` /
//!   `coin_out` (Curve, indices into `dex_pools.tokens`). A swap uses
//!   exactly one of the two representations; `dex_swaps_v` unifies them at
//!   query time.
//! * `pool_id` is 32 bytes: the pool address left padded with zeros, or
//!   the native `bytes32` id (V4, Balancer). `emitter` is the contract that
//!   emitted the event (pool, PoolManager, Vault) and is part of the pool's
//!   identity, so a forked singleton can not collide with the original.
//! * Decoding never needs RPC. Pool tokens come from creation events; pools
//!   first seen mid-history are resolved in the background by
//!   [`PoolWorker`] and joined at query time.
//! * `dex_pools` holds one row per creation EVENT (positional key, like
//!   every block scoped table) plus at most one row of the RPC resolver.
//!   Readers go through the `dex_pool_current_v` view: event rows before
//!   resolver rows, then the earliest position - the first creation event
//!   wins, a forged later `PairCreated` can not replace a pool's tokens.
//! * Nothing is ever deleted (docs/design.md §2): a purge INSERTs
//!   tombstones into [`BASE_TABLES`] ([`tombstone_sql`]), the side tables
//!   follow through their materialized views, the aggregates are keyed by
//!   `epoch` and read through `*_v` views that apply the validity rule.
//! * `tx_from` / `tx_to` are filled on swaps AND liquidity rows by
//!   [`DexRows::attach_transactions`]. `dex_liquidity.tx_from` is who
//!   seeded / pulled liquidity; the event `sender` is usually a router.
//!
//! # Populating `quote_tokens`
//!
//! USD valuation needs to know which tokens are dollars and which one is
//! the wrapped native coin. No chain specific data ships with the indexer;
//! insert the rows once per chain (addresses are raw bytes):
//!
//! ```sql
//! INSERT INTO quote_tokens (chain, token, kind) VALUES
//!   (1, unhex('A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48'), 'stable'),  -- USDC
//!   (1, unhex('dAC17F958D2ee523a2206206994597C13D831ec7'), 'stable'),  -- USDT
//!   (1, unhex('C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2'), 'native');  -- WETH
//! -- pseudo addresses without a contract need decimals (and a symbol):
//! INSERT INTO quote_tokens (chain, token, kind, decimals, symbol) VALUES
//!   (1, unhex('0000000000000000000000000000000000000000'), 'native', 18, 'ETH'),
//!   (1, unhex('EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE'), 'native', 18, 'ETH');
//! ```
//!
//! To retire a quote token insert it again with `kind = ''`. The views pick
//! the change up immediately (nothing about USD is materialized).

pub mod decode;
pub mod derived;
pub mod events;
pub mod models;
pub mod resolve;
pub mod worker;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod integration_tests;
#[cfg(test)]
pub(crate) mod sql;

use std::collections::HashSet;

use alloy::primitives::{Address, B256};

pub use self::{
    decode::decode,
    derived::DEX_DERIVED,
    models::{
        pool_address_of, pool_id_of, DexLiquidity, DexPool, DexSwap,
        LiquidityKind, PoolSource, Protocol,
    },
    worker::{
        MissingPoolSource, PoolSink, PoolWorker, PoolWorkerOptions,
        PoolWorkerStats,
    },
};

/// Block scoped tables the indexer writes, in INSERT order. `purge_range`
/// tombstones exactly these (before `blocks`), see [`tombstone_sql`]. All
/// of them have a `block_number` column EXCEPT [`POOLS_TABLE`], whose block
/// column is [`POOLS_TABLE_BLOCK_COLUMN`] ([`block_column`]).
pub const BASE_TABLES: &[&str] =
    &["dex_swaps", "dex_liquidity", POOLS_TABLE];

/// Read-path tables fed by materialized views of [`BASE_TABLES`]. Never
/// written nor tombstoned directly: the views pass `_version`,
/// `is_deleted` and `epoch` through, so they follow their base table.
pub const SIDE_TABLES: &[&str] =
    &["dex_swaps_by_pool", "dex_swaps_by_trader", "dex_pools_by_token"];

pub const POOLS_TABLE: &str = "dex_pools";

/// `dex_pools` is block scoped through the block of its creation event.
pub const POOLS_TABLE_BLOCK_COLUMN: &str = "created_block";

/// The block column of a table of [`BASE_TABLES`] / [`SIDE_TABLES`].
pub fn block_column(table: &str) -> &'static str {
    if table == POOLS_TABLE {
        POOLS_TABLE_BLOCK_COLUMN
    } else {
        "block_number"
    }
}

/// Extra predicate a purge of `table` must carry. Rows of the RPC resolver
/// (`created_block = 0`) are chain STATE, not part of a block: a gap heal
/// starting at block 0 must not tombstone them. (If one is tombstoned
/// anyway the backfill resolves the pool again and the newer row wins.)
pub fn purge_filter(table: &str) -> Option<&'static str> {
    (table == POOLS_TABLE).then_some("source = 'event'")
}

/// The tombstone INSERT of `purge_range` for one of [`BASE_TABLES`]: every
/// live row of `chain` in `[from_block, to_block)` again, with `version`
/// and `is_deleted = 1`. `FINAL` then hides the rows here and - through
/// the materialized views - in the side tables. Idempotent, and safe to run
/// concurrently for different chains (it is only an INSERT).
pub fn tombstone_sql(
    table: &str,
    chain: u64,
    from_block: u64,
    to_block: Option<u64>,
    version: u64,
) -> String {
    let column = block_column(table);
    let mut sql = format!(
        "INSERT INTO {table} SELECT * REPLACE ({version} AS _version, \
         1 AS is_deleted) FROM {table} FINAL WHERE chain = {chain} \
         AND {column} >= {from_block}"
    );

    if let Some(to_block) = to_block {
        sql.push_str(&format!(" AND {column} < {to_block}"));
    }
    if let Some(filter) = purge_filter(table) {
        sql.push_str(&format!(" AND {filter}"));
    }

    sql
}

/// Pool ids of `dex_swaps` / `dex_liquidity` without a `dex_pools` row, for
/// the [`MissingPoolSource`] of the pipeline. Placeholders: `{chain}`,
/// `{limit}`. Columns: `pool_id FixedString(32)`, `emitter
/// FixedString(20)`, `protocol String`. Families described by events only
/// (V4, Balancer) are excluded: RPC can not resolve them. `dex_pools` is
/// read with `FINAL` (a tombstoned pool IS missing); the swap side is not,
/// on purpose: it is the big side, and resolving the pool of a reorged-out
/// swap is harmless.
pub const MISSING_POOLS_SQL: &str = "\
SELECT pool_id, emitter, any(family) AS protocol FROM (\
SELECT pool_id, emitter, toString(protocol) AS family \
FROM dex_swaps_by_pool WHERE chain = {chain} \
UNION ALL \
SELECT pool_id, emitter, toString(protocol) AS family \
FROM dex_liquidity WHERE chain = {chain}) \
WHERE family NOT IN ('uniswap_v4', 'balancer_v2') \
AND (pool_id, emitter) NOT IN (\
SELECT pool_id, emitter FROM dex_pools FINAL WHERE chain = {chain}) \
GROUP BY pool_id, emitter \
LIMIT {limit}";

/// A pool whose tokens are unknown and can be asked over RPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PoolCandidate {
    pub pool_id: B256,
    /// The pool contract (these families emit from the pool itself).
    pub address: Address,
    /// Family of the event that revealed the pool: which getters to try.
    pub protocol: Protocol,
}

/// `from` and `to` of a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TxOrigin {
    pub from: Address,
    /// `None` for contract creations.
    pub to: Option<Address>,
}

/// Rows decoded from one batch of logs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DexRows {
    pub pools: Vec<DexPool>,
    pub swaps: Vec<DexSwap>,
    pub liquidity: Vec<DexLiquidity>,
}

impl DexRows {
    pub fn rows(&self) -> usize {
        self.pools.len() + self.swaps.len() + self.liquidity.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows() == 0
    }

    /// Moves every row of `other` into `self`.
    pub fn append(&mut self, other: &mut DexRows) {
        self.pools.append(&mut other.pools);
        self.swaps.append(&mut other.swaps);
        self.liquidity.append(&mut other.liquidity);
    }

    /// Stamps the flush version on every row.
    pub fn set_version(&mut self, version: u64) {
        for pool in &mut self.pools {
            pool._version = version;
        }
        for swap in &mut self.swaps {
            swap._version = version;
        }
        for row in &mut self.liquidity {
            row._version = version;
        }
    }

    /// Stamps the chain's purge generation on every row (docs/design.md
    /// §2): the aggregates are keyed by it.
    pub fn set_epoch(&mut self, epoch: u32) {
        for pool in &mut self.pools {
            pool.epoch = epoch;
        }
        for swap in &mut self.swaps {
            swap.epoch = epoch;
        }
        for row in &mut self.liquidity {
            row.epoch = epoch;
        }
    }

    /// Fills `tx_from` / `tx_to` (and `trader`) from the transactions of
    /// the same batch. Optional: without it `trader` is the recipient (or
    /// sender) named by the event, which is often a router.
    pub fn attach_transactions<F>(&mut self, lookup: F)
    where
        F: Fn(&B256) -> Option<TxOrigin>,
    {
        for swap in &mut self.swaps {
            if let Some(origin) = lookup(&swap.transaction_hash) {
                swap.tx_from = origin.from;
                swap.tx_to = origin.to.unwrap_or_default();
                if !origin.from.is_zero() {
                    swap.trader = origin.from;
                }
            }
        }

        for row in &mut self.liquidity {
            if let Some(origin) = lookup(&row.transaction_hash) {
                row.tx_from = origin.from;
                row.tx_to = origin.to.unwrap_or_default();
            }
        }
    }

    /// Pools that traded in this batch, were not created in it and can be
    /// resolved over RPC: what to hand to [`PoolWorker::discover`].
    /// Deduplicated, in first-seen order.
    pub fn pool_candidates(&self) -> Vec<PoolCandidate> {
        let created: HashSet<(B256, Address)> = self
            .pools
            .iter()
            .map(|pool| (pool.pool_id, pool.emitter))
            .collect();

        let mut seen: HashSet<B256> = HashSet::new();
        let mut candidates = Vec::new();

        let traded = self
            .swaps
            .iter()
            .map(|swap| (swap.pool_id, swap.emitter, swap.protocol))
            .chain(
                self.liquidity
                    .iter()
                    .map(|row| (row.pool_id, row.emitter, row.protocol)),
            );

        for (pool_id, emitter, protocol) in traded {
            if !protocol.resolvable_by_rpc()
                || created.contains(&(pool_id, emitter))
                || !seen.insert(pool_id)
            {
                continue;
            }

            candidates.push(PoolCandidate {
                pool_id,
                address: emitter,
                protocol,
            });
        }

        candidates
    }

    /// Tokens named by the pools created in this batch (for the token
    /// metadata worker). Pseudo addresses of native coins are skipped.
    pub fn token_addresses(&self) -> Vec<Address> {
        let mut seen: HashSet<Address> = HashSet::new();

        self.pools
            .iter()
            .flat_map(|pool| {
                pool.tokens.iter().chain(pool.underlying_tokens.iter())
            })
            .copied()
            .filter(|token| !is_native_placeholder(token))
            .filter(|token| seen.insert(*token))
            .collect()
    }
}

/// Addresses that stand for the chain's native coin and have no contract:
/// zero (Uniswap V4) and `0xEeee...EEeE` (Curve).
pub fn is_native_placeholder(token: &Address) -> bool {
    token.is_zero() || token.0 .0 == [0xee; 20]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::sql::{normalize, statements, MIGRATIONS};
    use alloy::primitives::{I256, U256};

    fn swap(pool: Address, protocol: Protocol) -> DexSwap {
        DexSwap {
            chain: 1,
            block_number: 1,
            timestamp: 1,
            transaction_hash: B256::repeat_byte(9),
            log_index: 0,
            pool_id: pool_id_of(pool),
            emitter: pool,
            protocol,
            sender: Address::repeat_byte(1),
            recipient: Address::repeat_byte(2),
            tx_from: Address::ZERO,
            tx_to: Address::ZERO,
            trader: Address::repeat_byte(2),
            amount0: I256::ONE,
            amount1: I256::MINUS_ONE,
            token_in: Address::ZERO,
            token_out: Address::ZERO,
            amount_in: U256::ZERO,
            amount_out: U256::ZERO,
            coin_in: 0,
            coin_out: 0,
            underlying: false,
            sqrt_price_x96: U256::ZERO,
            liquidity: U256::ZERO,
            tick: 0,
            fee: 0,
            epoch: 0,
            _version: 0,
        }
    }

    #[test]
    fn versions_are_stamped_on_block_rows_only() {
        let mut rows = DexRows::default();
        rows.swaps
            .push(swap(Address::repeat_byte(7), Protocol::UniswapV2));
        rows.pools = decode(
            1,
            &[crate::dex::fixtures::v2_pair_created_for(
                Address::repeat_byte(7),
            )],
        )
        .pools;
        rows.liquidity =
            decode(1, &[crate::dex::fixtures::V2_SYNC.log()]).liquidity;

        rows.set_version(42);
        rows.set_epoch(3);

        assert_eq!((rows.swaps[0]._version, rows.swaps[0].epoch), (42, 3));
        assert_eq!((rows.pools[0]._version, rows.pools[0].epoch), (42, 3));
        assert_eq!(
            (rows.liquidity[0]._version, rows.liquidity[0].epoch),
            (42, 3)
        );
    }

    #[test]
    fn transactions_are_attached_to_liquidity_rows_too() {
        let mut rows = decode(1, &[crate::dex::fixtures::V2_MINT.log()]);
        let provider = Address::repeat_byte(0x55);
        let router = rows.liquidity[0].sender;

        rows.attach_transactions(|_| {
            Some(TxOrigin { from: provider, to: Some(router) })
        });

        assert_eq!(rows.liquidity[0].tx_from, provider);
        assert_eq!(rows.liquidity[0].tx_to, router);
        assert_ne!(rows.liquidity[0].sender, provider);
    }

    #[test]
    fn tombstones_are_plain_inserts() {
        assert_eq!(
            tombstone_sql("dex_swaps", 10, 500, None, 99),
            "INSERT INTO dex_swaps SELECT * REPLACE (99 AS _version, \
             1 AS is_deleted) FROM dex_swaps FINAL WHERE chain = 10 \
             AND block_number >= 500"
        );

        let pools = tombstone_sql(POOLS_TABLE, 10, 0, Some(7), 99);
        assert!(pools.ends_with(
            "AND created_block >= 0 AND created_block < 7 \
             AND source = 'event'"
        ));

        for table in BASE_TABLES {
            let sql = tombstone_sql(table, 1, 1, None, 1).to_uppercase();
            assert!(sql.starts_with("INSERT INTO "));
            assert!(!sql.replace("IS_DELETED", "").contains("DELETE"));
            assert!(!sql.contains("ALTER") && !sql.contains("DROP"));
        }
    }

    #[test]
    fn transactions_set_the_trader() {
        let mut rows = DexRows::default();
        rows.swaps
            .push(swap(Address::repeat_byte(7), Protocol::UniswapV2));

        let user = Address::repeat_byte(0x55);
        let router = Address::repeat_byte(0x66);
        rows.attach_transactions(|hash| {
            (*hash == B256::repeat_byte(9))
                .then_some(TxOrigin { from: user, to: Some(router) })
        });

        assert_eq!(rows.swaps[0].tx_from, user);
        assert_eq!(rows.swaps[0].tx_to, router);
        assert_eq!(rows.swaps[0].trader, user);

        // Unknown transaction: untouched.
        let mut rows = DexRows::default();
        rows.swaps
            .push(swap(Address::repeat_byte(7), Protocol::UniswapV2));
        rows.attach_transactions(|_| None);
        assert_eq!(rows.swaps[0].trader, Address::repeat_byte(2));
    }

    #[test]
    fn candidates_skip_singletons_and_pools_created_in_the_batch() {
        let known = Address::repeat_byte(7);
        let unknown = Address::repeat_byte(8);
        let manager = Address::repeat_byte(9);

        let mut rows = DexRows::default();
        rows.swaps.push(swap(known, Protocol::UniswapV2));
        rows.swaps.push(swap(unknown, Protocol::UniswapV3));
        rows.swaps.push(swap(unknown, Protocol::UniswapV3));
        rows.swaps.push(swap(manager, Protocol::UniswapV4));

        let logs = [crate::dex::fixtures::v2_pair_created_for(known)];
        rows.pools = decode(1, &logs).pools;
        assert_eq!(rows.pools.len(), 1);

        assert_eq!(
            rows.pool_candidates(),
            vec![PoolCandidate {
                pool_id: pool_id_of(unknown),
                address: unknown,
                protocol: Protocol::UniswapV3,
            }]
        );
    }

    #[test]
    fn native_placeholders_are_not_tokens() {
        assert!(is_native_placeholder(&Address::ZERO));
        assert!(is_native_placeholder(&Address::repeat_byte(0xee)));
        assert!(!is_native_placeholder(&Address::repeat_byte(0xef)));
    }

    /// `CREATE TABLE` statements of the migrations as (name, body).
    fn tables() -> Vec<(String, String)> {
        MIGRATIONS
            .iter()
            .flat_map(|(_, sql)| statements(sql))
            .map(|statement| normalize(&statement))
            .filter_map(|statement| {
                let rest = statement
                    .strip_prefix("CREATE TABLE IF NOT EXISTS ")?;
                let (name, body) = rest.split_once(' ')?;
                Some((name.to_owned(), body.to_owned()))
            })
            .collect()
    }

    #[test]
    fn every_table_with_a_block_column_is_block_scoped() {
        let mut expected: Vec<String> = tables()
            .into_iter()
            .filter(|(_, body)| {
                body.contains(" block_number UInt64")
                    || body.contains(" created_block UInt64")
            })
            .map(|(name, _)| name)
            .collect();
        expected.sort();

        let mut listed: Vec<String> = BASE_TABLES
            .iter()
            .chain(SIDE_TABLES)
            .map(|name| name.to_string())
            .collect();
        listed.sort();

        assert_eq!(listed, expected);

        for (name, body) in tables() {
            if listed.contains(&name) {
                let column = format!(" {} UInt64", block_column(&name));
                assert!(body.contains(&column), "{name}");
            }
        }
    }

    /// Every materialized view as (name, target, select).
    fn materialized_views() -> Vec<(String, String, String)> {
        MIGRATIONS
            .iter()
            .flat_map(|(_, sql)| statements(sql))
            .map(|statement| normalize(&statement))
            .filter_map(|statement| {
                let rest = statement.strip_prefix(
                    "CREATE MATERIALIZED VIEW IF NOT EXISTS ",
                )?;
                let (name, rest) = rest.split_once(" TO ")?;
                let (target, select) = rest.split_once(" AS ")?;
                Some((
                    name.to_owned(),
                    target.to_owned(),
                    select.to_owned(),
                ))
            })
            .collect()
    }

    #[test]
    fn migrations_follow_the_schema_rules() {
        for (name, sql) in MIGRATIONS {
            assert!(name.starts_with("001"), "{name}");

            // Naive splitters must survive: no `;` in comments / strings.
            for line in sql.lines() {
                if let Some((_, comment)) = line.split_once("--") {
                    assert!(!comment.contains(';'), "{name}: {line}");
                }
            }

            for statement in statements(sql) {
                let statement = normalize(&statement);
                assert!(
                    statement.starts_with("CREATE TABLE IF NOT EXISTS ")
                        || statement
                            .starts_with("CREATE VIEW IF NOT EXISTS ")
                        || statement.starts_with(
                            "CREATE MATERIALIZED VIEW IF NOT EXISTS "
                        ),
                    "{name}: {statement}"
                );
                // The database comes from the connection URL.
                assert!(!statement.contains("indexer."), "{name}");
                assert!(!statement.contains("PROJECTION"), "{name}");
                // Section 9 of the design: neither exists any more.
                assert!(!statement.contains("traces"), "{name}");
                assert!(!statement.contains("contracts"), "{name}");
            }
        }

        for (name, body) in tables() {
            let block_scoped = BASE_TABLES.contains(&name.as_str())
                || SIDE_TABLES.contains(&name.as_str());
            let aggregating = body.contains("AggregatingMergeTree");

            if block_scoped {
                assert!(
                    body.contains(
                        "ReplacingMergeTree(_version, is_deleted)"
                    ),
                    "{name}"
                );
                for column in [
                    " _version UInt64",
                    " is_deleted UInt8 DEFAULT 0",
                    " epoch UInt32 DEFAULT 0",
                ] {
                    assert!(body.contains(column), "{name}: {column}");
                }
            } else if aggregating {
                // Epoch is the LAST key column of every aggregate.
                assert!(body.contains(" epoch UInt32,"), "{name}");
                assert!(body.trim_end().ends_with(", epoch)"), "{name}");
                assert!(
                    body.contains("PARTITION BY toYYYYMM(bucket)"),
                    "{name}"
                );
            } else {
                assert_eq!(name, "quote_tokens");
            }

            // 50+ chains in one database: months only for the base
            // tables, chain only for lookups - never both.
            assert!(!body.contains("PARTITION BY (chain"), "{name}");
            if ["dex_swaps", "dex_liquidity"].contains(&name.as_str()) {
                assert!(
                    body.contains("PARTITION BY toYYYYMM(timestamp)"),
                    "{name}"
                );
            } else if block_scoped {
                assert!(body.contains("PARTITION BY chain"), "{name}");
            }
        }
    }

    #[test]
    fn side_tables_follow_tombstones_and_aggregates_skip_them() {
        let views = materialized_views();
        assert_eq!(views.len(), 8);

        for (name, target, select) in views {
            if SIDE_TABLES.contains(&target.as_str()) {
                for column in ["epoch", "_version", "is_deleted"] {
                    assert!(
                        select.contains(&format!(" {column}")),
                        "{name}: {column}"
                    );
                }
                assert!(!select.contains("is_deleted = 0"), "{name}");
            } else {
                assert!(select.contains("WHERE is_deleted = 0"), "{name}");
                assert!(select.trim_end().ends_with(", epoch"), "{name}");
            }
        }
    }

    #[test]
    fn nothing_is_ever_deleted() {
        for (name, sql) in MIGRATIONS {
            for statement in statements(sql) {
                let upper = normalize(&statement).to_uppercase();
                for verb in
                    ["DELETE FROM", "ALTER TABLE", "DROP ", "TRUNCATE"]
                {
                    assert!(!upper.contains(verb), "{name}: {verb}");
                }
            }
        }

        for table in DEX_DERIVED {
            let upper = table.rebuild_sql.to_uppercase();
            assert!(upper.starts_with("INSERT INTO "), "{}", table.name);
            assert!(
                !upper.replace("IS_DELETED", "").contains("DELETE"),
                "{}",
                table.name
            );
        }
    }

    #[test]
    fn missing_pools_sql_has_its_placeholders() {
        assert_eq!(MISSING_POOLS_SQL.matches("{chain}").count(), 3);
        assert!(MISSING_POOLS_SQL.contains("FROM dex_pools FINAL"));
        assert_eq!(MISSING_POOLS_SQL.matches("{limit}").count(), 1);
    }
}
