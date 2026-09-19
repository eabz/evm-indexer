//! Reading and writing the floor (migration `0008_chain_coverage.sql`).
//!
//! Every write goes through this file, and there are only two of them:
//! [`set_if_absent`], which a chain's first start uses, and [`lower_to`],
//! which `indexer backfill` uses once an older range is complete. There is
//! no "set" and no "update": the floor is a fact about the stored data, and
//! the only honest way to change it is to change the data first.

use crate::{coverage::date, db::Database, pipeline::lease::Fence};
use anyhow::{Context, Result};
use log::info;

/// How a floor came to be, for the operator reading a row a year later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// No flag: now - 365 days (EVM).
    DefaultYear,
    /// No flag, or `--new-blocks-only`: the head (Solana).
    Head,
    /// `--start-block N` on the chain's first start.
    StartBlock,
    /// `--start-date YYYY-MM-DD` on the chain's first start.
    StartDate,
    /// Lowered by `indexer backfill`.
    Backfill,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DefaultYear => "default-1y",
            Self::Head => "head",
            Self::StartBlock => "start-block",
            Self::StartDate => "start-date",
            Self::Backfill => "backfill",
        }
    }

    /// A word nobody recognises reads as "we no longer know", never as one
    /// of the real reasons.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim() {
            "default-1y" => Self::DefaultYear,
            "head" => Self::Head,
            "start-block" => Self::StartBlock,
            "start-date" => Self::StartDate,
            "backfill" => Self::Backfill,
            _ => return None,
        })
    }

    /// How the line in a log or on a web page explains itself.
    pub fn plainly(self) -> &'static str {
        match self {
            Self::DefaultYear => "the default, one year of history",
            Self::Head => "the head of the chain when indexing started",
            Self::StartBlock => "--start-block",
            Self::StartDate => "--start-date",
            Self::Backfill => "lowered by indexer backfill",
        }
    }
}

/// The date this chain is complete from, and the block it resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Floor {
    pub block: u64,
    /// Unix seconds of that block; 0 when the source gave none.
    pub timestamp: u32,
    pub reason: Reason,
}

impl Floor {
    /// `2024-03-01`, or `unknown` when the block carried no timestamp.
    pub fn date(&self) -> String {
        if self.timestamp == 0 {
            "unknown".to_string()
        } else {
            date::format(i64::from(self.timestamp))
        }
    }

    /// `ReplacingMergeTree` keeps the HIGHEST `_version`, so the version is
    /// the block counted downwards and the LOWEST floor is the one that
    /// survives. See the migration's header: this is what makes "a later
    /// start cannot raise the floor" true of the engine and not only of the
    /// code above it.
    pub fn version(&self) -> u64 {
        u64::MAX - self.block
    }
}

/// What this chain promises, as `coverage_v` answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coverage {
    pub floor: Floor,
    /// Exclusive end of the gap-free run from the floor. Equal to
    /// `floor.block` when nothing is covered yet.
    pub covered_to_block: u64,
}

impl Coverage {
    /// Is there anything at all below the covered head?
    pub fn is_empty(&self) -> bool {
        self.covered_to_block <= self.floor.block
    }
}

/// The floor this chain already has, or `None` when it has none yet.
pub async fn stored(db: &Database) -> Result<Option<Floor>> {
    let sql = format!(
        "SELECT coverage_from_block, coverage_from_ts, reason \
         FROM chain_coverage FINAL WHERE chain = {}",
        db.chain_id
    );

    let rows = db
        .db
        .query(&sql)
        .fetch_all::<(u64, u32, String)>()
        .await
        .context("read the chain's coverage floor")?;

    Ok(rows.into_iter().next().map(|(block, timestamp, reason)| Floor {
        block,
        timestamp,
        // An unknown word is not a reason to refuse to start: the numbers
        // are what matter and they are right there.
        reason: Reason::parse(&reason).unwrap_or(Reason::StartBlock),
    }))
}

/// The floor and the gap-free head, as `coverage_v` answers them.
pub async fn coverage(db: &Database) -> Result<Option<Coverage>> {
    let sql = format!(
        "SELECT coverage_from_block, coverage_from_ts, reason, \
         covered_to_block FROM coverage_v WHERE chain = {}",
        db.chain_id
    );

    let rows = db
        .db
        .query(&sql)
        .fetch_all::<(u64, u32, String, u64)>()
        .await
        .context("read the chain's coverage")?;

    Ok(rows.into_iter().next().map(
        |(block, timestamp, reason, covered_to_block)| Coverage {
            floor: Floor {
                block,
                timestamp,
                reason: Reason::parse(&reason)
                    .unwrap_or(Reason::StartBlock),
            },
            covered_to_block,
        },
    ))
}

/// Writes `floor` when the chain has none, and returns the floor the chain
/// has afterwards.
///
/// The read comes first so that a second start can SAY that it kept the old
/// floor rather than silently doing so; the engine's version rule (see
/// [`Floor::version`]) is what makes the answer right even when two
/// processes get here at the same moment and neither sees the other's
/// insert (ClickHouse has no read-your-writes, design section 2).
pub async fn set_if_absent(
    db: &Database,
    fence: &Fence,
    floor: Floor,
) -> Result<Floor> {
    if let Some(existing) = stored(db).await? {
        return Ok(existing);
    }

    write(db, fence, floor).await?;

    info!(
        "Chain {}: coverage floor set to block {} ({}), from {}. This is \
         now fixed: everything from here on is kept, and it does not move \
         when the process restarts or a flag changes.",
        db.chain_id,
        floor.block,
        floor.date(),
        floor.reason.plainly()
    );

    Ok(floor)
}

/// Lowers the floor to `floor`, refusing to raise it.
///
/// Called by `indexer backfill` AFTER the older range is complete and
/// verified, which is the only moment the lower claim is true.
pub async fn lower_to(
    db: &Database,
    fence: &Fence,
    floor: Floor,
) -> Result<Floor> {
    let Some(existing) = stored(db).await? else {
        return set_if_absent(db, fence, floor).await;
    };

    if floor.block >= existing.block {
        // Not an error: a backfill of a range that is already inside the
        // covered window is a perfectly ordinary thing to run.
        info!(
            "Chain {}: the coverage floor stays at block {} ({}). Block {} \
             is not below it, and the floor never moves later - no data is \
             ever dropped.",
            db.chain_id,
            existing.block,
            existing.date(),
            floor.block
        );
        return Ok(existing);
    }

    write(db, fence, floor).await?;

    info!(
        "Chain {}: coverage floor lowered from block {} ({}) to block {} \
         ({}). This database is now complete from the earlier date.",
        db.chain_id,
        existing.block,
        existing.date(),
        floor.block,
        floor.date()
    );

    Ok(floor)
}

/// The one INSERT. Fenced like every other write: a process whose lease has
/// gone must not be the one that decides what this database promises.
async fn write(db: &Database, fence: &Fence, floor: Floor) -> Result<()> {
    fence.check()?;

    let sql = format!(
        "INSERT INTO chain_coverage \
         (chain, coverage_from_block, coverage_from_ts, reason, _version) \
         SELECT {}, {}, {}, '{}', {}",
        db.chain_id,
        floor.block,
        floor.timestamp,
        // Not user input: one of five words this file owns. Written out so
        // a reader does not have to go and check.
        floor.reason.as_str(),
        floor.version()
    );

    db.db.query(&sql).execute().await.with_context(|| {
        format!("store the coverage floor of chain {}", db.chain_id)
    })
}

/// What a position on this chain is CALLED. The column is `block_number`
/// everywhere (design section 13), but on Solana it holds a slot, and a
/// sentence for a person has to say the word the person uses.
pub fn unit_of(chain: u64) -> &'static str {
    if chain == crate::pipeline::solana::SOLANA_CHAIN_ID {
        "slot"
    } else {
        "block"
    }
}

/// The sentence every surface prints: `indexer verify`, the fleet's status
/// line and the control panel all say the same thing in the same words.
///
/// `covered_to_block` is exclusive, so the last covered block is one below
/// it, and `covered_to_date` is that block's day when the caller could
/// afford to look it up. `unit` is [`unit_of`] for the chain.
pub fn sentence(
    coverage: &Coverage,
    unit: &str,
    covered_to_date: Option<&str>,
    stored_head: Option<u64>,
) -> String {
    if coverage.is_empty() {
        return format!(
            "Coverage: nothing stored yet. The floor is {} ({unit} {}), \
             from {}.",
            coverage.floor.date(),
            coverage.floor.block,
            coverage.floor.reason.plainly()
        );
    }

    let last = coverage.covered_to_block.saturating_sub(1);
    let to = match covered_to_date {
        Some(date) => format!("{date} ({unit} {last})"),
        None => format!("{unit} {last}"),
    };

    let mut line = format!(
        "Coverage: gap-free from {} ({unit} {}) to {}.",
        coverage.floor.date(),
        coverage.floor.block,
        to
    );

    if let Some(head) = stored_head {
        if head >= coverage.covered_to_block {
            line.push_str(&format!(
                " There are blocks above that, up to {head}, with at least \
                 one hole in between: `indexer verify` says where."
            ));
        }
    }

    line
}

/// Warns, in the owner's words, that a flag was ignored because the floor
/// is already fixed. `None` when the flag agrees with the stored floor (or
/// when there was no flag).
pub fn disagreement(
    stored: Floor,
    wanted: &super::Wanted,
) -> Option<String> {
    let (asked, how) = match wanted {
        super::Wanted::Block(block) if *block != stored.block => {
            (format!("block {block}"), "--start-block")
        }
        super::Wanted::Date(date)
            if date.midnight() != date_of(stored.timestamp) =>
        {
            (date.to_string(), "--start-date")
        }
        _ => return None,
    };

    Some(format!(
        "{how} says {asked}, but chain's coverage floor was fixed at block \
         {} ({}) when it was first indexed, from {}. The stored floor is \
         kept: it is what this database actually promises, and moving it \
         later would drop data. To go FURTHER BACK, run `indexer backfill \
         {how} ...`; the floor follows once that range is complete.",
        stored.block,
        stored.date(),
        stored.reason.plainly()
    ))
}

fn date_of(timestamp: u32) -> i64 {
    date::start_of_day(i64::from(timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage::{date::Date, Wanted};

    #[test]
    fn the_version_makes_the_lowest_floor_win() {
        let early =
            Floor { block: 100, timestamp: 1, reason: Reason::Backfill };
        let late = Floor {
            block: 1_000_000,
            timestamp: 2,
            reason: Reason::StartBlock,
        };

        // ReplacingMergeTree keeps the HIGHEST version, and that has to be
        // the EARLIEST floor.
        assert!(
            early.version() > late.version(),
            "a later floor would have won and raised the promise"
        );

        // Block 0 is the earliest floor there is, so it must carry the
        // highest version of all.
        let genesis =
            Floor { block: 0, timestamp: 3, reason: Reason::StartBlock };
        assert_eq!(genesis.version(), u64::MAX);
    }

    #[test]
    fn every_reason_survives_the_round_trip() {
        for reason in [
            Reason::DefaultYear,
            Reason::Head,
            Reason::StartBlock,
            Reason::StartDate,
            Reason::Backfill,
        ] {
            assert_eq!(Reason::parse(reason.as_str()), Some(reason));
            assert!(!reason.plainly().is_empty());
        }

        assert_eq!(Reason::parse("something else"), None);
    }

    fn floor(block: u64, date: &str) -> Floor {
        Floor {
            block,
            timestamp: Date::parse(date).unwrap().midnight() as u32,
            reason: Reason::StartDate,
        }
    }

    #[test]
    fn a_flag_that_agrees_with_the_stored_floor_says_nothing() {
        let stored = floor(500, "2024-03-01");

        assert_eq!(disagreement(stored, &Wanted::Block(500)), None);
        assert_eq!(
            disagreement(
                stored,
                &Wanted::Date(Date::parse("2024-03-01").unwrap())
            ),
            None
        );
        // No flag at all is never a disagreement.
        assert_eq!(disagreement(stored, &Wanted::DefaultYear), None);
        assert_eq!(disagreement(stored, &Wanted::Head), None);
    }

    #[test]
    fn a_flag_that_disagrees_says_what_happened_and_what_to_do() {
        let stored = floor(500, "2024-03-01");

        let why = disagreement(stored, &Wanted::Block(9)).unwrap();
        assert!(why.contains("--start-block"), "{why}");
        assert!(why.contains("500"), "{why}");
        assert!(why.contains("2024-03-01"), "{why}");
        assert!(why.contains("indexer backfill"), "{why}");

        let why = disagreement(
            stored,
            &Wanted::Date(Date::parse("2020-01-01").unwrap()),
        )
        .unwrap();
        assert!(why.contains("--start-date"), "{why}");
        assert!(why.contains("2020-01-01"), "{why}");
    }

    #[test]
    fn the_sentence_reads_the_same_everywhere() {
        let coverage = Coverage {
            floor: floor(500, "2024-03-01"),
            covered_to_block: 1_000,
        };

        let line =
            sentence(&coverage, "block", Some("2024-06-15"), Some(999));
        assert_eq!(
            line,
            "Coverage: gap-free from 2024-03-01 (block 500) to 2024-06-15 \
             (block 999)."
        );

        // A stored head above the gap-free part is the interesting case.
        let line =
            sentence(&coverage, "block", Some("2024-06-15"), Some(5_000));
        assert!(line.contains("up to 5000"), "{line}");
        assert!(line.contains("hole"), "{line}");

        // Nothing stored yet.
        let empty = Coverage {
            floor: floor(500, "2024-03-01"),
            covered_to_block: 500,
        };
        let line = sentence(&empty, "block", None, None);
        assert!(line.contains("nothing stored yet"), "{line}");
        assert!(line.contains("block 500"), "{line}");

        // On Solana the same sentence says "slot", because that is the
        // word the person reading it uses.
        let line = sentence(&coverage, "slot", None, Some(999));
        assert!(line.contains("slot 500"), "{line}");
        assert!(line.contains("slot 999"), "{line}");
        assert!(!line.contains("block"), "{line}");
    }

    #[test]
    fn only_solana_counts_in_slots() {
        assert_eq!(unit_of(1), "block");
        assert_eq!(unit_of(8453), "block");
        assert_eq!(unit_of(1_399_811_149), "slot");
    }

    #[test]
    fn a_floor_without_a_timestamp_never_prints_1970() {
        let floor =
            Floor { block: 7, timestamp: 0, reason: Reason::StartBlock };
        assert_eq!(floor.date(), "unknown");
    }
}
