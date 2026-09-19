//! In-memory reference model used by the tests: a fake chain that can
//! fork, a fake ClickHouse that implements the storage rules of
//! docs/design.md (row versions, tombstones, `FINAL` per partition,
//! materialized views feeding epoch-keyed aggregates, the VALIDITY RULE),
//! a fake writer, and a tiny sync loop ([`Node`]) that drives the real
//! [`ReorgGuard`] the way the pipeline will.
//!
//! The store can be told to fail at any [`PurgeStep`], optionally after
//! doing only PART of the step (an `INSERT .. SELECT` is not atomic across
//! partitions), and the writer can die between the children and the
//! `blocks` rows of a flush.

use super::{
    BlockHeader, CanonicalChain, DiscoveryCache, PurgeOptions, PurgeStep,
    Purger, ReorgConfig, ReorgError, ReorgGuard, ReorgMetrics,
    ReorgRecord, ReorgStore, StreamGuard, Verdict, WriterControl,
};
use crate::db::next_version;
use alloy::primitives::B256;
use anyhow::{anyhow, bail};
use futures::{future::BoxFuture, FutureExt};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

/// xorshift64*: deterministic, seedable, no dependency.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[0, n)`; 0 when `n` is 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }

    /// Uniform in `[lo, hi]`.
    pub fn between(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }

    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

// ---------------------------------------------------------------- chain

/// Child tables of the model ("transactions" and "logs").
pub const CHILD_TABLES: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeBlock {
    pub header: BlockHeader,
    /// Values of the child rows, per child table; the index is the key.
    pub children: [Vec<u64>; CHILD_TABLES],
}

struct ChainState {
    blocks: Vec<FakeBlock>,
    rng: Rng,
    salt: u64,
    next_id: u64,
    max_block_time: u32,
    /// Calls served (headers + fetches).
    calls: u64,
    /// `(after this many calls, depth)`: the chain reorganizes by itself
    /// while somebody is reading it.
    scripted: Vec<(u64, u64)>,
    fail_next_headers: u32,
}

impl ChainState {
    fn push_block(&mut self) {
        let number = self.blocks.len() as u64;
        let (parent_hash, parent_ts) = match self.blocks.last() {
            Some(parent) => (parent.header.hash, parent.header.timestamp),
            None => (B256::ZERO, 1_700_000_000),
        };

        self.next_id += 1;
        let mut hash = [0u8; 32];
        hash[..8].copy_from_slice(&self.salt.to_be_bytes());
        hash[8..16].copy_from_slice(&self.next_id.to_be_bytes());
        hash[31] = 1;

        let timestamp = parent_ts
            + 1
            + self.rng.below(u64::from(self.max_block_time)) as u32;

        let mut children: [Vec<u64>; CHILD_TABLES] = Default::default();
        for table in children.iter_mut() {
            for _ in 0..self.rng.below(4) {
                table.push(1 + self.rng.below(1_000));
            }
        }

        self.blocks.push(FakeBlock {
            header: BlockHeader {
                number,
                hash: B256::from(hash),
                parent_hash,
                timestamp,
            },
            children,
        });
    }

    fn reorg(&mut self, depth: u64, new_len: u64) {
        // Genesis never reorganizes.
        let depth = depth.min(self.blocks.len() as u64 - 1);
        self.blocks.truncate(self.blocks.len() - depth as usize);
        for _ in 0..new_len {
            self.push_block();
        }
    }

    fn served(&mut self) {
        self.calls += 1;
        let calls = self.calls;
        let due: Vec<u64> = self
            .scripted
            .iter()
            .filter(|(at, _)| *at <= calls)
            .map(|(_, depth)| *depth)
            .collect();
        self.scripted.retain(|(at, _)| *at > calls);
        for depth in due {
            self.reorg(depth, depth + 1);
        }
    }
}

/// A chain that can be extended and reorganized.
pub struct FakeChain {
    state: Mutex<ChainState>,
}

impl FakeChain {
    /// A chain holding only block 0. `max_block_time` (seconds) decides how
    /// many blocks share a day.
    pub fn new(seed: u64, max_block_time: u32) -> Arc<Self> {
        let mut state = ChainState {
            blocks: Vec::new(),
            rng: Rng::new(seed),
            salt: seed,
            next_id: 0,
            max_block_time: max_block_time.max(1),
            calls: 0,
            scripted: Vec::new(),
            fail_next_headers: 0,
        };
        state.push_block();
        Arc::new(Self { state: Mutex::new(state) })
    }

    pub fn extend(&self, blocks: u64) {
        let mut state = self.state.lock().unwrap();
        for _ in 0..blocks {
            state.push_block();
        }
    }

    /// Replaces the newest `depth` blocks with `new_len` different ones.
    pub fn reorg(&self, depth: u64, new_len: u64) {
        self.state.lock().unwrap().reorg(depth, new_len);
    }

    /// The chain reorganizes `depth` deep after serving `calls` more
    /// requests.
    pub fn reorg_after_calls(&self, calls: u64, depth: u64) {
        let mut state = self.state.lock().unwrap();
        let at = state.calls + calls;
        state.scripted.push((at, depth));
    }

    pub fn clear_script(&self) {
        self.state.lock().unwrap().scripted.clear();
    }

    pub fn fail_next_headers(&self, times: u32) {
        self.state.lock().unwrap().fail_next_headers = times;
    }

    pub fn calls(&self) -> u64 {
        self.state.lock().unwrap().calls
    }

    /// Exclusive.
    pub fn head(&self) -> u64 {
        self.state.lock().unwrap().blocks.len() as u64
    }

    pub fn block(&self, number: u64) -> Option<FakeBlock> {
        self.state.lock().unwrap().blocks.get(number as usize).cloned()
    }

    /// What a stream response would carry: the blocks of `[from, to)` that
    /// exist right now.
    pub fn fetch(&self, from: u64, to: u64) -> Vec<FakeBlock> {
        let mut state = self.state.lock().unwrap();
        let blocks = state
            .blocks
            .iter()
            .skip(from as usize)
            .take(to.saturating_sub(from) as usize)
            .cloned()
            .collect();
        state.served();
        blocks
    }
}

impl CanonicalChain for FakeChain {
    fn headers(
        &self,
        from: u64,
        to: u64,
    ) -> BoxFuture<'_, anyhow::Result<Vec<BlockHeader>>> {
        async move {
            {
                let mut state = self.state.lock().unwrap();
                if state.fail_next_headers > 0 {
                    state.fail_next_headers -= 1;
                    bail!("injected: header source unavailable");
                }
            }
            Ok(self.fetch(from, to).iter().map(|b| b.header).collect())
        }
        .boxed()
    }
}

// ---------------------------------------------------------------- store

/// Base tables are partitioned by month and `FINAL` does not merge across
/// partitions: two versions of a key only replace each other inside one
/// partition.
pub const PARTITION_SECONDS: u32 = 30 * 86_400;

/// The `_version` clock of one process: `db::next_version()` with a wall
/// clock that can be set BACK (NTP step, VM snapshot, skewed container
/// host) and the seed a process reads from the database at startup.
#[derive(Debug)]
pub struct ModelClock {
    /// How far behind the real clock this process' wall clock is.
    behind: u64,
    last: std::sync::atomic::AtomicU64,
}

impl ModelClock {
    pub fn new(behind: u64, seed: u64) -> Arc<Self> {
        Arc::new(Self {
            behind,
            last: std::sync::atomic::AtomicU64::new(seed),
        })
    }

    /// Strictly increasing within the process, never below the seed.
    pub fn next(&self) -> u64 {
        use std::sync::atomic::Ordering::SeqCst;

        let wall = next_version().saturating_sub(self.behind);
        let mut last = self.last.load(SeqCst);
        loop {
            let next = wall.max(last + 1);
            match self.last.compare_exchange(last, next, SeqCst, SeqCst) {
                Ok(_) => return next,
                Err(current) => last = current,
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row<T> {
    pub version: u64,
    pub deleted: bool,
    pub epoch: u32,
    pub timestamp: u32,
    pub data: T,
}

/// `SELECT .. FINAL`: per partition the newest version, unless it is a
/// tombstone. With EQUAL versions ClickHouse keeps the row inserted LAST
/// (measured on 25.12: tombstone last -> gone, canonical last -> alive,
/// also after `OPTIMIZE FINAL`); `versions` is in insertion order.
pub fn live<T>(versions: &[Row<T>]) -> Vec<&Row<T>> {
    let mut newest: BTreeMap<u32, &Row<T>> = BTreeMap::new();
    for row in versions {
        let partition = row.timestamp / PARTITION_SECONDS;
        match newest.get(&partition) {
            Some(current) if current.version > row.version => {}
            _ => {
                newest.insert(partition, row);
            }
        }
    }
    newest.into_values().filter(|row| !row.deleted).collect()
}

/// The aggregates of the model: one sourced from `blocks`, two from child
/// tables, with different bucket widths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Agg {
    BlocksDaily,
    Child0Daily,
    Child1Hourly,
}

impl Agg {
    fn bucket(&self, timestamp: u32) -> u32 {
        let width = match self {
            Agg::BlocksDaily | Agg::Child0Daily => 86_400,
            Agg::Child1Hourly => 3_600,
        };
        timestamp - timestamp % width
    }

    fn child(table: usize) -> Agg {
        if table == 0 {
            Agg::Child0Daily
        } else {
            Agg::Child1Hourly
        }
    }
}

/// `(count, sum)` per `(aggregate, bucket)`.
pub type AggView = BTreeMap<(Agg, u32), (u64, u64)>;

/// Every stored version of the rows of one child table, by `(block, position)`.
pub type ChildRows = BTreeMap<(u64, u32), Vec<Row<u64>>>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChainData {
    pub blocks: BTreeMap<u64, Vec<Row<B256>>>,
    pub children: [ChildRows; CHILD_TABLES],
    /// READ-PATH SIDE TABLES: one mirror per child table, plus one fed
    /// from `blocks` (the model's `block_lookup`). Written ONLY by the
    /// materialized view of their base table - which passes `_version`
    /// and `is_deleted` through, so a tombstone in the base table
    /// normally kills the mirror row for free. A push that gets LOST
    /// leaves an orphan nothing else can ever remove, which is what
    /// [`ReorgStore::tombstone_side_rows`] repairs.
    pub side_children: [ChildRows; CHILD_TABLES],
    pub side_blocks: BTreeMap<u64, Vec<Row<B256>>>,
    pub checkpoints: BTreeMap<(u64, u64), Vec<Row<()>>>,
    pub reorgs: Vec<ReorgRecord>,
    /// `(aggregate, bucket, epoch)` -> `(count, sum)`.
    pub aggs: BTreeMap<(Agg, u32, u32), (u64, u64)>,
}

impl ChainData {
    /// `max(_version)` over every block scoped table and the checkpoints:
    /// what a starting process seeds its version clock with.
    pub fn max_version(&self) -> u64 {
        let blocks = self.blocks.values().flatten().map(|r| r.version);
        let children = self
            .children
            .iter()
            .flat_map(|table| table.values().flatten())
            .map(|r| r.version);
        let checkpoints =
            self.checkpoints.values().flatten().map(|r| r.version);

        blocks.chain(children).chain(checkpoints).max().unwrap_or(0)
    }

    fn add(&mut self, agg: Agg, ts: u32, epoch: u32, value: u64) {
        let entry =
            self.aggs.entry((agg, agg.bucket(ts), epoch)).or_default();
        entry.0 += 1;
        entry.1 += value;
    }

    /// The `*_v` view: the VALIDITY RULE of docs/design.md.
    pub fn aggregates(&self) -> AggView {
        let mut view = AggView::new();
        for ((agg, bucket, epoch), (count, sum)) in &self.aggs {
            let floor = self
                .reorgs
                .iter()
                .filter(|r| r.from_ts <= *bucket)
                .map(|r| r.epoch)
                .max()
                .unwrap_or(0);
            if *epoch >= floor {
                let entry = view.entry((*agg, *bucket)).or_default();
                entry.0 += count;
                entry.1 += sum;
            }
        }
        view.retain(|_, (count, _)| *count > 0);
        view
    }

    pub fn live_blocks(&self) -> BTreeMap<u64, Vec<B256>> {
        self.blocks
            .iter()
            .map(|(n, v)| (*n, live(v).iter().map(|r| r.data).collect()))
            .filter(|(_, hashes): &(u64, Vec<B256>)| !hashes.is_empty())
            .collect()
    }

    pub fn live_children(
        &self,
        table: usize,
    ) -> BTreeMap<(u64, u32), Vec<u64>> {
        self.children[table]
            .iter()
            .map(|(k, v)| (*k, live(v).iter().map(|r| r.data).collect()))
            .filter(|(_, values): &((u64, u32), Vec<u64>)| {
                !values.is_empty()
            })
            .collect()
    }

    pub fn live_side_children(
        &self,
        table: usize,
    ) -> BTreeMap<(u64, u32), Vec<u64>> {
        self.side_children[table]
            .iter()
            .map(|(k, v)| (*k, live(v).iter().map(|r| r.data).collect()))
            .filter(|(_, values): &((u64, u32), Vec<u64>)| {
                !values.is_empty()
            })
            .collect()
    }

    pub fn live_side_blocks(&self) -> BTreeMap<u64, Vec<B256>> {
        self.side_blocks
            .iter()
            .map(|(n, v)| (*n, live(v).iter().map(|r| r.data).collect()))
            .filter(|(_, hashes): &(u64, Vec<B256>)| !hashes.is_empty())
            .collect()
    }

    pub fn live_checkpoints(&self) -> Vec<(u64, u64)> {
        self.checkpoints
            .iter()
            .filter(|(_, v)| !live(v).is_empty())
            .map(|(k, _)| *k)
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    pub step: PurgeStep,
    /// Do part of the step's work before failing.
    pub partial: bool,
}

/// How many write operations a lagging read can be behind.
const MAX_LAG: usize = 3;

#[derive(Default)]
struct StoreState {
    chains: HashMap<u64, ChainData>,
    faults: HashMap<u64, Fault>,
    /// Steps executed per chain, for order assertions.
    journal: HashMap<u64, Vec<PurgeStep>>,
    /// NO READ-YOUR-WRITES: when set, a read may be served from the state
    /// of up to [`MAX_LAG`] write operations ago (never two reads in a
    /// row, "it heals on the next try").
    lag: Option<Rng>,
    /// The next `current_epoch` read misses the newest `reorgs` row.
    stale_epoch_once: bool,
    /// The next tombstone of a base table does NOT reach its side table:
    /// the base part landed and the materialized view push did not.
    lose_side_push: u32,
    /// `live_children` never reaches 0 (somebody else keeps writing).
    children_never_die: bool,
    /// The next children tombstone statement misses the upper half of the
    /// range / the next `min_timestamp` misses the lowest block: rows that
    /// were flushed a moment ago and can not be read yet.
    miss_children_once: bool,
    miss_lowest_timestamp_once: bool,
    lagged_last: bool,
    lagged_reads: u64,
    /// State of each chain before its most recent writes, oldest first.
    history: HashMap<u64, VecDeque<ChainData>>,
}

impl StoreState {
    /// What a query sees: usually the present, sometimes the recent past.
    fn view(&mut self, chain: u64) -> ChainData {
        let current = self.chains.get(&chain).cloned().unwrap_or_default();

        let Some(rng) = self.lag.as_mut() else {
            return current;
        };

        if self.lagged_last || !rng.chance(35) {
            self.lagged_last = false;
            return current;
        }

        let history = self.history.entry(chain).or_default();
        if history.is_empty() {
            return current;
        }

        let back =
            1 + rng.below(history.len().min(MAX_LAG) as u64) as usize;
        self.lagged_last = true;
        self.lagged_reads += 1;
        history[history.len() - back].clone()
    }

    /// Call before every write.
    fn write(&mut self, chain: u64) -> &mut ChainData {
        if self.lag.is_some() {
            let before =
                self.chains.get(&chain).cloned().unwrap_or_default();
            let history = self.history.entry(chain).or_default();
            history.push_back(before);
            while history.len() > MAX_LAG {
                history.pop_front();
            }
        }
        self.chains.entry(chain).or_default()
    }
}

/// One "database" shared by every chain.
#[derive(Default)]
pub struct FakeStore {
    state: Mutex<StoreState>,
    /// NEGATIVE CONTROLS, to show the subtleties matter: compute `from_ts`
    /// over live rows only / look for live orphans only / trust the
    /// materialized views instead of verifying the side tables (what the
    /// purge did before this step existed).
    min_ts_live_only: bool,
    orphans_live_only: bool,
    trust_the_views: bool,
}

fn in_range(number: u64, from: u64, to: Option<u64>) -> bool {
    number >= from && to.is_none_or(|to| number < to)
}

impl FakeStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A store without read-your-writes (see [`StoreState::lag`]).
    pub fn with_lag(seed: u64) -> Arc<Self> {
        let store = Self::default();
        store.state.lock().unwrap().lag = Some(Rng::new(seed));
        Arc::new(store)
    }

    pub fn stale_epoch_once(&self) {
        self.state.lock().unwrap().stale_epoch_once = true;
    }

    pub fn miss_children_once(&self) {
        self.state.lock().unwrap().miss_children_once = true;
    }

    pub fn miss_lowest_timestamp_once(&self) {
        self.state.lock().unwrap().miss_lowest_timestamp_once = true;
    }

    pub fn children_never_die(&self) {
        self.state.lock().unwrap().children_never_die = true;
    }

    /// The next `times` tombstone statements land in their base table
    /// WITHOUT reaching the side tables.
    pub fn lose_side_push(&self, times: u32) {
        self.state.lock().unwrap().lose_side_push = times;
    }

    pub fn lagged_reads(&self) -> u64 {
        self.state.lock().unwrap().lagged_reads
    }

    /// Time passes (a process restarts): every write is readable.
    pub fn settle_visibility(&self, chain: u64) {
        self.state.lock().unwrap().history.remove(&chain);
    }

    /// NEGATIVE CONTROL: `from_ts` over live rows only.
    pub fn with_min_ts_live_only() -> Arc<Self> {
        Arc::new(Self { min_ts_live_only: true, ..Self::default() })
    }

    /// NEGATIVE CONTROL: only live rows count as orphans.
    pub fn with_orphans_live_only() -> Arc<Self> {
        Arc::new(Self { orphans_live_only: true, ..Self::default() })
    }

    /// NEGATIVE CONTROL: the purge trusts the materialized views and never
    /// looks at the side tables (`live_side_rows` says 0,
    /// `tombstone_side_rows` writes nothing).
    pub fn with_trusted_views() -> Arc<Self> {
        Arc::new(Self { trust_the_views: true, ..Self::default() })
    }

    /// The next time `chain` reaches `step`, fail (once).
    pub fn fail_at(&self, chain: u64, step: PurgeStep, partial: bool) {
        let mut state = self.state.lock().unwrap();
        state.faults.insert(chain, Fault { step, partial });
    }

    pub fn clear_faults(&self) {
        self.state.lock().unwrap().faults.clear();
    }

    pub fn fault_pending(&self, chain: u64) -> bool {
        self.state.lock().unwrap().faults.contains_key(&chain)
    }

    pub fn snapshot(&self, chain: u64) -> ChainData {
        let state = self.state.lock().unwrap();
        state.chains.get(&chain).cloned().unwrap_or_default()
    }

    pub fn take_journal(&self, chain: u64) -> Vec<PurgeStep> {
        let mut state = self.state.lock().unwrap();
        state.journal.remove(&chain).unwrap_or_default()
    }

    /// Records the step; `Err` when a fault is due. `Ok(true)` = the fault
    /// is due but part of the work has to be done first.
    fn enter(
        state: &mut StoreState,
        chain: u64,
        step: PurgeStep,
    ) -> anyhow::Result<bool> {
        let writes = matches!(
            step,
            PurgeStep::TombstoneCheckpoints
                | PurgeStep::TombstoneChildren
                | PurgeStep::InsertReorg
                | PurgeStep::RebuildDerived
                | PurgeStep::TombstoneBlocks
                | PurgeStep::TombstoneSideTables
        );
        state.journal.entry(chain).or_default().push(step);
        match state.faults.get(&chain).copied() {
            Some(fault) if fault.step == step => {
                state.faults.remove(&chain);
                if fault.partial && writes {
                    Ok(true)
                } else {
                    Err(injected(step))
                }
            }
            _ => Ok(false),
        }
    }

    /// Test setup: makes `[from, to)` look as if it had never been stored
    /// (rows of every table vanish without a trace; aggregates are NOT
    /// touched, so only use it where they do not matter).
    pub fn forget(&self, chain: u64, from: u64, to: u64) {
        let mut state = self.state.lock().unwrap();
        let data = state.write(chain);
        data.blocks.retain(|number, _| !in_range(*number, from, Some(to)));
        data.side_blocks
            .retain(|number, _| !in_range(*number, from, Some(to)));
        for table in
            data.children.iter_mut().chain(data.side_children.iter_mut())
        {
            table.retain(|(number, _), _| {
                !in_range(*number, from, Some(to))
            });
        }
        data.checkpoints.clear();
    }

    /// `tombstone_children`, restricted to some child tables.
    pub fn tombstone_children_of(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
        tables: &[usize],
    ) -> anyhow::Result<u64> {
        let mut state = self.state.lock().unwrap();
        let step = PurgeStep::TombstoneChildren;
        let partial = Self::enter(&mut state, chain, step)?;
        let missing = std::mem::take(&mut state.miss_children_once);
        let lose_push = state.lose_side_push > 0;
        state.lose_side_push = state.lose_side_push.saturating_sub(1);
        let view = state.view(chain);
        let data = state.write(chain);

        // Partial: only the lower half of the affected blocks.
        let affected: BTreeSet<u64> = view
            .children
            .iter()
            .flat_map(|table| table.keys().map(|(number, _)| *number))
            .filter(|number| in_range(*number, from, to))
            .collect();
        let middle =
            affected.iter().nth(affected.len() / 2).copied().unwrap_or(0);

        // The materialized view of a base table sees the tombstone insert
        // and pushes it on - unless this push is the one that gets lost.
        let pushes = if lose_push { None } else { Some(()) };

        let mut count = 0;
        for (index, table) in view.children.iter().enumerate() {
            if !tables.contains(&index) {
                continue;
            }
            for (key, versions) in table {
                if !in_range(key.0, from, to) {
                    continue;
                }
                if (partial || missing) && key.0 >= middle {
                    continue;
                }
                let dead = dead_copies(versions, version);
                count += dead.len() as u64;
                if pushes.is_some() {
                    data.side_children[index]
                        .entry(*key)
                        .or_default()
                        .extend(dead.clone());
                }
                data.children[index].entry(*key).or_default().extend(dead);
            }
        }

        if partial {
            return Err(injected(step));
        }
        Ok(count)
    }

    /// `tombstone_side_rows`, restricted to some child mirrors (a module
    /// scoped store only owns its own read path).
    pub fn tombstone_side_rows_of(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
        tables: &[usize],
    ) -> BoxFuture<'_, anyhow::Result<u64>> {
        let tables = tables.to_vec();
        async move {
            let mut state = self.state.lock().unwrap();
            let step = PurgeStep::TombstoneSideTables;
            Self::enter(&mut state, chain, step)?;
            let view = state.view(chain);
            let data = state.write(chain);
            let mut count = 0;

            for index in tables {
                for (key, versions) in &view.side_children[index] {
                    if !in_range(key.0, from, to) {
                        continue;
                    }
                    let dead = dead_copies(versions, version);
                    count += dead.len() as u64;
                    data.side_children[index]
                        .entry(*key)
                        .or_default()
                        .extend(dead);
                }
            }

            Ok(count)
        }
        .boxed()
    }

    /// `rebuild_derived`; the aggregates in `keep_range_for` count the
    /// purged block range too (their source rows stay alive in it).
    pub fn rebuild_keeping(
        &self,
        chain: u64,
        from_ts: u32,
        epoch: u32,
        purged_from: u64,
        purged_to: Option<u64>,
        keep_range_for: &[Agg],
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        let step = PurgeStep::RebuildDerived;
        // Partial: only the first aggregate is rebuilt.
        let partial = Self::enter(&mut state, chain, step)?;
        let view = state.view(chain);
        let data = state.write(chain);

        // Every rebuild leaves the purged block range out by itself:
        // its `blocks` rows are still alive, and the tombstones of its
        // children may not be readable yet.
        let purged = |agg: Agg, number: u64| {
            !keep_range_for.contains(&agg)
                && in_range(number, purged_from, purged_to)
        };

        for (number, versions) in &view.blocks {
            for row in live(versions) {
                if row.timestamp >= from_ts
                    && !purged(Agg::BlocksDaily, *number)
                {
                    data.add(
                        Agg::BlocksDaily,
                        row.timestamp,
                        epoch,
                        *number,
                    );
                }
            }
        }

        if partial {
            return Err(injected(step));
        }

        for (index, table) in view.children.iter().enumerate() {
            for ((number, _), versions) in table {
                for row in live(versions) {
                    if row.timestamp >= from_ts
                        && !purged(Agg::child(index), *number)
                    {
                        data.add(
                            Agg::child(index),
                            row.timestamp,
                            epoch,
                            row.data,
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Missing ranges of `[from, to)`, like `Database::missing_ranges`.
    pub fn missing_ranges(
        &self,
        chain: u64,
        from: u64,
        to: u64,
    ) -> Vec<(u64, u64)> {
        let data = self.snapshot(chain);
        let stored: BTreeSet<u64> =
            data.live_blocks().into_keys().collect();
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for number in from..to {
            if stored.contains(&number) {
                continue;
            }
            match ranges.last_mut() {
                Some(last) if last.1 == number => last.1 = number + 1,
                _ => ranges.push((number, number + 1)),
            }
        }
        ranges
    }

    /// The writer's insert into the child tables (the views add to the
    /// aggregates under the epoch of the rows).
    pub fn insert_children(
        &self,
        chain: u64,
        blocks: &[FakeBlock],
        epoch: u32,
        version: u64,
    ) {
        let mut state = self.state.lock().unwrap();
        let data = state.write(chain);
        for block in blocks {
            let timestamp = block.header.timestamp;
            for (table, values) in block.children.iter().enumerate() {
                for (index, value) in values.iter().enumerate() {
                    let row = Row {
                        version,
                        deleted: false,
                        epoch,
                        timestamp,
                        data: *value,
                    };
                    data.children[table]
                        .entry((block.header.number, index as u32))
                        .or_default()
                        .push(row.clone());
                    data.side_children[table]
                        .entry((block.header.number, index as u32))
                        .or_default()
                        .push(row);
                    data.add(Agg::child(table), timestamp, epoch, *value);
                }
            }
        }
    }

    /// A module writes its re-decoded rows: one child table only.
    pub fn insert_child_table(
        &self,
        chain: u64,
        blocks: &[FakeBlock],
        table: usize,
        epoch: u32,
        version: u64,
    ) {
        let mut state = self.state.lock().unwrap();
        let data = state.write(chain);
        for block in blocks {
            let timestamp = block.header.timestamp;
            for (index, value) in block.children[table].iter().enumerate()
            {
                let row = Row {
                    version,
                    deleted: false,
                    epoch,
                    timestamp,
                    data: *value,
                };
                data.children[table]
                    .entry((block.header.number, index as u32))
                    .or_default()
                    .push(row.clone());
                data.side_children[table]
                    .entry((block.header.number, index as u32))
                    .or_default()
                    .push(row);
                data.add(Agg::child(table), timestamp, epoch, *value);
            }
        }
    }

    pub fn insert_blocks(
        &self,
        chain: u64,
        blocks: &[FakeBlock],
        epoch: u32,
        version: u64,
    ) {
        let mut state = self.state.lock().unwrap();
        let data = state.write(chain);
        for block in blocks {
            let header = block.header;
            let row = Row {
                version,
                deleted: false,
                epoch,
                timestamp: header.timestamp,
                data: header.hash,
            };
            data.blocks
                .entry(header.number)
                .or_default()
                .push(row.clone());
            data.side_blocks.entry(header.number).or_default().push(row);
            data.add(
                Agg::BlocksDaily,
                header.timestamp,
                epoch,
                header.number,
            );
        }
    }

    pub fn insert_checkpoint(
        &self,
        chain: u64,
        from: u64,
        to: u64,
        epoch: u32,
        version: u64,
    ) {
        let mut state = self.state.lock().unwrap();
        let data = state.write(chain);
        data.checkpoints.entry((from, to)).or_default().push(Row {
            version,
            deleted: false,
            epoch,
            timestamp: 0,
            data: (),
        });
    }
}

fn injected(step: PurgeStep) -> anyhow::Error {
    anyhow!("injected failure at `{step}`")
}

/// Tombstones for every live row of the key, as seen by a (possibly
/// stale) read.
fn dead_copies<T: Clone>(
    versions: &[Row<T>],
    version: u64,
) -> Vec<Row<T>> {
    live(versions)
        .into_iter()
        .map(|row| Row { version, deleted: true, ..row.clone() })
        .collect()
}

fn overlaps(checkpoint: (u64, u64), from: u64, to: Option<u64>) -> bool {
    checkpoint.1 > from && to.is_none_or(|to| checkpoint.0 < to)
}

impl ReorgStore for FakeStore {
    fn current_epoch(
        &self,
        chain: u64,
    ) -> BoxFuture<'_, anyhow::Result<u32>> {
        async move {
            let mut state = self.state.lock().unwrap();
            Self::enter(&mut state, chain, PurgeStep::ReadEpoch)?;
            let mut view = state.view(chain);
            if std::mem::take(&mut state.stale_epoch_once) {
                view.reorgs.pop();
            }
            Ok(view.reorgs.iter().map(|r| r.epoch).max().unwrap_or(0))
        }
        .boxed()
    }

    fn stored_head(
        &self,
        chain: u64,
    ) -> BoxFuture<'_, anyhow::Result<Option<u64>>> {
        async move {
            let mut state = self.state.lock().unwrap();
            Ok(state.view(chain).live_blocks().into_keys().max())
        }
        .boxed()
    }

    fn stored_hashes(
        &self,
        chain: u64,
        from: u64,
        to: u64,
    ) -> BoxFuture<'_, anyhow::Result<Vec<(u64, B256)>>> {
        async move {
            let mut state = self.state.lock().unwrap();
            Self::enter(&mut state, chain, PurgeStep::FindForkPoint)?;
            Ok(state
                .view(chain)
                .live_blocks()
                .into_iter()
                .filter(|(n, _)| *n >= from && *n < to)
                .map(|(n, hashes)| (n, hashes[0]))
                .collect())
        }
        .boxed()
    }

    fn has_orphan_children(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<bool>> {
        async move {
            let mut state = self.state.lock().unwrap();
            Self::enter(&mut state, chain, PurgeStep::FindOrphans)?;
            let view = state.view(chain);
            let blocks = view.live_blocks();
            Ok(view.children.iter().any(|table| {
                table.iter().any(|((number, _), versions)| {
                    in_range(*number, from, to)
                        && !blocks.contains_key(number)
                        && (!self.orphans_live_only
                            || !live(versions).is_empty())
                })
            }))
        }
        .boxed()
    }

    fn min_timestamp(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<Option<u32>>> {
        async move {
            let mut state = self.state.lock().unwrap();
            Self::enter(&mut state, chain, PurgeStep::MinTimestamp)?;
            let mut view = state.view(chain);
            if std::mem::take(&mut state.miss_lowest_timestamp_once) {
                let lowest = view
                    .blocks
                    .keys()
                    .copied()
                    .find(|number| in_range(*number, from, to));
                view.blocks.retain(|number, _| Some(*number) != lowest);
                for table in view.children.iter_mut() {
                    table.retain(|(number, _), _| Some(*number) != lowest);
                }
            }
            let live_only = self.min_ts_live_only;
            let mut all: Vec<u32> = Vec::new();

            for (number, versions) in &view.blocks {
                if in_range(*number, from, to) {
                    if live_only {
                        all.extend(
                            live(versions).iter().map(|r| r.timestamp),
                        );
                    } else {
                        all.extend(versions.iter().map(|r| r.timestamp));
                    }
                }
            }
            for table in &view.children {
                for ((number, _), versions) in table {
                    if in_range(*number, from, to) {
                        if live_only {
                            all.extend(
                                live(versions).iter().map(|r| r.timestamp),
                            );
                        } else {
                            all.extend(
                                versions.iter().map(|r| r.timestamp),
                            );
                        }
                    }
                }
            }

            Ok(all.into_iter().min())
        }
        .boxed()
    }

    fn live_children(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<u64>> {
        async move {
            let mut state = self.state.lock().unwrap();
            Self::enter(&mut state, chain, PurgeStep::Verify)?;
            if state.children_never_die {
                return Ok(1);
            }
            let view = state.view(chain);
            Ok(view
                .children
                .iter()
                .flat_map(|table| table.iter())
                .filter(|((number, _), _)| in_range(*number, from, to))
                .map(|(_, versions)| live(versions).len() as u64)
                .sum())
        }
        .boxed()
    }

    fn live_blocks(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<u64>> {
        async move {
            let mut state = self.state.lock().unwrap();
            Self::enter(&mut state, chain, PurgeStep::Verify)?;
            let view = state.view(chain);
            Ok(view
                .blocks
                .iter()
                .filter(|(number, _)| in_range(**number, from, to))
                .map(|(_, versions)| live(versions).len() as u64)
                .sum())
        }
        .boxed()
    }

    fn live_side_rows(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<u64>> {
        async move {
            let mut state = self.state.lock().unwrap();
            Self::enter(&mut state, chain, PurgeStep::Verify)?;
            if self.trust_the_views {
                return Ok(0);
            }
            let view = state.view(chain);
            let children: u64 = view
                .side_children
                .iter()
                .flat_map(|table| table.iter())
                .filter(|((number, _), _)| in_range(*number, from, to))
                .map(|(_, versions)| live(versions).len() as u64)
                .sum();
            let blocks: u64 = view
                .side_blocks
                .iter()
                .filter(|(number, _)| in_range(**number, from, to))
                .map(|(_, versions)| live(versions).len() as u64)
                .sum();
            Ok(children + blocks)
        }
        .boxed()
    }

    fn tombstone_side_rows(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
    ) -> BoxFuture<'_, anyhow::Result<u64>> {
        async move {
            let mut state = self.state.lock().unwrap();
            let step = PurgeStep::TombstoneSideTables;
            // Partial: only the child mirrors, not the `blocks` mirror.
            let partial = Self::enter(&mut state, chain, step)?;
            if self.trust_the_views {
                return Ok(0);
            }
            let view = state.view(chain);
            let data = state.write(chain);
            let mut count = 0;

            for (index, table) in view.side_children.iter().enumerate() {
                for (key, versions) in table {
                    if !in_range(key.0, from, to) {
                        continue;
                    }
                    let dead = dead_copies(versions, version);
                    count += dead.len() as u64;
                    data.side_children[index]
                        .entry(*key)
                        .or_default()
                        .extend(dead);
                }
            }

            if partial {
                return Err(injected(step));
            }

            for (number, versions) in &view.side_blocks {
                if !in_range(*number, from, to) {
                    continue;
                }
                let dead = dead_copies(versions, version);
                count += dead.len() as u64;
                data.side_blocks.entry(*number).or_default().extend(dead);
            }

            Ok(count)
        }
        .boxed()
    }

    fn live_checkpoints(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<u64>> {
        async move {
            let mut state = self.state.lock().unwrap();
            Self::enter(&mut state, chain, PurgeStep::Verify)?;
            let view = state.view(chain);
            Ok(view
                .live_checkpoints()
                .into_iter()
                .filter(|checkpoint| overlaps(*checkpoint, from, to))
                .count() as u64)
        }
        .boxed()
    }

    fn tombstone_checkpoints(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
    ) -> BoxFuture<'_, anyhow::Result<u64>> {
        async move {
            let mut state = self.state.lock().unwrap();
            let step = PurgeStep::TombstoneCheckpoints;
            let partial = Self::enter(&mut state, chain, step)?;

            // The SELECT half may be stale, the INSERT half is not.
            let view = state.view(chain);
            let data = state.write(chain);
            let mut count = 0;

            for key in view.live_checkpoints() {
                if !overlaps(key, from, to) {
                    continue;
                }
                let seen = &view.checkpoints[&key];
                let epoch = seen.last().map_or(0, |r| r.epoch);
                let dead = dead_copies(seen, version);
                count += dead.len() as u64;
                data.checkpoints.entry(key).or_default().extend(dead);

                if partial {
                    // Died between the tombstone and the remainder.
                    continue;
                }

                let mut remainders = Vec::new();
                if key.0 < from {
                    remainders.push((key.0, from));
                }
                if let Some(to) = to {
                    if key.1 > to {
                        remainders.push((to, key.1));
                    }
                }
                for remainder in remainders {
                    data.checkpoints.entry(remainder).or_default().push(
                        Row {
                            version,
                            deleted: false,
                            epoch,
                            timestamp: 0,
                            data: (),
                        },
                    );
                }
            }

            if partial {
                return Err(injected(step));
            }
            Ok(count)
        }
        .boxed()
    }

    fn tombstone_children(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
    ) -> BoxFuture<'_, anyhow::Result<u64>> {
        async move {
            self.tombstone_children_of(chain, from, to, version, &[0, 1])
        }
        .boxed()
    }

    fn insert_reorg<'a>(
        &'a self,
        record: &'a ReorgRecord,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        async move {
            let mut state = self.state.lock().unwrap();
            let step = PurgeStep::InsertReorg;
            let chain = record.chain;
            // Partial: the row lands but the acknowledgement is lost.
            let partial = Self::enter(&mut state, chain, step)?;
            state.write(chain).reorgs.push(record.clone());
            if partial {
                return Err(injected(step));
            }
            Ok(())
        }
        .boxed()
    }

    fn rebuild_derived(
        &self,
        chain: u64,
        from_ts: u32,
        epoch: u32,
        purged_from: u64,
        purged_to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        async move {
            self.rebuild_keeping(
                chain,
                from_ts,
                epoch,
                purged_from,
                purged_to,
                &[],
            )
        }
        .boxed()
    }

    fn tombstone_blocks(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
    ) -> BoxFuture<'_, anyhow::Result<u64>> {
        async move {
            let mut state = self.state.lock().unwrap();
            let step = PurgeStep::TombstoneBlocks;
            // Partial: every other block dies.
            let partial = Self::enter(&mut state, chain, step)?;
            let lose_push = state.lose_side_push > 0;
            state.lose_side_push = state.lose_side_push.saturating_sub(1);
            let view = state.view(chain);
            let data = state.write(chain);

            let mut count = 0;
            for (number, versions) in &view.blocks {
                if !in_range(*number, from, to) {
                    continue;
                }
                if partial && number % 2 == 1 {
                    continue;
                }
                let dead = dead_copies(versions, version);
                count += dead.len() as u64;
                if !lose_push {
                    data.side_blocks
                        .entry(*number)
                        .or_default()
                        .extend(dead.clone());
                }
                data.blocks.entry(*number).or_default().extend(dead);
            }

            if partial {
                return Err(injected(step));
            }
            Ok(count)
        }
        .boxed()
    }
}

// --------------------------------------------------------------- writer

/// A failed flush is FATAL in the pipeline: the process always restarts.
#[derive(Debug)]
pub struct WriterDied(&'static str);

impl std::fmt::Display for WriterDied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "injected: writer died {}", self.0)
    }
}

impl std::error::Error for WriterDied {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushFault {
    /// Children written, no `blocks` row: the classic orphan maker.
    AfterChildren,
    /// Children written, only every other `blocks` row.
    AfterSomeBlocks,
}

struct WriterState {
    buffer: Vec<FakeBlock>,
    epoch: u32,
    fault: Option<FlushFault>,
    quiesces: u32,
    fail_quiesce: bool,
}

pub struct FakeWriter {
    chain: u64,
    store: Arc<FakeStore>,
    clock: Arc<ModelClock>,
    /// `quiesce` drops the buffer instead of flushing it.
    discard_on_quiesce: bool,
    /// `quiesce` returns without making sure its flush can be read back
    /// (a contract violation the purge should still survive).
    sloppy: bool,
    state: Mutex<WriterState>,
}

impl FakeWriter {
    pub fn new(
        chain: u64,
        store: Arc<FakeStore>,
        clock: Arc<ModelClock>,
        discard_on_quiesce: bool,
        sloppy: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            chain,
            store,
            clock,
            discard_on_quiesce,
            sloppy,
            state: Mutex::new(WriterState {
                buffer: Vec::new(),
                // Deliberately wrong until somebody adopts the real one.
                epoch: u32::MAX,
                fault: None,
                quiesces: 0,
                fail_quiesce: false,
            }),
        })
    }

    pub fn epoch(&self) -> u32 {
        self.state.lock().unwrap().epoch
    }

    pub fn buffered(&self) -> usize {
        self.state.lock().unwrap().buffer.len()
    }

    pub fn quiesces(&self) -> u32 {
        self.state.lock().unwrap().quiesces
    }

    pub fn fail_next_flush(&self, fault: FlushFault) {
        self.state.lock().unwrap().fault = Some(fault);
    }

    pub fn fail_next_quiesce(&self) {
        self.state.lock().unwrap().fail_quiesce = true;
    }

    pub fn push(&self, blocks: Vec<FakeBlock>) {
        self.state.lock().unwrap().buffer.extend(blocks);
    }

    /// Children first, `blocks` last, then the checkpoints.
    pub fn flush(&self) -> anyhow::Result<()> {
        let (blocks, epoch, fault) = {
            let mut state = self.state.lock().unwrap();
            if state.buffer.is_empty() {
                return Ok(());
            }
            (
                std::mem::take(&mut state.buffer),
                state.epoch,
                state.fault.take(),
            )
        };

        assert_ne!(epoch, u32::MAX, "the writer never adopted an epoch");

        let version = self.clock.next();
        self.store.insert_children(self.chain, &blocks, epoch, version);

        let written: Vec<FakeBlock> = match fault {
            Some(FlushFault::AfterChildren) => {
                return Err(WriterDied("before `blocks`").into())
            }
            Some(FlushFault::AfterSomeBlocks) => blocks
                .iter()
                .filter(|b| b.header.number % 2 == 0)
                .cloned()
                .collect(),
            None => blocks,
        };

        self.store.insert_blocks(self.chain, &written, epoch, version);

        if fault.is_some() {
            return Err(WriterDied("in the middle of `blocks`").into());
        }

        let mut range: Option<(u64, u64)> = None;
        for block in &written {
            let number = block.header.number;
            range = match range {
                Some((from, to)) if to == number => Some((from, to + 1)),
                Some((from, to)) => {
                    self.store.insert_checkpoint(
                        self.chain, from, to, epoch, version,
                    );
                    Some((number, number + 1))
                }
                None => Some((number, number + 1)),
            };
        }
        if let Some((from, to)) = range {
            self.store
                .insert_checkpoint(self.chain, from, to, epoch, version);
        }

        Ok(())
    }
}

impl WriterControl for FakeWriter {
    fn quiesce(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        async move {
            {
                let mut state = self.state.lock().unwrap();
                state.quiesces += 1;
                if state.fail_quiesce {
                    state.fail_quiesce = false;
                    bail!("injected: quiesce failed");
                }
                if self.discard_on_quiesce {
                    state.buffer.clear();
                }
            }
            self.flush()?;
            if !self.sloppy {
                self.store.settle_visibility(self.chain);
            }
            Ok(())
        }
        .boxed()
    }

    fn adopt_epoch(&self, epoch: u32) {
        self.state.lock().unwrap().epoch = epoch;
    }
}

/// Records what the optional hooks were told.
#[derive(Default)]
pub struct Recorder {
    pub evictions: Mutex<Vec<(u64, Option<u64>)>>,
    pub reorg_depths: Mutex<Vec<u64>>,
    pub purged_blocks: Mutex<Vec<u64>>,
}

impl DiscoveryCache for Recorder {
    fn evict_range(&self, from: u64, to: Option<u64>) {
        self.evictions.lock().unwrap().push((from, to));
    }
}

impl ReorgMetrics for Recorder {
    fn reorg(&self, depth: u64) {
        self.reorg_depths.lock().unwrap().push(depth);
    }

    fn purge_observed(&self, _duration: Duration, blocks: u64) {
        self.purged_blocks.lock().unwrap().push(blocks);
    }
}

// ----------------------------------------------------------------- node

#[derive(Debug, Clone, Copy)]
pub struct NodeOptions {
    pub chain_id: u64,
    pub start_block: u64,
    pub confirmations: u64,
    pub max_reorg_depth: u64,
    /// Blocks buffered before the writer flushes.
    pub flush_every: usize,
    /// Blocks per stream response.
    pub response_size: u64,
    pub discard_on_quiesce: bool,
    pub check_joins: bool,
    pub stream_guards: bool,
    pub check_tip: bool,
    pub sloppy_writer: bool,
    /// A starting process seeds its `_version` clock from the store
    /// (`false` = the bug of review round 2, for the negative control).
    pub seed_versions: bool,
}

impl NodeOptions {
    pub fn new(chain_id: u64) -> Self {
        Self {
            chain_id,
            start_block: 0,
            confirmations: 0,
            max_reorg_depth: 64,
            flush_every: 4,
            response_size: 3,
            discard_on_quiesce: false,
            check_joins: true,
            stream_guards: true,
            check_tip: false,
            sloppy_writer: false,
            seed_versions: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassOutcome {
    /// Everything up to the target is stored.
    CaughtUp,
    /// A rollback happened; run another pass.
    RolledBack(super::Rollback),
}

/// One indexer process for one chain: the sync loop of the pipeline,
/// reduced to what matters here.
pub struct Node {
    pub options: NodeOptions,
    pub chain: Arc<FakeChain>,
    pub store: Arc<FakeStore>,
    pub writer: Arc<FakeWriter>,
    pub recorder: Arc<Recorder>,
    pub guard: ReorgGuard,
    /// How far behind the real clock the wall clock of the CURRENT process
    /// is (grows with every `restart_with_clock_step_back`).
    clock_behind: u64,
    cursor: u64,
    started: bool,
    pub rollbacks: Vec<super::Rollback>,
}

impl Node {
    pub fn new(
        options: NodeOptions,
        chain: Arc<FakeChain>,
        store: Arc<FakeStore>,
    ) -> Self {
        let recorder = Arc::new(Recorder::default());
        let (writer, guard) =
            Self::boot(&options, &chain, &store, &recorder, 0);
        Self {
            options,
            chain,
            store,
            writer,
            recorder,
            guard,
            clock_behind: 0,
            cursor: options.start_block,
            started: false,
            rollbacks: Vec::new(),
        }
    }

    fn boot(
        options: &NodeOptions,
        chain: &Arc<FakeChain>,
        store: &Arc<FakeStore>,
        recorder: &Arc<Recorder>,
        clock_behind: u64,
    ) -> (Arc<FakeWriter>, ReorgGuard) {
        // What `Database::seed_version` does at startup.
        let seed = if options.seed_versions {
            store.snapshot(options.chain_id).max_version()
        } else {
            0
        };
        let clock = ModelClock::new(clock_behind, seed);

        let writer = FakeWriter::new(
            options.chain_id,
            store.clone(),
            clock.clone(),
            options.discard_on_quiesce,
            options.sloppy_writer,
        );
        let purger = Purger::new(
            store.clone(),
            writer.clone(),
            recorder.clone(),
            recorder.clone(),
        )
        .with_version_source(Arc::new(move || clock.next()))
        .with_options(PurgeOptions {
            tombstone_attempts: 6,
            retry_delay: Duration::ZERO,
        });
        let config = ReorgConfig {
            max_reorg_depth: options.max_reorg_depth,
            canonical_attempts: 4,
            canonical_retry_delay: Duration::ZERO,
            ..ReorgConfig::new(options.chain_id, options.start_block)
        };
        (writer.clone(), ReorgGuard::new(config, chain.clone(), purger))
    }

    /// The process died, and the wall clock of the host it comes back on
    /// is `step_back` version units (ms) behind where it was.
    pub fn restart_with_clock_step_back(&mut self, step_back: u64) {
        self.clock_behind = self.clock_behind.saturating_add(step_back);
        self.restart();
    }

    /// The process died: buffered rows and every in-memory state are gone.
    pub fn restart(&mut self) {
        let (writer, guard) = Self::boot(
            &self.options,
            &self.chain,
            &self.store,
            &self.recorder,
            self.clock_behind,
        );
        self.writer = writer;
        self.guard = guard;
        self.store.settle_visibility(self.options.chain_id);
        self.cursor = self.options.start_block;
        self.started = false;
    }

    pub fn target(&self) -> u64 {
        self.chain.head().saturating_sub(self.options.confirmations)
    }

    pub fn data(&self) -> ChainData {
        self.store.snapshot(self.options.chain_id)
    }

    /// One pass of the sync loop.
    pub async fn pass(&mut self) -> anyhow::Result<PassOutcome> {
        let chain_id = self.options.chain_id;

        if !self.started {
            self.guard.startup().await?;
            self.started = true;
        }

        let target = self.target();

        if target <= self.cursor {
            // Still a pass: gap healing must not wait for new blocks.
            self.guard.begin_pass(&[], self.cursor).await?;

            if self.options.check_tip {
                let verdict = self.guard.check_tip().await;
                if let Some(outcome) = self.settle_verdict(verdict).await?
                {
                    return Ok(outcome);
                }
            }

            return Ok(PassOutcome::CaughtUp);
        }

        let missing =
            self.store.missing_ranges(chain_id, self.cursor, target);
        self.guard.begin_pass(&missing, target).await?;

        for (from, to) in missing {
            let mut at = from;

            while at < to {
                let end = (at + self.options.response_size.max(1)).min(to);
                let blocks = self.chain.fetch(at, end);

                if blocks.len() as u64 != end - at {
                    // The chain got shorter under our feet.
                    self.writer.flush()?;
                    bail!("stream ended early at {at}");
                }

                let headers: Vec<BlockHeader> =
                    blocks.iter().map(|b| b.header).collect();
                let stream_guard = StreamGuard {
                    first_block: headers[0].number,
                    first_parent_hash: headers[0].parent_hash,
                };

                let verdict = self
                    .guard
                    .observe(
                        &headers,
                        self.options
                            .stream_guards
                            .then_some(&stream_guard),
                    )
                    .await;

                if let Some(outcome) = self.settle_verdict(verdict).await?
                {
                    return Ok(outcome);
                }

                self.writer.push(blocks);
                if self.writer.buffered() >= self.options.flush_every {
                    self.writer.flush()?;
                }

                at = end;
            }

            if self.options.check_joins {
                let verdict = self.guard.check_join(to).await;
                if let Some(outcome) = self.settle_verdict(verdict).await?
                {
                    return Ok(outcome);
                }
            }
        }

        self.writer.flush()?;
        self.cursor = target;

        Ok(PassOutcome::CaughtUp)
    }

    async fn settle_verdict(
        &mut self,
        verdict: Result<Verdict, ReorgError>,
    ) -> anyhow::Result<Option<PassOutcome>> {
        let verdict = match verdict {
            Ok(verdict) => verdict,
            Err(e) => {
                // The pipeline commits what was delivered, also on errors.
                self.writer.flush()?;
                return Err(e.into());
            }
        };

        match verdict {
            Verdict::Continue => Ok(None),
            Verdict::Rollback(rollback) => {
                // FIRST, and whatever `repair` answers: below the cursor
                // everything is stored, and that is no longer true.
                self.cursor = self.cursor.min(rollback.fork_point);
                self.guard.repair(&rollback).await?;
                self.rollbacks.push(rollback);
                Ok(Some(PassOutcome::RolledBack(rollback)))
            }
        }
    }

    /// Runs passes until caught up. An error is either a crash (`restart`
    /// = true: the process starts over) or a failed pass that the same
    /// process retries. Fatal reorg errors are returned.
    pub async fn settle(
        &mut self,
        restart: bool,
    ) -> Result<u32, ReorgError> {
        for passes in 1..=200 {
            match self.pass().await {
                Ok(PassOutcome::CaughtUp) => return Ok(passes),
                Ok(PassOutcome::RolledBack(_)) => {}
                Err(e) => {
                    if let Some(reorg) = e.downcast_ref::<ReorgError>() {
                        if reorg.is_fatal() {
                            return Err(e
                                .downcast::<ReorgError>()
                                .unwrap());
                        }
                    }
                    let writer_died =
                        e.chain().any(|cause| cause.is::<WriterDied>());
                    if restart || writer_died {
                        self.restart();
                    }
                }
            }
        }
        panic!("chain {}: did not settle", self.options.chain_id);
    }
}

// ---------------------------------------------------------- verification

/// What a clean index of the canonical blocks `[from, to)` looks like.
pub fn clean_index(chain: &FakeChain, from: u64, to: u64) -> ChainData {
    let store = FakeStore::new();
    let blocks: Vec<FakeBlock> =
        (from..to).filter_map(|number| chain.block(number)).collect();
    store.insert_children(0, &blocks, 0, 1);
    store.insert_blocks(0, &blocks, 0, 1);
    store.snapshot(0)
}

/// The proof obligation: what readers see (`FINAL` on base tables, the
/// validity rule on aggregates) equals a clean index of the canonical chain.
pub fn assert_clean(node: &Node, context: &str) {
    if let Err(problem) = check_clean(node) {
        panic!("{context}: {problem}");
    }
    assert_checkpoints_honest(&node.data(), context);
}

/// [`assert_clean`] without the panic.
pub fn check_clean(node: &Node) -> Result<(), String> {
    let data = node.data();
    let target = node.target();
    let clean = clean_index(&node.chain, node.options.start_block, target);

    // At most one live row per key, in every table.
    for (number, hashes) in data.live_blocks() {
        if hashes.len() != 1 {
            return Err(format!("block {number} is live twice"));
        }
    }
    for table in 0..CHILD_TABLES {
        for (key, values) in data.live_children(table) {
            if values.len() != 1 {
                return Err(format!("child {key:?} is live twice"));
            }
        }
    }

    let (stored, expected) = (data.live_blocks(), clean.live_blocks());
    if stored != expected {
        let numbers: BTreeSet<u64> =
            stored.keys().chain(expected.keys()).copied().collect();
        let wrong: Vec<String> = numbers
            .into_iter()
            .filter(|n| stored.get(n) != expected.get(n))
            .map(|n| match (stored.get(&n), expected.get(&n)) {
                (Some(_), Some(_)) => format!("{n} orphaned"),
                (Some(_), None) => format!("{n} unexpected"),
                _ => format!("{n} missing"),
            })
            .collect();
        return Err(format!(
            "blocks differ from the canonical chain [{}, {target}): {}",
            node.options.start_block,
            wrong.join(", ")
        ));
    }
    for table in 0..CHILD_TABLES {
        if data.live_children(table) != clean.live_children(table) {
            return Err(format!("child table {table} differs"));
        }
        // A side table is a mirror: an orphan there is invisible in every
        // base table and permanent (nothing rewrites it).
        if data.live_side_children(table) != clean.live_children(table) {
            return Err(format!(
                "side table of child {table} differs from the canonical \
                 chain (a materialized view push was lost and never \
                 repaired)"
            ));
        }
    }
    if data.live_side_blocks() != clean.live_blocks() {
        return Err(
            "the `blocks` side table differs from the canonical chain"
                .to_string(),
        );
    }
    if data.aggregates() != clean.aggregates() {
        return Err(format!(
            "aggregates differ:\n  stored {:?}\n  clean  {:?}\n  reorgs \
             {:?}",
            data.aggregates(),
            clean.aggregates(),
            data.reorgs
        ));
    }

    Ok(())
}

/// Holds at EVERY moment, also right after a crash: a checkpoint never
/// claims a block that is not stored.
pub fn assert_checkpoints_honest(data: &ChainData, context: &str) {
    let blocks = data.live_blocks();
    for (from, to) in data.live_checkpoints() {
        for number in from..to {
            assert!(
                blocks.contains_key(&number),
                "{context}: checkpoint [{from}, {to}) claims block \
                 {number}, which is not stored"
            );
        }
    }
}
