//! `sol_dex_programs`: the curated answer to "is this program a market?".
//!
//! # Why a table and not a constant
//!
//! The prop / "dark" AMMs - HumidiFi, Tessera, Scorch, QuantumAMM, GoonFi,
//! AlphaQ, Deriverse, SolFi V2, BisonFi and the rest - are together about
//! **32% of Solana DEX volume** (the Solana venue research §1.3) and publish
//! no IDL and, mostly, no event. The movement layer already decodes them
//! perfectly: it reads real SPL transfers, so the amounts, mints, price and
//! trader of any of them are exact.
//!
//! What is NOT a decoding problem is the judgement. The generic rule
//! identifies token movement precisely and "this was a trade on a market"
//! only probabilistically (§3.2, last row): staking, lending and NFT sales
//! move two mints across one counterparty too. **Promoting a program to a
//! VENUE is therefore a false-positive decision, and a false positive here
//! is a fabricated market on somebody's screen.**
//!
//! A decision like that belongs to an operator, with a stated confidence
//! and a stated source, revisable without a release - the same rule
//! `quote_tokens`, `dex_trusted_emitters` and `launchpad_trusted_emitters`
//! already follow. **Migrations seed nothing** (docs/design.md §5, and a
//! lint in `db::migrate` rejects an `INSERT` in a migration); the module
//! README ships the verified program ids as ready-to-run `INSERT`s.
//!
//! # What a row changes, and what it deliberately does not
//!
//! A row sets the `protocol` NAME a swap of that program is stored under,
//! through [`ProgramNames`]. It does **not** add the program to the
//! streaming query: that is still one line in [`crate::svm::programs::VENUES`],
//! because a streamed program costs bandwidth on every slot forever and
//! that is a different decision from naming one. An unlisted program keeps
//! the built-in `Venue` name, so the table can only ever ADD knowledge -
//! there is no configuration that makes the decoder worse.

use std::collections::HashMap;

use clickhouse::Row;
use serde::{Deserialize, Serialize};

use crate::svm::models::Pubkey;

/// What an operator decided a program is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProgramKind {
    /// A market whose swaps are real volume.
    Venue,
    /// An aggregator. ATTRIBUTION ONLY - 40% of Solana DEX volume is
    /// routed, so counting a router's instruction as a trade would double
    /// count almost half the chain.
    Router,
    /// A proprietary market maker: a real market, but with no IDL and
    /// usually no event, so only the movement layer can see it.
    PropAmm,
    /// An app or bot that routes into somebody else's venue. Its volume
    /// PARTITIONS a venue's and is never added to it.
    Frontend,
}

impl ProgramKind {
    pub const ALL: [ProgramKind; 4] = [
        ProgramKind::Venue,
        ProgramKind::Router,
        ProgramKind::PropAmm,
        ProgramKind::Frontend,
    ];

    pub const fn as_str(&self) -> &'static str {
        match self {
            ProgramKind::Venue => "venue",
            ProgramKind::Router => "router",
            ProgramKind::PropAmm => "prop_amm",
            ProgramKind::Frontend => "frontend",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        ProgramKind::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    /// Does a swap of this program count as VENUE volume?
    ///
    /// A router's and a front end's do not, and saying so in one place is
    /// what stops the double counting.
    pub const fn is_volume(&self) -> bool {
        matches!(self, ProgramKind::Venue | ProgramKind::PropAmm)
    }
}

impl std::fmt::Display for ProgramKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A `sol_dex_programs` row. OPERATOR DATA: the indexer reads it and never
/// writes it.
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct SolDexProgram {
    pub program_id: Pubkey,
    /// The value the `protocol` column of a swap gets.
    pub name: String,
    /// `venue` | `router` | `prop_amm` | `frontend`.
    pub kind: String,
    /// How sure the operator is, 0..=100. A low number is not a reason to
    /// hide the row - it is a reason for a screen to say so.
    pub confidence: u8,
    /// Where the claim comes from: a URL, an IDL, "observed live", a Dune
    /// query. Free text, and the whole point of the column is that a
    /// judgement without a provenance is not reviewable.
    pub source: String,
    pub _version: u64,
}

/// `program -> protocol name`, as the decoder consults it.
///
/// Empty by default, which is the safe state: every streamed program
/// already has a built-in name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProgramNames {
    names: HashMap<Pubkey, String>,
    kinds: HashMap<Pubkey, ProgramKind>,
}

impl ProgramNames {
    pub fn new(rows: impl IntoIterator<Item = SolDexProgram>) -> Self {
        let mut names = HashMap::new();
        let mut kinds = HashMap::new();
        for row in rows {
            // A row with no name would blank the `protocol` column, which
            // is strictly worse than the built-in default.
            if row.name.trim().is_empty() {
                continue;
            }
            if let Some(kind) = ProgramKind::parse(&row.kind) {
                kinds.insert(row.program_id, kind);
            }
            names.insert(row.program_id, row.name);
        }
        Self { names, kinds }
    }

    /// What the operator said this program IS, when they said anything.
    pub fn kind(&self, program: &Pubkey) -> Option<ProgramKind> {
        self.kinds.get(program).copied()
    }

    /// Does a swap of `program` count as venue volume?
    ///
    /// An operator's row can say "router" or "frontend" about a program
    /// this module STREAMS, and the honest answer to that is a loud
    /// contradiction rather than a silent subtraction: which programs are
    /// streamed as venues is a code-level decision
    /// ([`crate::svm::programs::VENUES`]), and a row in a table must not be
    /// able to make a venue's volume vanish without anyone noticing. So
    /// this answers the question and [`contradictions`] is what reports
    /// the disagreement; nothing filters rows on it.
    pub fn counts_as_volume(&self, program: &Pubkey) -> bool {
        self.kind(program).map(|kind| kind.is_volume()).unwrap_or(true)
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// The `protocol` a swap of `program` is stored under: the operator's
    /// name if there is one, and `fallback` - the built-in `Venue` name -
    /// otherwise. Never empty.
    pub fn protocol<'a>(
        &'a self,
        program: &Pubkey,
        fallback: &'a str,
    ) -> &'a str {
        self.names.get(program).map(String::as_str).unwrap_or(fallback)
    }
}

/// Reads the whole table. Tiny by construction - a few dozen rows - so it
/// is loaded once at startup and held.
pub const LOAD_SQL: &str = "\
SELECT program_id, name, kind, confidence, source, _version \
FROM sol_dex_programs FINAL \
ORDER BY program_id";

/// Programs the module streams that an operator has NOT described yet.
/// Diagnostics: it is the list a reviewer works through.
pub fn undescribed(names: &ProgramNames) -> Vec<&'static str> {
    crate::svm::programs::VENUES
        .iter()
        .filter(|venue| {
            !names.names.contains_key(&crate::svm::programs::pubkey(
                venue.program_b58(),
            ))
        })
        .map(|venue| venue.as_str())
        .collect()
}

/// Streamed venues an operator has marked as something whose swaps are NOT
/// venue volume - a router or a front end.
///
/// The two statements cannot both be right: the program is in
/// [`crate::svm::programs::VENUES`], so every swap of it is already being
/// written as venue volume, and the row says it should not be. This is the
/// list a reviewer works through, exactly like [`undescribed`]; the
/// decoder does not act on it, because a table row must not be able to
/// delete a venue's volume silently. Resolving it is either an edit to the
/// row or an edit to `VENUES`, and both are decisions a person makes.
pub fn contradictions(names: &ProgramNames) -> Vec<&'static str> {
    crate::svm::programs::VENUES
        .iter()
        .filter(|venue| {
            !names.counts_as_volume(&crate::svm::programs::pubkey(
                venue.program_b58(),
            ))
        })
        .map(|venue| venue.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::svm::programs::{pubkey, Venue};

    fn row(program: &str, name: &str, kind: ProgramKind) -> SolDexProgram {
        SolDexProgram {
            program_id: pubkey(program),
            name: name.to_owned(),
            kind: kind.as_str().to_owned(),
            confidence: 90,
            source: "unit test".to_owned(),
            _version: 1,
        }
    }

    #[test]
    fn kinds_round_trip_through_their_column_value() {
        for kind in ProgramKind::ALL {
            assert_eq!(ProgramKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ProgramKind::parse("dex"), None);
    }

    /// Routers and front ends are attribution, never volume. That rule
    /// lives in exactly one place so it cannot be applied inconsistently.
    #[test]
    fn only_markets_count_as_volume() {
        assert!(ProgramKind::Venue.is_volume());
        assert!(ProgramKind::PropAmm.is_volume());
        assert!(!ProgramKind::Router.is_volume());
        assert!(!ProgramKind::Frontend.is_volume());
    }

    /// The `kind` column was read out of the table and then thrown away:
    /// nothing anywhere consulted it, so a program an operator called a
    /// router got a nicer `protocol` string and its swaps were counted
    /// just the same, with nothing saying so.
    ///
    /// It is kept now, and the one case where it contradicts the code -
    /// a STREAMED venue an operator says is not a market - is reported.
    /// It is deliberately not acted on: `VENUES` is a code-level decision
    /// and a table row must not be able to make a venue's volume vanish
    /// silently.
    #[test]
    fn an_operator_calling_a_streamed_venue_a_router_is_reported() {
        let names = ProgramNames::new([
            row(
                Venue::PumpSwap.program_b58(),
                "pumpswap",
                ProgramKind::Router,
            ),
            row(
                Venue::OrcaWhirlpool.program_b58(),
                "orca",
                ProgramKind::Venue,
            ),
        ]);

        assert_eq!(
            names.kind(&pubkey(Venue::PumpSwap.program_b58())),
            Some(ProgramKind::Router)
        );
        assert!(!names
            .counts_as_volume(&pubkey(Venue::PumpSwap.program_b58())));
        assert!(names.counts_as_volume(&pubkey(
            Venue::OrcaWhirlpool.program_b58()
        )));
        // An unlisted program is a market until somebody says otherwise.
        assert!(names
            .counts_as_volume(&pubkey(Venue::RaydiumAmmV4.program_b58())));

        assert_eq!(contradictions(&names), vec!["pumpswap"]);
        // A fresh database contradicts nothing.
        assert!(contradictions(&ProgramNames::default()).is_empty());
    }

    /// An empty registry - the default, and what a fresh database has -
    /// must leave every name exactly as the code already had it.
    #[test]
    fn an_empty_registry_changes_nothing() {
        let names = ProgramNames::default();
        assert!(names.is_empty());
        for venue in Venue::ALL {
            let program = pubkey(venue.program_b58());
            assert_eq!(
                names.protocol(&program, venue.as_str()),
                venue.as_str()
            );
        }
        assert_eq!(undescribed(&names).len(), Venue::ALL.len() - 1);
    }

    #[test]
    fn an_operator_name_wins_over_the_built_in_one() {
        let humidifi = "9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp";
        let names = ProgramNames::new([
            row(humidifi, "humidifi", ProgramKind::PropAmm),
            row(
                Venue::BisonFi.program_b58(),
                "bisonfi_v2",
                ProgramKind::PropAmm,
            ),
        ]);

        assert_eq!(names.len(), 2);
        assert_eq!(
            names.protocol(&pubkey(humidifi), "unknown"),
            "humidifi"
        );
        assert_eq!(
            names.protocol(
                &pubkey(Venue::BisonFi.program_b58()),
                Venue::BisonFi.as_str()
            ),
            "bisonfi_v2"
        );
        // And an unlisted program keeps its built-in name.
        assert_eq!(
            names.protocol(
                &pubkey(Venue::PumpSwap.program_b58()),
                Venue::PumpSwap.as_str()
            ),
            Venue::PumpSwap.as_str()
        );
    }

    /// A blank name must not blank the column: the built-in default is
    /// always better than nothing.
    #[test]
    fn a_blank_name_is_ignored_rather_than_stored() {
        let names = ProgramNames::new([row(
            Venue::PumpSwap.program_b58(),
            "   ",
            ProgramKind::Venue,
        )]);
        assert!(names.is_empty());
        assert_eq!(
            names.protocol(
                &pubkey(Venue::PumpSwap.program_b58()),
                Venue::PumpSwap.as_str()
            ),
            Venue::PumpSwap.as_str()
        );
    }

    /// The README ships the rows this table is meant to hold, because a
    /// migration may not. That only helps while the two agree, so: every
    /// program the module streams, and every trusted launchpad emitter,
    /// must appear there in base58.
    ///
    /// The statements themselves were run against a real ClickHouse 25.12
    /// with the full migration set; this keeps them from drifting.
    #[test]
    fn the_readme_ships_an_insert_for_every_streamed_program() {
        const README: &str = include_str!("README.md");

        for venue in crate::svm::programs::VENUES {
            assert!(
                README.contains(venue.program_b58()),
                "{} is streamed but the README's sol_dex_programs INSERT \
                 does not list it",
                venue.as_str()
            );
        }
        for family in crate::svm::launchpads::SolFamily::ALL {
            assert!(
                README.contains(family.venue().program_b58()),
                "{family} has no launchpad_trusted_emitters row in the \
                 README"
            );
            assert!(
                README.contains(family.as_str()),
                "{family} is not named in the README"
            );
        }
        // The one form that keeps all 32 bytes AND round trips: a bare
        // `base58Decode` yields a String, and the column is a
        // FixedString(32).
        assert!(
            README.contains("toFixedString(base58Decode("),
            "the README's INSERTs must build a FixedString(32)"
        );
    }

    #[test]
    fn the_load_query_names_every_column_of_the_row() {
        for column in SolDexProgram::COLUMN_NAMES {
            assert!(
                LOAD_SQL.contains(column),
                "{column} is missing from LOAD_SQL"
            );
        }
        assert!(LOAD_SQL.contains("FINAL"));
    }
}
