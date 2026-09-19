//! The coverage floor: one consistent window, live data first
//! (docs/design.md section 16).
//!
//! # What this module is for
//!
//! The promise this indexer makes is **"gap-free and consistent from a
//! known date to now, everything kept"**, and NOT "all of history". The
//! floor is that known date. It is chosen once, on a chain's first start,
//! written down next to the chain's data, and from then on it is a fact
//! about the database rather than a setting anybody can change: a restart,
//! a different flag, or a box in the control panel all keep it exactly
//! where it is, loudly.
//!
//! Three questions, three files:
//!
//! | Question | Where |
//! |---|---|
//! | What day is `2024-03-01`, in unix seconds, and what day is this timestamp? | [`date`] |
//! | Which block is the first one at or after that moment? | [`resolve`] |
//! | What is this chain's floor, and may I change it? | [`store`] |
//!
//! This file holds the decision that joins them: given what the flags ask
//! for and what is already stored, what happens. It is deliberately a pure
//! function ([`decide`]) with the database and the source on either side of
//! it, so the rules can be read and tested without either.
//!
//! # The rules, in one place
//!
//! * **Nothing stored, no flags**: an EVM chain starts one year ago
//!   (resolved to a block by a binary search over block timestamps); Solana
//!   starts at the head, because Envio serves Solana history from
//!   2026-01-03, the free tier is slow, and going back is a choice rather
//!   than a default.
//! * **Nothing stored, a flag**: `--start-block N` or `--start-date D`
//!   decides, and `--new-blocks-only` means the head on either family.
//! * **Something stored**: the stored floor wins, always. A flag that
//!   disagrees produces a warning that says what was ignored and how to get
//!   what was asked for.
//! * **Moving it EARLIER** is `indexer backfill --start-block / --start-date`,
//!   which lowers the floor once the older range is complete and verified.
//! * **Moving it LATER** is refused, on every path, by two independent
//!   mechanisms (see the migration's header).

pub mod date;
pub mod resolve;
pub mod store;

use crate::{db::Database, pipeline::lease::Fence, reorg::CanonicalChain};
use anyhow::Result;
use date::Date;
use log::warn;
use store::{Floor, Reason};

/// One year, the default depth of history for an EVM chain.
///
/// Not a round number picked for looks: a year is what makes every
/// "all-time", "last 12 months" and year-on-year figure a real answer
/// rather than an artefact of when the indexer happened to be started.
pub const DEFAULT_HISTORY: i64 = 365 * date::SECONDS_PER_DAY;

/// Which family a chain belongs to, which is the only thing the default
/// depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Evm,
    Svm,
}

/// What the flags ask for, before anything has been read or resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wanted {
    /// `--start-block N`.
    Block(u64),
    /// `--start-date YYYY-MM-DD`.
    Date(Date),
    /// No flag, on an EVM chain: one year ago.
    DefaultYear,
    /// `--new-blocks-only`, or no flag on Solana.
    Head,
}

impl Wanted {
    /// Reads the three flags the way the CLI hands them over.
    ///
    /// `--start-block` and `--start-date` are mutually exclusive at the
    /// command line, so at most one of them is set here.
    pub fn of(
        start_block: u64,
        start_date: Option<Date>,
        new_blocks_only: bool,
        family: Family,
    ) -> Self {
        // The head beats everything: "only new blocks" is not a request
        // for a window, it is a request for no history at all.
        if new_blocks_only {
            return Self::Head;
        }
        if let Some(date) = start_date {
            return Self::Date(date);
        }
        if start_block > 0 {
            return Self::Block(start_block);
        }
        match family {
            Family::Evm => Self::DefaultYear,
            // Solana: live first. Envio serves history from 2026-01-03 and
            // the free tier is slow, so a year of it is not a default
            // anybody would want to discover by accident.
            Family::Svm => Self::Head,
        }
    }

    fn reason(&self) -> Reason {
        match self {
            Self::Block(_) => Reason::StartBlock,
            Self::Date(_) => Reason::StartDate,
            Self::DefaultYear => Reason::DefaultYear,
            Self::Head => Reason::Head,
        }
    }
}

/// What happens, given what is stored and what the flags ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// A floor is already stored and it wins. The string is the warning to
    /// log when a flag asked for something else.
    Keep(Floor, Option<String>),
    /// Nothing is stored yet: resolve this and write it down.
    Resolve(Wanted),
}

/// The whole rule, with no database and no chain behind it.
pub fn decide(stored: Option<Floor>, wanted: Wanted) -> Decision {
    match stored {
        Some(floor) => {
            Decision::Keep(floor, store::disagreement(floor, &wanted))
        }
        None => Decision::Resolve(wanted),
    }
}

/// Decides, resolves and persists this chain's floor, and returns it.
///
/// Called once per start, after the lease is held: the lease is what makes
/// one process per chain, which is what makes "the first writer wins" a
/// statement about two starts rather than about two threads.
///
/// `head` is the source's exclusive height and `now` is unix seconds - both
/// passed in rather than read here, so a test can drive this with a fake
/// chain and a fixed clock.
pub async fn establish(
    db: &Database,
    fence: &Fence,
    chain: &dyn CanonicalChain,
    head: u64,
    now: i64,
    wanted: Wanted,
) -> Result<Floor> {
    let stored = store::stored(db).await?;

    let wanted = match decide(stored, wanted) {
        Decision::Keep(floor, warning) => {
            if let Some(warning) = warning {
                warn!("Chain {}: {warning}", db.chain_id);
            }
            return Ok(floor);
        }
        Decision::Resolve(wanted) => wanted,
    };

    let floor = resolve_floor(chain, head, now, wanted).await?;

    store::set_if_absent(db, fence, floor).await
}

/// Turns a [`Wanted`] into a block and a timestamp, asking the source only
/// when it has to.
pub async fn resolve_floor(
    chain: &dyn CanonicalChain,
    head: u64,
    now: i64,
    wanted: Wanted,
) -> Result<Floor> {
    let reason = wanted.reason();

    let at_or_after = match wanted {
        // A block number needs no search - but its timestamp does, and it
        // is one request, so the floor can be reported as a DATE like every
        // other floor.
        Wanted::Block(block) => {
            let timestamp = timestamp_of(chain, block, head).await;
            return Ok(Floor { block, timestamp, reason });
        }
        Wanted::Head => {
            let block = head.saturating_sub(1);
            let timestamp = timestamp_of(chain, block, head).await;
            return Ok(Floor { block, timestamp, reason });
        }
        Wanted::Date(date) => date.midnight(),
        Wanted::DefaultYear => now - DEFAULT_HISTORY,
    };

    let found =
        resolve::block_at_or_after(chain, head, at_or_after).await?;

    log::info!(
        "Chain: resolved {} to block {} ({}) in {} header request(s).",
        match wanted {
            Wanted::Date(date) => date.to_string(),
            _ => format!("{} (one year ago)", date::format(at_or_after)),
        },
        found.block,
        date::format(i64::from(found.timestamp)),
        found.probes
    );

    Ok(Floor { block: found.block, timestamp: found.timestamp, reason })
}

/// The floor on a chain whose source serves no block headers to search -
/// Solana, where a slot is not a block and there is nothing to bisect.
///
/// Only two answers are reachable there, and `--start-date` is refused by
/// `pipeline::solana::check_flags` rather than quietly rounded to a slot:
///
/// * the HEAD, which is the default and what `--new-blocks-only` asks for.
///   Its timestamp is `now`, which is what "the head" means to within a
///   second and is a far better answer than "unknown";
/// * a SLOT the operator named, whose time nothing here knows. The floor is
///   still exact - it is a slot number - and only the DATE it is reported
///   with is unknown until someone looks it up.
pub fn slot_floor(head: u64, now: i64, wanted: Wanted) -> Floor {
    match wanted {
        Wanted::Block(slot) => {
            Floor { block: slot, timestamp: 0, reason: Reason::StartBlock }
        }
        // `Date` and `DefaultYear` cannot reach here (the CLI refuses the
        // first and `Wanted::of` never produces the second for Solana),
        // but a floor is not worth a panic: the head is the safe answer.
        _ => Floor {
            block: head.saturating_sub(1),
            timestamp: now.clamp(0, i64::from(u32::MAX)) as u32,
            reason: Reason::Head,
        },
    }
}

/// The timestamp of one block, or 0 when the source will not say.
///
/// Never fatal: a floor whose DATE is unknown is still a perfectly good
/// floor, and refusing to start a chain because one header request failed
/// would be the wrong trade.
async fn timestamp_of(
    chain: &dyn CanonicalChain,
    block: u64,
    head: u64,
) -> u32 {
    if head == 0 {
        return 0;
    }

    match chain.headers(block, (block + 1).min(head.max(block + 1))).await
    {
        Ok(headers) => headers
            .iter()
            .find(|header| header.number == block)
            .map_or(0, |header| header.timestamp),
        Err(e) => {
            warn!(
                "Could not read the timestamp of block {block} for the \
                 coverage floor ({e:#}). The floor is still block {block}; \
                 only the date it is reported with is unknown."
            );
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage::resolve::tests::FakeChain;

    fn a_date(value: &str) -> Date {
        Date::parse(value).unwrap()
    }

    // ------------------------------------------------ what the flags mean

    #[test]
    fn with_no_flags_an_evm_chain_asks_for_a_year_and_solana_for_the_head()
    {
        assert_eq!(
            Wanted::of(0, None, false, Family::Evm),
            Wanted::DefaultYear
        );
        assert_eq!(Wanted::of(0, None, false, Family::Svm), Wanted::Head);
    }

    #[test]
    fn new_blocks_only_still_means_the_head_on_both_families() {
        for family in [Family::Evm, Family::Svm] {
            assert_eq!(Wanted::of(0, None, true, family), Wanted::Head);
            // And it beats a flag that asks for history, which is what
            // "--new-blocks-only skips history" has always meant.
            assert_eq!(Wanted::of(100, None, true, family), Wanted::Head);
            assert_eq!(
                Wanted::of(0, Some(a_date("2020-01-01")), true, family),
                Wanted::Head
            );
        }
    }

    #[test]
    fn a_flag_is_what_it_says() {
        assert_eq!(
            Wanted::of(1_000, None, false, Family::Evm),
            Wanted::Block(1_000)
        );
        assert_eq!(
            Wanted::of(0, Some(a_date("2024-03-01")), false, Family::Evm),
            Wanted::Date(a_date("2024-03-01"))
        );
    }

    // --------------------------------------------- the decision itself

    #[test]
    fn a_stored_floor_always_wins() {
        let stored = Floor {
            block: 500,
            timestamp: a_date("2024-03-01").midnight() as u32,
            reason: Reason::DefaultYear,
        };

        for wanted in [
            Wanted::Block(9),
            Wanted::Date(a_date("2020-01-01")),
            Wanted::DefaultYear,
            Wanted::Head,
        ] {
            match decide(Some(stored), wanted) {
                Decision::Keep(kept, _) => assert_eq!(kept, stored),
                other => panic!("{wanted:?} gave {other:?}"),
            }
        }
    }

    /// The owner has to be TOLD, or they will believe the flag they typed.
    #[test]
    fn a_flag_that_is_ignored_produces_a_warning() {
        let stored = Floor {
            block: 500,
            timestamp: a_date("2024-03-01").midnight() as u32,
            reason: Reason::DefaultYear,
        };

        let Decision::Keep(_, Some(warning)) =
            decide(Some(stored), Wanted::Block(9))
        else {
            panic!("no warning for a --start-block that was ignored");
        };
        assert!(warning.contains("indexer backfill"), "{warning}");

        // And a flag that agrees says nothing at all.
        assert_eq!(
            decide(Some(stored), Wanted::Block(500)),
            Decision::Keep(stored, None)
        );
    }

    #[test]
    fn an_empty_database_resolves_what_was_asked_for() {
        assert_eq!(
            decide(None, Wanted::DefaultYear),
            Decision::Resolve(Wanted::DefaultYear)
        );
    }

    // -------------------------------------------------- resolving it

    /// A two-second chain, a year of it, and the arithmetic that has to
    /// come out: 365 days is 15,768,000 blocks at 2 seconds.
    #[tokio::test]
    async fn the_default_is_one_year_of_history() {
        let spacing = 2;
        let height = 30_000_000u64;
        let genesis = 1_500_000_000;
        let chain = FakeChain::new(genesis, spacing, height);
        let now = genesis + spacing * (height as i64 - 1);

        let floor =
            resolve_floor(&chain, height, now, Wanted::DefaultYear)
                .await
                .unwrap();

        let expected_ts = now - DEFAULT_HISTORY;
        assert!(
            i64::from(floor.timestamp) >= expected_ts,
            "the floor is BEFORE the year it promises"
        );
        // And not more than one block early.
        assert!(
            i64::from(floor.timestamp) - expected_ts < spacing,
            "the floor is {} seconds too early",
            i64::from(floor.timestamp) - expected_ts
        );
        assert_eq!(floor.reason, Reason::DefaultYear);
        assert_eq!(floor.block, height - 1 - (365 * 86_400 / 2));
    }

    #[tokio::test]
    async fn the_head_is_the_last_block_the_source_has() {
        let chain = FakeChain::new(1_600_000_000, 12, 1_000);

        let floor =
            resolve_floor(&chain, 1_000, 1_700_000_000, Wanted::Head)
                .await
                .unwrap();

        assert_eq!(floor.block, 999);
        assert_eq!(floor.reason, Reason::Head);
        assert_eq!(floor.timestamp, 1_600_000_000 + 12 * 999);
    }

    /// A block number is taken as given - but the DATE is looked up, so
    /// every floor reads the same way afterwards.
    #[tokio::test]
    async fn a_start_block_keeps_its_number_and_gains_a_date() {
        let chain = FakeChain::new(1_600_000_000, 12, 1_000);

        let floor = resolve_floor(
            &chain,
            1_000,
            1_700_000_000,
            Wanted::Block(300),
        )
        .await
        .unwrap();

        assert_eq!(floor.block, 300);
        assert_eq!(floor.timestamp, 1_600_000_000 + 12 * 300);
        assert_eq!(floor.reason, Reason::StartBlock);
    }

    #[tokio::test]
    async fn a_start_date_lands_on_the_first_block_of_that_day() {
        let genesis = a_date("2024-01-01").midnight();
        // Tall enough to reach March: 12-second blocks for half a year.
        let height = 1_500_000u64;
        let chain = FakeChain::new(genesis, 12, height);

        let floor = resolve_floor(
            &chain,
            height,
            genesis + 12 * height as i64,
            Wanted::Date(a_date("2024-03-01")),
        )
        .await
        .unwrap();

        assert_eq!(floor.reason, Reason::StartDate);
        assert_eq!(floor.date(), "2024-03-01");
        assert!(
            i64::from(floor.timestamp) >= a_date("2024-03-01").midnight()
        );
        // The first block of that day, not the second: 60 days at 12
        // seconds is exactly 432,000 blocks after a midnight genesis.
        assert_eq!(floor.block, 432_000);
    }

    /// Solana: no headers to bisect, so the head is "now" and a named slot
    /// keeps its number.
    #[test]
    fn a_slot_floor_needs_no_source_at_all() {
        let now = 1_800_000_000;

        let head = slot_floor(400_000_000, now, Wanted::Head);
        assert_eq!(head.block, 399_999_999);
        assert_eq!(head.timestamp, now as u32);
        assert_eq!(head.reason, Reason::Head);

        let named =
            slot_floor(400_000_000, now, Wanted::Block(391_000_000));
        assert_eq!(named.block, 391_000_000);
        assert_eq!(named.reason, Reason::StartBlock);
        // The slot is exact; only the DATE is unknown.
        assert_eq!(named.date(), "unknown");
    }

    /// A chain younger than a year: the floor is its own beginning, not an
    /// error and not the head.
    #[tokio::test]
    async fn a_chain_younger_than_a_year_starts_at_its_genesis() {
        let genesis = 1_700_000_000;
        let chain = FakeChain::new(genesis, 12, 1_000);

        let floor = resolve_floor(
            &chain,
            1_000,
            genesis + 10_000,
            Wanted::DefaultYear,
        )
        .await
        .unwrap();

        assert_eq!(floor.block, 0);
        assert_eq!(floor.timestamp, genesis as u32);
    }
}
