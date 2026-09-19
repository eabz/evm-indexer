pub mod derived;
#[cfg(test)]
mod integration_tests;
pub mod migrate;
pub mod models;
pub mod ranges;
pub mod schema;

pub use schema::{
    block_number_column, tables_with_block_number, tombstone_sql,
    BASE_TABLES, SIDE_TABLES,
};

use crate::{metrics::Metrics, pipeline::modules::ModuleRows};
use alloy::primitives::B256;
use anyhow::{anyhow, bail, Context, Result};
use clickhouse::{Client, Row};
use log::{info, warn};
use models::{
    block::DatabaseBlock, erc1155_transfer::DatabaseERC1155Transfer,
    erc20_transfer::DatabaseERC20Transfer,
    erc721_transfer::DatabaseERC721Transfer, log::DatabaseLog,
    transaction::DatabaseTransaction, withdrawal::DatabaseWithdrawal,
};
use ranges::{
    assemble_missing_ranges, contiguous_ranges, gaps_sql, is_dense,
    stats_sql, BlockRange, DatabaseCheckpoint, GapRow, MissingRanges,
    RangeStats, MAX_GAPS_PER_PASS,
};
use serde::Serialize;
use std::{
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Last `_version` handed out by [`next_version`].
static LAST_VERSION: AtomicU64 = AtomicU64::new(0);

/// `_version` for a flush: unix time in milliseconds, taken ONCE per flush
/// and stamped on every row of it (`RowBatch::set_version`).
///
/// Strictly increasing inside a process even when the wall clock steps
/// back, so a later flush of the same block always wins the
/// `ReplacingMergeTree(_version)` dedup.
pub fn next_version() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default();

    let mut last = LAST_VERSION.load(Ordering::Relaxed);
    loop {
        let next = now.max(last + 1);
        match LAST_VERSION.compare_exchange_weak(
            last,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return next,
            Err(current) => last = current,
        }
    }
}

/// Never hand out a `_version` at or below `stored` again.
///
/// `next_version` is only monotonic INSIDE a process. After a restart on a
/// host whose wall clock moved back (NTP step, VM snapshot, skewed
/// container host) it would hand out versions BELOW the stored ones: the
/// tombstones of a purge (version V) would then beat the canonical rows
/// streamed afterwards (version V' < V), and the range would stay
/// invisible until the clock passes V. So every process that writes seeds
/// the counter with the highest version the chain has stored
/// ([`Database::seed_version`]).
pub fn seed_version(stored: u64) {
    LAST_VERSION.fetch_max(stored, Ordering::Relaxed);
}

/// Attempts per table insert before the flush is reported as failed.
const INSERT_ATTEMPTS: u32 = 6;
const INSERT_BACKOFF_BASE: Duration = Duration::from_secs(1);
const INSERT_BACKOFF_MAX: Duration = Duration::from_secs(30);

const CONNECT_ATTEMPTS: u32 = 10;

/// Client side insert timeouts. Without them a black-holed connection
/// blocks the flush (and with it the whole indexer) forever, because
/// `wait_for_async_insert=1` makes the server hold the response.
///
/// Sending one chunk to the socket.
const INSERT_SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// Waiting for the server's answer after the last chunk. Must stay ABOVE
/// the server's `wait_for_async_insert_timeout` (120s by default) so the
/// server's own, more descriptive, timeout error wins when it is alive.
const INSERT_END_TIMEOUT: Duration = Duration::from_secs(180);
/// Fetching the table schema for the insert (cached after the first time).
const INSERT_PREPARE_TIMEOUT: Duration = Duration::from_secs(30);

/// Sets `$field` on every block scoped row of a [`RowBatch`].
macro_rules! stamp {
    ($batch:expr, $field:ident = $value:expr) => {{
        stamp!(@rows $batch, $field = $value;
            blocks, logs, transactions, withdrawals,
            erc20_transfers, erc721_transfers, erc1155_transfers);
    }};
    (@rows $batch:expr, $field:ident = $value:expr; $($rows:ident),*) => {$(
        for row in &mut $batch.$rows {
            row.$field = $value;
        }
    )*};
}

/// Rows produced from one or more HyperSync responses. Always holds WHOLE
/// blocks: every row that belongs to a block in `blocks` is in here too.
#[derive(Debug, Default)]
pub struct RowBatch {
    pub blocks: Vec<DatabaseBlock>,
    pub logs: Vec<DatabaseLog>,
    pub transactions: Vec<DatabaseTransaction>,
    pub withdrawals: Vec<DatabaseWithdrawal>,
    pub erc20_transfers: Vec<DatabaseERC20Transfer>,
    pub erc721_transfers: Vec<DatabaseERC721Transfer>,
    pub erc1155_transfers: Vec<DatabaseERC1155Transfer>,
    /// Rows of the decoder modules (DEX, ...), decoded from `logs` in
    /// transform. Stored BEFORE `blocks`, like every other child.
    pub modules: ModuleRows,
}

impl RowBatch {
    /// Total rows over all tables.
    pub fn rows(&self) -> usize {
        self.blocks.len()
            + self.logs.len()
            + self.transactions.len()
            + self.withdrawals.len()
            + self.erc20_transfers.len()
            + self.erc721_transfers.len()
            + self.erc1155_transfers.len()
            + self.modules.rows()
    }

    pub fn is_empty(&self) -> bool {
        self.rows() == 0
    }

    /// Moves every row of `other` into `self`.
    pub fn append(&mut self, other: &mut RowBatch) {
        self.blocks.append(&mut other.blocks);
        self.logs.append(&mut other.logs);
        self.transactions.append(&mut other.transactions);
        self.withdrawals.append(&mut other.withdrawals);
        self.erc20_transfers.append(&mut other.erc20_transfers);
        self.erc721_transfers.append(&mut other.erc721_transfers);
        self.erc1155_transfers.append(&mut other.erc1155_transfers);
        self.modules.append(&mut other.modules);
    }

    /// Stamps `_version` on every block scoped row of the batch (module
    /// rows included). Called once per flush with [`next_version`].
    pub fn set_version(&mut self, version: u64) {
        stamp!(self, _version = version);
        self.modules.set_version(version);
    }

    /// Stamps the chain's current purge generation on every block scoped
    /// row of the batch (docs/design.md, section 2). Called once per flush,
    /// like [`Self::set_version`]: the aggregates file every contribution
    /// under the epoch of the rows it came from.
    pub fn set_epoch(&mut self, epoch: u32) {
        stamp!(self, epoch = epoch);
        self.modules.set_epoch(epoch);
    }

    /// `_version` of the batch (0 before [`Self::set_version`]).
    pub fn version(&self) -> u64 {
        self.blocks.first().map(|block| block._version).unwrap_or(0)
    }

    /// `epoch` of the batch (0 before [`Self::set_epoch`]).
    pub fn epoch(&self) -> u32 {
        self.blocks.first().map(|block| block.epoch).unwrap_or(0)
    }

    /// Lowest and highest block number in the batch.
    pub fn block_span(&self) -> Option<(u64, u64)> {
        let min = self.blocks.iter().map(|b| b.number).min()?;
        let max = self.blocks.iter().map(|b| b.number).max()?;
        Some((min, max))
    }
}

/// A single `FixedString(32)` column.
#[serde_with::serde_as]
#[derive(Debug, Row, serde::Deserialize)]
struct HashRow {
    #[serde_as(as = "crate::utils::format::SerB256")]
    hash: B256,
}

/// Connection settings extracted from the database url.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseParams {
    /// `scheme://host:port` of the ClickHouse HTTP interface.
    pub endpoint: String,
    pub user: String,
    pub password: String,
    pub database: String,
    /// Things the operator should know about how the url was interpreted
    /// (logged at startup; never contain the password).
    pub warnings: Vec<String>,
}

/// Ports of the ClickHouse NATIVE protocol (plain / TLS). The client speaks
/// HTTP, so these never work; they show up when a 2.x url is reused.
const NATIVE_PORT: u16 = 9000;
const NATIVE_TLS_PORT: u16 = 9440;
const HTTP_PORT: u16 = 8123;
const HTTPS_PORT: u16 = 8443;

impl DatabaseParams {
    /// `scheme://user:password@host[:port]/database`.
    ///
    /// The clickhouse crate speaks HTTP, so the port defaults to 8123
    /// (`http`) / 8443 (`https`). The legacy `clickhouse://` scheme found in
    /// older compose files is accepted as an alias of `http://`; since such
    /// urls usually carry the native port, `clickhouse://host:9000` is
    /// rewritten to `http://host:8123` (and `:9440` to `https://host:8443`)
    /// with a warning. An explicit `http(s)://` url is never rewritten,
    /// only warned about.
    pub fn parse(database_url: &str) -> Result<Self> {
        // Errors never echo the url: it contains the password.
        let url = url::Url::parse(database_url).map_err(|e| {
            anyhow!(
                "invalid database url ({e}), expected \
                 http://user:password@host:port/database"
            )
        })?;

        let mut warnings = Vec::new();

        let (scheme, port) = match (url.scheme(), url.port()) {
            ("clickhouse", Some(NATIVE_PORT)) => {
                warnings.push(format!(
                    "The database url uses the legacy clickhouse:// scheme \
                     with the native protocol port {NATIVE_PORT}. The \
                     indexer speaks HTTP: using http on port {HTTP_PORT} \
                     instead. Update the url to http://host:{HTTP_PORT}/db."
                ));
                ("http", HTTP_PORT)
            }
            ("clickhouse", Some(NATIVE_TLS_PORT)) => {
                warnings.push(format!(
                    "The database url uses the legacy clickhouse:// scheme \
                     with the native TLS port {NATIVE_TLS_PORT}. The \
                     indexer speaks HTTP: using https on port {HTTPS_PORT} \
                     instead. Update the url to https://host:{HTTPS_PORT}/db."
                ));
                ("https", HTTPS_PORT)
            }
            ("clickhouse", port) => ("http", port.unwrap_or(HTTP_PORT)),
            ("http", port) => ("http", port.unwrap_or(HTTP_PORT)),
            ("https", port) => ("https", port.unwrap_or(HTTPS_PORT)),
            (other, _) => bail!(
                "unsupported database url scheme '{other}', \
                 use http:// or https://"
            ),
        };

        // Explicit http(s): respected as written, but almost certainly a
        // mistake.
        if warnings.is_empty()
            && (port == NATIVE_PORT || port == NATIVE_TLS_PORT)
        {
            warnings.push(format!(
                "The database url points at port {port}, which is normally \
                 the ClickHouse NATIVE protocol. The indexer speaks HTTP \
                 (default ports {HTTP_PORT} / {HTTPS_PORT}); if the \
                 connection fails, fix the port."
            ));
        }

        let host = url.host_str().context("no host in database url")?;

        let database = url.path().trim_matches('/');
        if database.is_empty() {
            bail!("no database name in database url");
        }

        Ok(Self {
            endpoint: format!("{scheme}://{host}:{port}"),
            user: url.username().to_string(),
            password: url.password().unwrap_or("").to_string(),
            database: database.to_string(),
            warnings,
        })
    }
}

/// How many distinct monthly partitions one INSERT of a flush may touch.
///
/// Base tables and aggregates are `PARTITION BY toYYYYMM(...)` and
/// ClickHouse refuses an insert block that touches more than
/// `max_partitions_per_insert_block` (100 by default) partitions with code
/// 252 - in the table AND in everything its materialized views feed. A
/// flush normally covers hours, but a pass healing gaps spread over the
/// whole history, or a chain with a very long block time, can put blocks
/// of hundreds of months into one. Below the limit so a view whose bucket
/// lands in a neighbouring month still fits.
pub const MAX_MONTHS_PER_FLUSH: usize = 90;

/// A slice of a flush: the rows whose `timestamp` is in `[from, to)`.
/// Whole months, so no monthly partition is ever written by two of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushWindow {
    pub from: u32,
    /// Exclusive; `u32::MAX` for the last window.
    pub to: u32,
}

impl FlushWindow {
    /// The single window every ordinary flush uses.
    pub const ALL: Self = Self { from: 0, to: u32::MAX };

    pub fn holds(&self, timestamp: u32) -> bool {
        timestamp >= self.from
            && (timestamp < self.to || self.to == u32::MAX)
    }
}

/// The windows a flush of these block timestamps has to be split into,
/// oldest first: one, unless the flush touches more than
/// [`MAX_MONTHS_PER_FLUSH`] distinct UTC months.
///
/// Splitting by month (not by block count) is what the partition key asks
/// for, and it keeps whole blocks together: a child row carries its
/// block's timestamp, so it always lands in the same window as its block.
pub fn flush_windows(
    timestamps: impl Iterator<Item = u32>,
) -> Vec<FlushWindow> {
    // The END of the month a timestamp falls into: a grouping key and a
    // window boundary in one.
    let mut ends: Vec<u32> = timestamps
        .map(|timestamp| {
            derived::next_month_start(timestamp).min(u64::from(u32::MAX))
                as u32
        })
        .collect();
    ends.sort_unstable();
    ends.dedup();

    if ends.len() <= MAX_MONTHS_PER_FLUSH {
        return vec![FlushWindow::ALL];
    }

    let mut windows = Vec::new();
    let mut from = 0;

    for chunk in ends.chunks(MAX_MONTHS_PER_FLUSH) {
        let to = *chunk.last().expect("chunks are never empty");
        windows.push(FlushWindow { from, to });
        from = to;
    }

    // The last window stays open ended: a row a month boundary rounded
    // away must never fall outside every window.
    if let Some(last) = windows.last_mut() {
        last.to = u32::MAX;
    }

    windows
}

/// A row that belongs to a monthly partition: what [`flush_windows`]
/// groups by and what [`select`] filters on. Implemented for every row
/// type a flush writes (the module ones in `pipeline::modules`), so a new
/// table can not silently skip the split - it would not compile.
pub trait Timestamped {
    /// Unix seconds deciding the row's partition.
    fn timestamp(&self) -> u32;
}

macro_rules! timestamped {
    ($($row:ty),+ $(,)?) => {$(
        impl Timestamped for $row {
            fn timestamp(&self) -> u32 {
                self.timestamp
            }
        }
    )+};
}

timestamped!(
    DatabaseBlock,
    DatabaseTransaction,
    DatabaseLog,
    DatabaseWithdrawal,
    DatabaseERC20Transfer,
    DatabaseERC721Transfer,
    DatabaseERC1155Transfer,
);

/// The rows of `window`, borrowed.
pub fn select<T: Timestamped>(rows: &[T], window: FlushWindow) -> Vec<&T> {
    rows.iter().filter(|row| window.holds(row.timestamp())).collect()
}

/// Identifies one flush for the server side insert deduplication
/// (docs/design.md, section 2, "Retried inserts must not double count"):
/// the same rows retried carry the same token, so ClickHouse drops the
/// second copy INCLUDING what it would have pushed through the
/// materialized views. `version` is unique per flush, so rows that are
/// legitimately written again later (re-streamed after a purge) never
/// collide with an old token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushKey {
    pub chain: u64,
    /// First and last block of the flush (inclusive).
    pub span: (u64, u64),
    pub version: u64,
}

impl FlushKey {
    /// `insert_deduplication_token` of this flush for `table`.
    pub fn token(&self, table: &str) -> String {
        format!(
            "{table}:{}:{}-{}:{}",
            self.chain, self.span.0, self.span.1, self.version
        )
    }
}

#[derive(Clone)]
pub struct Database {
    pub chain_id: u64,
    /// Queries and the block scoped inserts of a flush: SYNCHRONOUS
    /// inserts. An acknowledged insert is a written part, which is what
    /// makes `blocks`-last a commit marker. (Asynchronous inserts are not
    /// an option for them: ClickHouse refuses
    /// `deduplicate_blocks_in_dependent_materialized_views` together with
    /// `async_insert`, and a flush is one big batch anyway.)
    pub db: Client,
    /// Small, frequent inserts of the background workers (`tokens`,
    /// resolver rows of `dex_pools`): batched server side, acknowledged
    /// once durable.
    small: Client,
    metrics: Metrics,
    /// The chain's purge generation, stamped on every row of a flush.
    epoch: Arc<AtomicU32>,
}

impl Database {
    pub async fn new(database_url: &str, chain_id: u64) -> Result<Self> {
        let params = DatabaseParams::parse(database_url)?;

        for warning in &params.warnings {
            warn!("{warning}");
        }

        info!(
            "Connecting to ClickHouse at {} (database '{}').",
            params.endpoint, params.database
        );

        let db = Client::default()
            .with_url(&params.endpoint)
            .with_user(&params.user)
            .with_password(&params.password)
            .with_database(&params.database);

        let small = db
            .clone()
            .with_option("async_insert", "1")
            // REQUIRED: an acknowledged insert must mean durable data.
            .with_option("wait_for_async_insert", "1");

        let database = Self {
            chain_id,
            db,
            small,
            metrics: Metrics::disabled(),
            epoch: Arc::new(AtomicU32::new(0)),
        };

        database.wait_until_ready().await?;

        Ok(database)
    }

    /// A handle that was never connected, for the unit tests of the code
    /// that only BUILDS statements (the purge's table lists and
    /// predicates). Every query on it fails.
    #[cfg(test)]
    pub(crate) fn offline(chain_id: u64) -> Self {
        let db = Client::default().with_url("http://127.0.0.1:1");
        Self {
            chain_id,
            small: db.clone(),
            db,
            metrics: Metrics::disabled(),
            epoch: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Same database, reporting rows / retries to `metrics`.
    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.metrics = metrics;
        self
    }

    /// The epoch the next flush is stamped with.
    pub fn epoch(&self) -> u32 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Adopts `epoch` (after a purge). Shared by every clone.
    pub fn set_epoch(&self, epoch: u32) {
        self.epoch.store(epoch, Ordering::SeqCst);
    }

    /// Seeds [`next_version`] from the highest `_version` stored for the
    /// chain over `tables` (every table this process may write a newer
    /// version of a row into). Read a few times: ClickHouse has no
    /// read-your-writes, and the newest part is exactly what matters.
    ///
    /// Cost: one column per table, and `_version` compresses to almost
    /// nothing (it is constant per flush). Once per process start.
    pub async fn seed_version(&self, tables: &[&str]) -> Result<u64> {
        const READS: u32 = 3;

        let selects: Vec<String> = tables
            .iter()
            .map(|table| {
                format!(
                    "SELECT max(_version) AS v FROM `{table}` \
                     WHERE chain = {}",
                    self.chain_id
                )
            })
            .collect();

        let sql = format!(
            "SELECT toUInt64(max(v)) FROM ({})",
            selects.join(" UNION ALL ")
        );

        let mut stored = 0u64;
        for read in 0..READS {
            if read > 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let seen: u64 = self
                .db
                .query(&sql)
                .fetch_one()
                .await
                .context("query the highest stored _version")?;
            stored = stored.max(seen);
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or_default();

        if stored > now {
            warn!(
                "Chain {}: the newest stored row version is {} ms AHEAD of \
                 this host's clock (the clock stepped back, or another \
                 host's clock is ahead). New rows continue from the stored \
                 version, so nothing is hidden.",
                self.chain_id,
                stored - now
            );
        }

        seed_version(stored);

        Ok(stored)
    }

    /// The chain's current purge generation: `max(epoch)` of its `reorgs`
    /// rows, 0 when it never had a purge.
    pub async fn current_epoch(&self) -> Result<u32> {
        self.db
            .query(&format!(
                "SELECT toUInt32(max(epoch)) FROM reorgs WHERE chain = {}",
                self.chain_id
            ))
            .fetch_one::<u32>()
            .await
            .context("query the current epoch")
    }

    /// Reads [`Self::current_epoch`] and adopts it when it is NEWER (a
    /// purge of another process, e.g. `indexer backfill`; `reorgs` may lag
    /// behind a purge of this process for a moment, so it never goes
    /// back). Returns the epoch in force.
    pub async fn refresh_epoch(&self) -> Result<u32> {
        let stored = self.current_epoch().await?;
        Ok(self.epoch.fetch_max(stored, Ordering::SeqCst).max(stored))
    }

    async fn wait_until_ready(&self) -> Result<()> {
        let mut attempt = 0;

        loop {
            attempt += 1;

            match self.db.query("SELECT 1").fetch_one::<u8>().await {
                Ok(_) => {
                    info!("Connected to ClickHouse.");
                    return Ok(());
                }
                Err(e) if attempt >= CONNECT_ATTEMPTS => {
                    return Err(anyhow!(e).context(format!(
                        "could not connect to ClickHouse after \
                         {CONNECT_ATTEMPTS} attempts"
                    )));
                }
                Err(e) => {
                    let wait =
                        Duration::from_secs(2_u64.pow(attempt.min(5)));
                    warn!(
                        "ClickHouse connection attempt {attempt}/\
                         {CONNECT_ATTEMPTS} failed: {e}. Retrying in {wait:?}."
                    );
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    /// Missing block ranges inside `range`, computed in ClickHouse.
    pub async fn missing_ranges(
        &self,
        range: BlockRange,
    ) -> Result<MissingRanges> {
        if range.is_empty() {
            return Ok(assemble_missing_ranges(
                range,
                RangeStats { indexed: 0, max_number: 0 },
                &[],
                MAX_GAPS_PER_PASS,
            ));
        }

        let stats = self
            .db
            .query(&stats_sql(self.chain_id, range))
            .fetch_one::<RangeStats>()
            .await
            .context("query indexed block stats")?;

        // Common case (nothing indexed, or no holes): no gap scan needed.
        let gaps = if stats.indexed == 0 || is_dense(range, stats) {
            Vec::new()
        } else {
            self.db
                .query(&gaps_sql(self.chain_id, range, MAX_GAPS_PER_PASS))
                .fetch_all::<GapRow>()
                .await
                .context("query missing block ranges")?
        };

        Ok(assemble_missing_ranges(range, stats, &gaps, MAX_GAPS_PER_PASS))
    }

    /// Hash of an indexed canonical block, if present. `FINAL`: the latest
    /// version of the row is the canonical one.
    pub async fn block_hash(&self, number: u64) -> Result<Option<B256>> {
        let query = format!(
            "SELECT hash FROM blocks FINAL WHERE chain = {} AND number = {} \
             LIMIT 1",
            self.chain_id, number
        );

        let row = self
            .db
            .query(&query)
            .fetch_optional::<HashRow>()
            .await
            .context("query block hash")?;

        Ok(row.map(|row| row.hash))
    }

    /// Highest live block of the chain (`FINAL`), `None` when empty.
    pub async fn stored_head(&self) -> Result<Option<u64>> {
        let (count, max): (u64, u64) = self
            .db
            .query(&format!(
                "SELECT toUInt64(count()), toUInt64(max(number)) \
                 FROM blocks FINAL WHERE chain = {}",
                self.chain_id
            ))
            .fetch_one()
            .await
            .context("query the stored head")?;

        Ok((count > 0).then_some(max))
    }

    /// Stores a batch. Every non-block table (module tables included) is
    /// written concurrently, then `blocks` LAST: a block row only exists
    /// once all of its data is durable, which is what resume / gap
    /// detection relies on. The checkpoint rows follow `blocks`.
    ///
    /// Returns an error only after every retry is exhausted, in which case
    /// NO block row of this batch was written (or, for a failed checkpoint
    /// insert, everything was: checkpoints are an index, `blocks` decides).
    pub async fn store(&self, batch: &RowBatch) -> Result<()> {
        if batch.block_span().is_none() {
            if batch.is_empty() {
                return Ok(());
            }
            // Rows can not be committed without their block.
            bail!("refusing to store a batch of rows without block rows");
        }

        let windows = flush_windows(
            batch.blocks.iter().map(|block| block.timestamp),
        );

        if windows.len() > 1 {
            info!(
                "Chain {}: this flush spans {} UTC months, more than one \
                 insert may touch; storing it in {} parts, oldest first.",
                self.chain_id,
                windows.len() * MAX_MONTHS_PER_FLUSH,
                windows.len()
            );
        }

        // Oldest first, each part complete in itself (children, then
        // `blocks`, then its checkpoints): a crash between two parts leaves
        // the later months as ordinary gaps.
        for window in windows {
            self.store_window(batch, window).await?;
        }

        Ok(())
    }

    /// One part of a flush: every row whose `timestamp` falls into
    /// `window`. With the single [`FlushWindow::ALL`] this is the whole
    /// flush and behaves exactly as an unsplit one - the deduplication
    /// token included, because the key's block span is computed from the
    /// window's own blocks.
    async fn store_window(
        &self,
        batch: &RowBatch,
        window: FlushWindow,
    ) -> Result<()> {
        let blocks: Vec<&DatabaseBlock> = batch
            .blocks
            .iter()
            .filter(|block| window.holds(block.timestamp))
            .collect();

        let Some(span) = blocks
            .iter()
            .map(|block| block.number)
            .min()
            .zip(blocks.iter().map(|block| block.number).max())
        else {
            return Ok(());
        };

        let key = FlushKey {
            chain: self.chain_id,
            span,
            version: batch.version(),
        };

        let logs = select(&batch.logs, window);
        let transactions = select(&batch.transactions, window);
        let withdrawals = select(&batch.withdrawals, window);
        let erc20 = select(&batch.erc20_transfers, window);
        let erc721 = select(&batch.erc721_transfers, window);
        let erc1155 = select(&batch.erc1155_transfers, window);

        let results = tokio::join!(
            self.insert_flush_refs("logs", &logs, &key),
            self.insert_flush_refs("transactions", &transactions, &key),
            self.insert_flush_refs("withdrawals", &withdrawals, &key),
            self.insert_flush_refs("erc20_transfers", &erc20, &key),
            self.insert_flush_refs("erc721_transfers", &erc721, &key),
            self.insert_flush_refs("erc1155_transfers", &erc1155, &key),
            batch.modules.store(self, &key, window),
        );

        let (r0, r1, r2, r3, r4, r5, r6) = results;
        let failures: Vec<String> = [r0, r1, r2, r3, r4, r5, r6]
            .into_iter()
            .filter_map(|r| r.err())
            .map(|e| format!("{e:#}"))
            .collect();

        if !failures.is_empty() {
            bail!("failed to store batch: {}", failures.join("; "));
        }

        self.insert_flush_refs("blocks", &blocks, &key).await?;

        let checkpoints: Vec<DatabaseCheckpoint> =
            contiguous_ranges(blocks.iter().map(|b| b.number))
                .into_iter()
                .map(|range| DatabaseCheckpoint {
                    chain: self.chain_id,
                    from_block: range.from,
                    to_block: range.to,
                    epoch: batch.epoch(),
                    _version: key.version,
                })
                .collect();

        let checkpoints: Vec<&DatabaseCheckpoint> =
            checkpoints.iter().collect();

        self.insert_flush_refs("checkpoints", &checkpoints, &key).await
    }

    /// Inserts rows of a flush into a block scoped `table`: synchronous,
    /// with the flush's deduplication token, so a retry of an insert that
    /// was applied but not acknowledged is dropped by the server - in the
    /// table AND in everything its materialized views feed.
    pub async fn insert_flush<T>(
        &self,
        table: &'static str,
        rows: &[T],
        key: &FlushKey,
    ) -> Result<()>
    where
        T: Serialize,
        for<'a> T: Row<Value<'a> = T>,
    {
        let rows: Vec<&T> = rows.iter().collect();
        self.insert_flush_refs(table, &rows, key).await
    }

    /// [`Self::insert_flush`] for rows selected out of a larger batch (the
    /// parts of a flush that is split by month, `flush_windows`), without
    /// copying them.
    pub async fn insert_flush_refs<T>(
        &self,
        table: &'static str,
        rows: &[&T],
        key: &FlushKey,
    ) -> Result<()>
    where
        T: Serialize,
        for<'a> T: Row<Value<'a> = T>,
    {
        if rows.is_empty() {
            return Ok(());
        }

        let client = self
            .db
            .clone()
            .with_option("async_insert", "0")
            .with_option("insert_deduplicate", "1")
            .with_option("insert_deduplication_token", key.token(table))
            .with_option(
                "deduplicate_blocks_in_dependent_materialized_views",
                "1",
            );

        self.insert_retrying(&client, table, rows).await
    }

    /// Inserts `rows` into `table`, retrying with exponential backoff.
    /// For rows that are NOT part of a flush (`tokens`, resolver rows):
    /// server side batching, no deduplication token. The target must be
    /// idempotent (`ReplacingMergeTree`).
    pub async fn insert_rows<T>(
        &self,
        table: &'static str,
        rows: &[T],
    ) -> Result<()>
    where
        T: Serialize,
        for<'a> T: Row<Value<'a> = T>,
    {
        if rows.is_empty() {
            return Ok(());
        }

        let rows: Vec<&T> = rows.iter().collect();
        self.insert_retrying(&self.small, table, &rows).await
    }

    async fn insert_retrying<T>(
        &self,
        client: &Client,
        table: &'static str,
        rows: &[&T],
    ) -> Result<()>
    where
        T: Serialize,
        for<'a> T: Row<Value<'a> = T>,
    {
        let mut attempt = 0;

        loop {
            attempt += 1;

            match Self::insert_once(client, table, rows).await {
                Ok(()) => {
                    self.metrics.rows_inserted(table, rows.len() as u64);
                    return Ok(());
                }
                Err(e) if attempt >= INSERT_ATTEMPTS => {
                    return Err(e.context(format!(
                        "insert of {} rows into '{table}' failed after \
                         {INSERT_ATTEMPTS} attempts",
                        rows.len()
                    )));
                }
                Err(e) => {
                    self.metrics.flush_retry(table);
                    let wait = insert_backoff(attempt);
                    warn!(
                        "Insert of {} rows into '{table}' failed (attempt \
                         {attempt}/{INSERT_ATTEMPTS}): {e}. Retrying in \
                         {wait:?}.",
                        rows.len()
                    );
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    async fn insert_once<T>(
        client: &Client,
        table: &str,
        rows: &[&T],
    ) -> Result<()>
    where
        T: Serialize,
        for<'a> T: Row<Value<'a> = T>,
    {
        // Validation is OFF for inserts: the crate's schema validation
        // has no mapping for (U)Int256 and panics on those columns (see
        // `utils::format`). The rows go out as plain `RowBinary` with an
        // explicit column list taken from the struct, so the column ORDER
        // of the table does not matter and columns that are not part of
        // the struct get their DEFAULT. The integration tests are the
        // type check: they insert and read back every table.
        let client = client.clone().with_validation(false);

        // Timeouts surface as ordinary errors, so the caller retries them
        // like any other failed insert.
        let insert = tokio::time::timeout(
            INSERT_PREPARE_TIMEOUT,
            client.insert::<T>(table),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "timed out after {INSERT_PREPARE_TIMEOUT:?} preparing the \
                 insert"
            )
        })??;

        let mut insert = insert.with_timeouts(
            Some(INSERT_SEND_TIMEOUT),
            Some(INSERT_END_TIMEOUT),
        );

        for row in rows {
            insert.write(*row).await?;
        }

        insert.end().await?;

        Ok(())
    }
}

fn insert_backoff(attempt: u32) -> Duration {
    INSERT_BACKOFF_BASE
        .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)))
        .min(INSERT_BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_url_defaults_to_the_http_port() {
        let params =
            DatabaseParams::parse("http://default:secret@ch/indexer")
                .unwrap();

        assert_eq!(params.endpoint, "http://ch:8123");
        assert_eq!(params.user, "default");
        assert_eq!(params.password, "secret");
        assert_eq!(params.database, "indexer");
    }

    #[test]
    fn https_url_defaults_to_the_https_port() {
        let params =
            DatabaseParams::parse("https://u:p@ch.example.com/db")
                .unwrap();
        assert_eq!(params.endpoint, "https://ch.example.com:8443");
    }

    #[test]
    fn explicit_port_wins() {
        let params =
            DatabaseParams::parse("http://u:p@localhost:18123/db")
                .unwrap();
        assert_eq!(params.endpoint, "http://localhost:18123");
    }

    #[test]
    fn legacy_clickhouse_scheme_maps_to_http() {
        let params =
            DatabaseParams::parse("clickhouse://u:p@clickhouse/indexer")
                .unwrap();
        assert_eq!(params.endpoint, "http://clickhouse:8123");

        let params = DatabaseParams::parse(
            "clickhouse://u:p@clickhouse:8123/indexer",
        )
        .unwrap();
        assert_eq!(params.endpoint, "http://clickhouse:8123");
    }

    #[test]
    fn legacy_scheme_with_native_ports_is_rewritten_with_a_warning() {
        let params = DatabaseParams::parse(
            "clickhouse://u:hunter2@ch:9000/indexer",
        )
        .unwrap();
        assert_eq!(params.endpoint, "http://ch:8123");
        assert_eq!(params.warnings.len(), 1);
        assert!(params.warnings[0].contains("9000"));
        assert!(params.warnings[0].contains("8123"));
        assert!(!params.warnings[0].contains("hunter2"));

        let params = DatabaseParams::parse(
            "clickhouse://u:hunter2@ch:9440/indexer",
        )
        .unwrap();
        assert_eq!(params.endpoint, "https://ch:8443");
        assert_eq!(params.warnings.len(), 1);
        assert!(params.warnings[0].contains("9440"));

        // Any other explicit port is respected silently.
        let params =
            DatabaseParams::parse("clickhouse://u:p@ch:18123/indexer")
                .unwrap();
        assert_eq!(params.endpoint, "http://ch:18123");
        assert!(params.warnings.is_empty());
    }

    #[test]
    fn explicit_http_with_a_native_port_is_kept_but_warned_about() {
        let params =
            DatabaseParams::parse("http://u:hunter2@ch:9000/db").unwrap();
        assert_eq!(params.endpoint, "http://ch:9000");
        assert_eq!(params.warnings.len(), 1);
        assert!(!params.warnings[0].contains("hunter2"));

        let params =
            DatabaseParams::parse("https://u:p@ch:9440/db").unwrap();
        assert_eq!(params.endpoint, "https://ch:9440");
        assert_eq!(params.warnings.len(), 1);

        let params = DatabaseParams::parse("http://u:p@ch/db").unwrap();
        assert!(params.warnings.is_empty());
    }

    #[test]
    fn insert_end_timeout_outlasts_the_server_side_wait() {
        // Server default wait_for_async_insert_timeout.
        assert!(INSERT_END_TIMEOUT > Duration::from_secs(120));
        assert!(INSERT_SEND_TIMEOUT < INSERT_END_TIMEOUT);
    }

    #[test]
    fn password_is_optional() {
        let params =
            DatabaseParams::parse("http://default@ch/db").unwrap();
        assert_eq!(params.password, "");
    }

    #[test]
    fn bad_urls_are_errors_and_do_not_leak_the_password() {
        for url in [
            "not a url with hunter2",
            "tcp://u:hunter2@ch:9000/db",
            "http://u:hunter2@ch:8123",
            "http://u:hunter2@ch:8123/",
        ] {
            let error = DatabaseParams::parse(url).unwrap_err();
            assert!(!format!("{error:#}").contains("hunter2"), "{url}");
        }
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        assert_eq!(insert_backoff(1), Duration::from_secs(1));
        assert_eq!(insert_backoff(2), Duration::from_secs(2));
        assert_eq!(insert_backoff(3), Duration::from_secs(4));
        assert_eq!(insert_backoff(10), Duration::from_secs(30));
        assert_eq!(insert_backoff(u32::MAX), Duration::from_secs(30));
    }

    #[test]
    fn row_batch_append_moves_rows() {
        use crate::db::models::log::test_support::log_with;

        let mut a = RowBatch::default();
        let mut b = RowBatch::default();
        b.logs.push(log_with(&[], vec![]));
        b.modules = crate::pipeline::modules::test_support::dex_rows(1, 5);
        assert_eq!(b.modules.rows(), 1);

        assert!(a.is_empty());
        a.append(&mut b);
        assert_eq!(a.rows(), 2);
        assert!(b.is_empty());
        assert_eq!(a.block_span(), None);
    }

    /// 2015-08-01, 2015-09-01, ... : the first instant of `count` UTC
    /// months in a row.
    fn monthly(count: usize) -> Vec<u32> {
        let mut timestamps = vec![1_438_387_200u32];
        for _ in 1..count {
            let last = *timestamps.last().unwrap();
            timestamps.push(derived::next_month_start(last) as u32);
        }
        timestamps
    }

    #[test]
    fn an_ordinary_flush_is_one_part() {
        // Hours, days, even 90 months: one insert, exactly as before.
        for count in [1, 2, 89, 90] {
            assert_eq!(
                flush_windows(monthly(count).into_iter()),
                vec![FlushWindow::ALL],
                "{count} months"
            );
        }

        assert_eq!(
            flush_windows(std::iter::empty()),
            vec![FlushWindow::ALL]
        );
        assert!(FlushWindow::ALL.holds(0));
        assert!(FlushWindow::ALL.holds(u32::MAX));
    }

    #[test]
    fn a_flush_over_more_months_than_one_insert_may_touch_is_split() {
        // 135 months (a gap heal spread over eleven years, or a chain with
        // a very long block time): ClickHouse refuses one insert block
        // over `max_partitions_per_insert_block` = 100 with code 252.
        let timestamps = monthly(135);
        let windows = flush_windows(timestamps.iter().copied());

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].from, 0);
        assert_eq!(windows[1].to, u32::MAX);
        // Contiguous, so no row can fall between two parts ...
        assert_eq!(windows[0].to, windows[1].from);

        // ... and every one of them lands in exactly one part, whole
        // months at a time.
        for timestamp in &timestamps {
            let parts = windows
                .iter()
                .filter(|window| window.holds(*timestamp))
                .count();
            assert_eq!(parts, 1, "{timestamp}");
        }

        // The first part carries 90 months, the second the remaining 45.
        let count = |window: &FlushWindow| {
            timestamps.iter().filter(|t| window.holds(**t)).count()
        };
        assert_eq!(count(&windows[0]), MAX_MONTHS_PER_FLUSH);
        assert_eq!(count(&windows[1]), 135 - MAX_MONTHS_PER_FLUSH);

        // 10 years of hourly blocks in one flush is still 121 months.
        let windows = flush_windows(
            monthly(400)
                .into_iter()
                .flat_map(|month| [month, month + 3_600, month + 86_400]),
        );
        assert_eq!(windows.len(), 5);
        for window in &windows {
            assert!(window.from < window.to);
        }
    }

    #[test]
    fn flush_tokens_are_deterministic_and_unique_per_flush() {
        let key = FlushKey { chain: 137, span: (10, 19), version: 1_234 };

        // A retry of the same flush: the same token.
        assert_eq!(key.token("logs"), "logs:137:10-19:1234");
        assert_eq!(key.token("logs"), key.token("logs"));
        // Another table, chain, span or flush: another token.
        assert_ne!(key.token("logs"), key.token("blocks"));
        assert_ne!(
            key.token("logs"),
            FlushKey { chain: 1, ..key }.token("logs")
        );
        assert_ne!(
            key.token("logs"),
            FlushKey { span: (10, 20), ..key }.token("logs")
        );
        assert_ne!(
            key.token("logs"),
            FlushKey { version: 1_235, ..key }.token("logs")
        );
    }

    #[test]
    fn a_seed_from_the_future_is_never_undercut() {
        let ahead = next_version() + 86_400_000;

        seed_version(ahead);
        let next = next_version();
        assert!(next > ahead, "{next} <= {ahead}");
        assert!(next_version() > next);

        // A lower seed changes nothing.
        seed_version(1);
        assert!(next_version() > next);
    }

    #[test]
    fn versions_are_strictly_increasing_unix_milliseconds() {
        let first = next_version();
        let second = next_version();
        let third = next_version();

        assert!(second > first && third > second);
        // 2020-01-01 in ms: it is a wall clock, not a counter.
        assert!(first > 1_577_836_800_000);
    }

    #[test]
    fn set_version_stamps_every_block_scoped_row() {
        use crate::db::models::{
            block::test_support::block_row, log::test_support::log_with,
        };

        let mut batch = RowBatch::default();
        batch.blocks.push(block_row(5, 5, 4));
        batch.blocks.push(block_row(6, 6, 5));
        batch.logs.push(log_with(&[], vec![]));

        batch.modules =
            crate::pipeline::modules::test_support::dex_rows(1, 5);

        batch.set_version(1_234);
        batch.set_epoch(7);

        assert_eq!((batch.version(), batch.epoch()), (1_234, 7));
        assert!(!batch.modules.dex.liquidity.is_empty());
        assert!(batch
            .modules
            .dex
            .liquidity
            .iter()
            .all(|row| row._version == 1_234 && row.epoch == 7));

        assert!(batch.blocks.iter().all(|row| row._version == 1_234));
        assert!(batch.logs.iter().all(|row| row._version == 1_234));
        assert!(batch.blocks.iter().all(|row| row.epoch == 7));
        assert!(batch.logs.iter().all(|row| row.epoch == 7));
        assert_eq!(batch.block_span(), Some((5, 6)));
    }
}
