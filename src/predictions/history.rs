//! The registry-only history pass (docs/design.md section 16).
//!
//! # The problem it fixes
//!
//! Prediction markets are the one dataset in this indexer that is WRONG
//! without data from below the coverage floor, and wrong in two visible
//! ways:
//!
//! * a market whose `ConditionPreparation` happened before the floor has no
//!   question and no outcomes, so it shows up on a screen as a market with
//!   no name;
//! * open interest is split minus merged minus redeemed collateral, so a
//!   market that was split into before the floor and redeemed after it
//!   reads as a NEGATIVE number.
//!
//! Neither is a decoder bug. Both are what "we started indexing in the
//! middle" looks like, and `src/predictions/README.md` has always said so.
//!
//! # What the pass does, and what it deliberately does not
//!
//! Once per chain, in the background, it walks the blocks BELOW the floor
//! and asks the source for the logs of the operator's own trusted
//! addresses - the registry, the exchanges, the NegRisk adapters, from
//! `prediction_trusted` - and of nothing else. A handful of addresses, a
//! log filter, one pass.
//!
//! It stores market metadata, questions, outcome-token mappings and the
//! split / merge / redeem / convert / resolution events open interest is
//! made of.
//!
//! It stores **no trade below the floor**, on purpose, and that is the one
//! thing to understand about this file. A trade is volume. Storing the
//! trades of a few trusted exchanges below the floor - while every other
//! dataset on the same chain starts at the floor - would make volume,
//! candles and leaderboards mean one thing below a date and another thing
//! above it, and nothing on a screen would say which. The window stays one
//! window; only the facts a market needs to be describable come from
//! underneath it.
//!
//! # Resumable and idempotent
//!
//! The pass always walks upwards, so one high-water mark is the whole of
//! its state (`prediction_history`, migration 0023). A crash, a restart or
//! a second run picks up from the last finished chunk. Re-running a chunk
//! that was already written is harmless: every insert carries the same
//! deterministic dedup token the live pipeline uses, and every row is
//! keyed the same way, so a repeat is a no-op rather than a double count.

use crate::{
    core::models::log::DatabaseLog,
    db::{flush_windows, next_version, ranges::BlockRange, Database},
    pipeline::{
        lease::Fence,
        modules::{EnabledModules, ModuleRows},
    },
    predictions::{self, RegistrySet},
};
use alloy::primitives::Address;
use anyhow::{Context, Result};
use futures::future::BoxFuture;
use log::{debug, info, warn};

/// Blocks asked for at a time. Large, because the answer is a log filter
/// over a handful of addresses: most chunks come back empty and the source
/// tells us how far it actually got.
pub const CHUNK_BLOCKS: u64 = 500_000;

/// A source that can answer "the logs of these addresses in this range".
///
/// The seam exists so the tests can drive the whole pass - the cursor, the
/// decoding, the dropped trades, the stored rows - against an in-memory
/// chain, and so nothing here has to know about HyperSync.
pub trait LogSource: Send + Sync {
    /// Logs of `addresses` in `range`, plus the blocks they are in.
    ///
    /// The returned `next_block` is the first block NOT covered by this
    /// answer. A source that skips ahead over blocks where none of the
    /// addresses existed yet reports that here, which is what makes
    /// starting at block 0 cost the same as starting at a deployment block
    /// nobody had to write down.
    fn logs(
        &self,
        chain: u64,
        range: BlockRange,
        addresses: &[Address],
    ) -> BoxFuture<'_, Result<LogPage>>;
}

/// One answer from a [`LogSource`], in the row type the decoder reads.
///
/// Stored rows rather than the source's own wire types on purpose: it keeps
/// HyperSync out of this file, and it is what lets the tests serve the
/// module's real recorded transactions through the same seam.
#[derive(Debug, Default)]
pub struct LogPage {
    pub logs: Vec<DatabaseLog>,
    /// First block NOT covered by `logs`.
    pub next_block: u64,
}

/// What one run of the pass did, in the words a log line uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HistoryReport {
    /// Trusted addresses the filter used. 0 means the operator has told
    /// this database nothing, and the pass did not run.
    pub addresses: usize,
    /// Blocks the pass covered this time.
    pub scanned: u64,
    /// Where it got to (exclusive). Equal to the floor when it finished.
    pub done_to_block: u64,
    pub floor_block: u64,
    pub markets: usize,
    pub questions: usize,
    pub position_events: usize,
    /// Trades found below the floor and deliberately NOT stored.
    pub trades_dropped: usize,
}

impl HistoryReport {
    pub fn finished(&self) -> bool {
        self.done_to_block >= self.floor_block
    }
}

impl std::fmt::Display for HistoryReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.addresses == 0 {
            return write!(
                f,
                "Prediction market history: nothing to do. This database \
                 has no trusted addresses yet (see prediction_trusted in \
                 src/predictions/README.md), so there is nothing to ask \
                 the source for."
            );
        }

        write!(
            f,
            "Prediction market history: {} block(s) below the coverage \
             floor read from {} trusted address(es), up to block {} of {}. \
             Stored {} market(s), {} question(s), {} split/merge/redeem \
             event(s); {} trade(s) below the floor were deliberately not \
             stored. {}",
            self.scanned,
            self.addresses,
            self.done_to_block,
            self.floor_block,
            self.markets,
            self.questions,
            self.position_events,
            self.trades_dropped,
            if self.finished() { "Finished." } else { "Not finished yet." }
        )
    }
}

/// The trusted addresses of this chain, and the lowest block worth asking
/// about.
///
/// Every `kind` in the table counts - `registry`, `exchange`, `adapter`,
/// and anything an operator adds later - because the question here is "does
/// this contract emit events a market's description is made of", and the
/// answer for every trusted contract is yes.
pub async fn trusted(db: &Database) -> Result<(Vec<Address>, u64)> {
    #[serde_with::serde_as]
    #[derive(clickhouse::Row, serde::Deserialize)]
    struct Row {
        #[serde_as(as = "crate::db::format::SerId32")]
        address: Address,
        from_block: u64,
    }

    let sql = format!(
        "SELECT address, from_block FROM prediction_trusted FINAL \
         WHERE chain = {} AND is_deleted = 0 ORDER BY address",
        db.chain_id
    );

    let rows = db
        .db
        .query(&sql)
        .fetch_all::<Row>()
        .await
        .context("read the trusted prediction market addresses")?;

    let from = rows.iter().map(|row| row.from_block).min().unwrap_or(0);
    let addresses = rows.into_iter().map(|row| row.address).collect();

    Ok((addresses, from))
}

/// How far a previous run got (exclusive).
pub async fn cursor(db: &Database) -> Result<u64> {
    let sql = format!(
        "SELECT done_to_block FROM prediction_history_v WHERE chain = {}",
        db.chain_id
    );

    let rows = db
        .db
        .query(&sql)
        .fetch_all::<u64>()
        .await
        .context("read how far the prediction history pass has got")?;

    Ok(rows.into_iter().next().unwrap_or(0))
}

/// Records how far the pass has got. `_version` is the block itself, so the
/// furthest run wins whatever order two inserts land in.
async fn save_cursor(
    db: &Database,
    fence: &Fence,
    done_to_block: u64,
    floor_block: u64,
) -> Result<()> {
    fence.check()?;

    let sql = format!(
        "INSERT INTO prediction_history \
         (chain, done_to_block, floor_block, _version) \
         SELECT {}, {done_to_block}, {floor_block}, {done_to_block}",
        db.chain_id
    );

    db.db.query(&sql).execute().await.with_context(|| {
        format!(
            "record the prediction history cursor of chain {}",
            db.chain_id
        )
    })
}

/// Runs the pass over `[cursor, floor_block)`.
///
/// Returns as soon as the floor is reached, and is safe to call again at
/// any time: a finished pass does no work and a half-finished one carries
/// on.
pub async fn run(
    db: &Database,
    fence: &Fence,
    source: &dyn LogSource,
    floor_block: u64,
    chunk_blocks: u64,
) -> Result<HistoryReport> {
    let (addresses, lowest) = trusted(db).await?;

    let mut report = HistoryReport {
        addresses: addresses.len(),
        floor_block,
        ..HistoryReport::default()
    };

    if addresses.is_empty() {
        return Ok(report);
    }

    let stored = cursor(db).await?;
    let mut at = stored.max(lowest);
    report.done_to_block = at;

    if at >= floor_block {
        return Ok(report);
    }

    info!(
        "Chain {}: reading prediction market history from {} trusted \
         address(es), blocks [{at}, {floor_block}). Market descriptions \
         and open interest need it; no trade below the floor is stored.",
        db.chain_id,
        addresses.len()
    );

    // Seeded with the trusted addresses: they are registries by definition,
    // which is what lets the first batch's ERC-1155 transfers be kept
    // instead of dropped as unrelated traffic.
    let mut registries = RegistrySet::new(addresses.iter().copied());

    let chunk_blocks = chunk_blocks.max(1);

    while at < floor_block {
        fence.check()?;

        let to = at.saturating_add(chunk_blocks).min(floor_block);
        let range = BlockRange::new(at, to);

        let page = source
            .logs(db.chain_id, range, &addresses)
            .await
            .with_context(|| format!("read the logs of {range}"))?;

        let mut rows = decode_page(db.chain_id, &page, &mut registries);

        report.trades_dropped += rows.trades.len();
        // The one deliberate omission: see the module doc.
        rows.trades.clear();

        report.markets += rows.markets.len();
        report.questions += rows.questions.len();
        report.position_events += rows.position_events.len();

        if !rows.is_empty() {
            store(db, fence, rows, range).await?;
        }

        // The source decides how far the answer really went: an address
        // that did not exist yet lets it skip whole ranges.
        let reached = page.next_block.max(to).min(floor_block);
        report.scanned += reached.saturating_sub(at);
        at = reached;

        save_cursor(db, fence, at, floor_block).await?;
        report.done_to_block = at;

        debug!(
            "Chain {}: prediction history reached block {at} of \
             {floor_block}.",
            db.chain_id
        );
    }

    info!("{report}");

    Ok(report)
}

/// One page of logs, decoded the way the live pipeline decodes them.
fn decode_page(
    chain: u64,
    page: &LogPage,
    registries: &mut RegistrySet,
) -> predictions::PredictionRows {
    let mut rows = predictions::decode(chain, &page.logs);

    // `tx_from` / `tx_to` are only ever read off a TRADE, and this pass
    // stores none, so the transactions are not fetched at all.
    registries.observe(&rows);
    rows.retain_transfers(|registry| registries.contains(registry));

    rows
}

/// Writes one chunk, through the same insert path, the same dedup tokens
/// and the same monthly windowing as a live flush.
async fn store(
    db: &Database,
    fence: &Fence,
    mut rows: predictions::PredictionRows,
    range: BlockRange,
) -> Result<()> {
    fence.check()?;

    rows.set_epoch(db.epoch());
    rows.set_version(next_version());

    let timestamps: Vec<u32> = rows
        .markets
        .iter()
        .map(|row| row.timestamp)
        .chain(rows.position_events.iter().map(|row| row.timestamp))
        .chain(rows.questions.iter().map(|row| row.timestamp))
        .chain(rows.resolutions.iter().map(|row| row.timestamp))
        .chain(rows.transfers.iter().map(|row| row.timestamp))
        .collect();

    let key = crate::db::FlushKey {
        chain: db.chain_id,
        span: (range.from, range.to.saturating_sub(1)),
        version: rows
            .markets
            .first()
            .map(|row| row._version)
            .unwrap_or_else(next_version),
    };

    let module_rows =
        ModuleRows { predictions: rows, ..ModuleRows::default() };

    for window in flush_windows(timestamps.into_iter()) {
        module_rows.store(db, &key, window).await.with_context(|| {
            format!("store the history rows of {range}")
        })?;
    }

    Ok(())
}

/// Starts the pass in the background, once, when it has anything to do.
///
/// Background because it must never delay indexing: the live window is the
/// product, and a market's description arriving a minute later is not worth
/// a minute of lag. Errors are logged and never fatal - the pass is an
/// improvement to old rows, not a precondition for new ones - and the next
/// start tries again from wherever it got to.
pub fn spawn(
    db: Database,
    fence: Fence,
    source: std::sync::Arc<dyn LogSource>,
    enabled: EnabledModules,
    floor_block: u64,
) {
    if !enabled.predictions || floor_block == 0 {
        return;
    }

    tokio::spawn(async move {
        match run(&db, &fence, source.as_ref(), floor_block, CHUNK_BLOCKS)
            .await
        {
            Ok(report) if report.addresses == 0 => {
                debug!("{report}");
            }
            Ok(_) => {}
            Err(e) => warn!(
                "Chain {}: the prediction market history pass stopped \
                 ({e:#}). Markets created below the coverage floor may \
                 still have no question and a negative open interest. It \
                 starts again from where it got to on the next start, or \
                 run `indexer backfill --module predictions \
                 --registry-only`.",
                db.chain_id
            ),
        }
    });
}
