//! Solana (SVM) data module: `sol_*` core tables and the DEX decoders that
//! feed the chain-neutral analytics shape.
//!
//! See `README.md` in this directory for what this module deliberately does
//! NOT do. The short version, because it matters and is easy to forget:
//! **this is an analytics-only, program-filtered pipeline.** It is not a
//! Solana block explorer backend. There is no wallet history, no chain-wide
//! transfer table and no block statistics, and there must never be one built
//! on these tables, because they would be a subset of the chain presented as
//! the chain.
//!
//! Layout follows docs/design.md section 12, like every other data module:
//! `models.rs` (row structs), `programs.rs` (the registry), `decode.rs` (the
//! pure generic decoder), `events.rs` (per-program decoders), `pda.rs`
//! (on-curve test), `fixtures.rs`, `integration_tests.rs`.

pub mod decode;
pub mod events;
pub mod fixtures;
pub mod models;
pub mod pda;
pub mod programs;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod integration_tests;

/// Ignored, network bound proof against mainnet and the public Solana RPC.
#[cfg(test)]
mod live_tests;

use decode::{Diagnostics, SvmTransaction};
use models::{SolSlot, SolToken, SolTransaction, SvmSwap};

/// Block scoped `sol_*` tables, in the order a purge must tombstone them:
/// children first, the commit marker LAST. Same rule as
/// `db::BASE_TABLES` - while the old `sol_slots` row is alive a crashed
/// purge is re-detected and re-run, which is harmless.
///
/// `sol_tokens` is NOT here: a mint's decimals are chain state, not part of
/// a slot, exactly like the EVM `tokens` table.
pub const BASE_TABLES: &[&str] =
    &["sol_dex_swaps", "sol_transactions", "sol_slots"];

/// Read-path side tables. None yet: phase 1 writes the base tables and the
/// candle query in the README reads them with `FINAL`.
pub const SIDE_TABLES: &[&str] = &[];

/// Rows one batch of slots produced.
#[derive(Debug, Default)]
pub struct SvmRows {
    pub slots: Vec<SolSlot>,
    pub transactions: Vec<SolTransaction>,
    pub swaps: Vec<SvmSwap>,
    pub tokens: Vec<SolToken>,
    pub diagnostics: Diagnostics,
}

impl SvmRows {
    pub fn rows(&self) -> usize {
        self.slots.len()
            + self.transactions.len()
            + self.swaps.len()
            + self.tokens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows() == 0
    }

    pub fn append(&mut self, other: &mut SvmRows) {
        self.slots.append(&mut other.slots);
        self.transactions.append(&mut other.transactions);
        self.swaps.append(&mut other.swaps);
        self.tokens.append(&mut other.tokens);
        self.diagnostics.merge(&other.diagnostics);
    }

    /// Stamps `_version` on every block scoped row, once per flush
    /// (docs/design.md section 2).
    pub fn set_version(&mut self, version: u64) {
        for row in &mut self.slots {
            row._version = version;
        }
        for row in &mut self.transactions {
            row._version = version;
        }
        for row in &mut self.swaps {
            row._version = version;
        }
        for row in &mut self.tokens {
            row._version = version;
        }
    }

    /// Stamps the chain's purge generation on every block scoped row.
    /// `sol_tokens` is chain state and carries no epoch.
    pub fn set_epoch(&mut self, epoch: u32) {
        for row in &mut self.slots {
            row.epoch = epoch;
        }
        for row in &mut self.transactions {
            row.epoch = epoch;
        }
        for row in &mut self.swaps {
            row.epoch = epoch;
        }
    }
}

/// One slot's worth of decoded input: the block header and its matched
/// transactions.
#[derive(Debug, Default)]
pub struct SvmSlotBatch {
    pub slot: u64,
    pub blockhash: models::Pubkey,
    pub parent_slot: u64,
    pub parent_blockhash: models::Pubkey,
    pub block_height: u64,
    pub timestamp: u32,
    pub transactions: Vec<SvmTransaction>,
}

/// Turns streamed slots into rows. Pure: no I/O, no clock, no RPC.
pub fn decode(chain: u64, batches: &[SvmSlotBatch]) -> SvmRows {
    let mut rows = SvmRows::default();
    let mut seen_mints: std::collections::HashMap<
        models::Pubkey,
        (u8, models::Pubkey),
    > = std::collections::HashMap::new();

    for batch in batches {
        rows.slots.push(SolSlot {
            chain,
            block_number: batch.slot,
            blockhash: batch.blockhash,
            parent_slot: batch.parent_slot,
            parent_blockhash: batch.parent_blockhash,
            block_height: batch.block_height,
            timestamp: batch.timestamp,
            epoch: 0,
            _version: 0,
            is_deleted: 0,
        });

        for tx in &batch.transactions {
            // Decimals arrive free on every account_activity token row, so
            // unlike EVM a Solana swap can be valued with no external call
            // at all (research section 6.5).
            for row in &tx.activity {
                if let (Some(mint), Some(decimals)) =
                    (row.mint, row.decimals)
                {
                    seen_mints.entry(mint).or_insert((
                        decimals,
                        row.token_program.unwrap_or(models::ZERO_PUBKEY),
                    ));
                }
            }

            let outcome =
                decode::decode_transaction(chain, batch.timestamp, tx);
            if outcome.swaps.is_empty() {
                rows.diagnostics.merge(&outcome.diagnostics);
                continue;
            }

            rows.transactions.push(SolTransaction {
                chain,
                block_number: batch.slot,
                tx_index: tx.tx_index,
                signature: tx.signature,
                fee_payer: tx.fee_payer,
                success: tx.success,
                fee: tx.fee,
                compute_units: tx.compute_units,
                dropped_logs: tx.dropped_logs,
                timestamp: batch.timestamp,
                epoch: 0,
                _version: 0,
                is_deleted: 0,
            });
            rows.diagnostics.merge(&outcome.diagnostics);
            rows.swaps.extend(outcome.swaps);
        }
    }

    // Only mints that actually appear in a swap are worth a row: the point
    // of the table is valuing swaps, not cataloguing the chain.
    let traded: std::collections::HashSet<models::Pubkey> = rows
        .swaps
        .iter()
        .flat_map(|swap| [swap.token0, swap.token1])
        .collect();
    rows.tokens = seen_mints
        .into_iter()
        .filter(|(mint, _)| traded.contains(mint))
        .map(|(mint, (decimals, program))| SolToken {
            chain,
            mint,
            decimals,
            program,
            _version: 0,
            is_deleted: 0,
        })
        .collect();
    rows.tokens.sort_by_key(|token| token.mint);

    rows
}

/// Registers Solana in the `chains` registry (migration 0006). Run once at
/// startup by `indexer run --chain solana`; idempotent (`chains` is a
/// ReplacingMergeTree keyed by `chain`). Migrations carry no seed rows.
pub const REGISTER_CHAIN_SQL: &str =
    "INSERT INTO chains (chain, name, family) VALUES (1399811149, 'solana', 'svm')";
