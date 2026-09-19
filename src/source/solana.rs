//! Envio Solana HyperSync: the only place that talks to the Solana source.
//!
//! Shaped like `source/mod.rs` (the EVM source) but it is a SEPARATE client
//! crate with a different query language, so it does not share code with it.
//! The seam the pipeline sees is [`SolanaSource::stream`] /
//! [`SolanaSource::head`] / [`SolanaSource::headers`].
//!
//! # Three things about this source that the EVM one does not have
//!
//! **1. A matched instruction does NOT return its children.** Probed live:
//! filtering on one BisonFi inner instruction returns that one instruction
//! row, the parent transaction, all of the transaction's log rows and all of
//! its `account_activity` rows - but NOT the child SPL transfers. The
//! movement decoder needs those transfers, so the SPL Token, Token-2022 and
//! System transfer instructions are extra objects in the SAME
//! `instruction_calls` array. Objects in one array are OR-ed; separate
//! arrays are INTERSECTED, so this union cannot be expressed any other way.
//!
//! **2. Responses are capped unless `max_num_instructions` is set.** Without
//! it a response stops after roughly 700 rows, i.e. one or two slots.
//!
//! **3. There is no live-tail mode and no reorg handling.** At the head
//! `stream_arrow` fails with "server made no progress at slot N". So the
//! range this source streams is always bounded and the head follower is
//! ours, exactly as it is for EVM.

use anyhow::{bail, Context, Result};
use hypersync_client_solana::{
    config::{ClientConfig, StreamConfig},
    simple_types::SolanaResponse,
    Client,
};
use hypersync_solana_net_types::{
    field_selection::{
        AccountActivityField, BlockField, InstructionField, LogField,
        SolanaFieldSelection, TransactionField,
    },
    query::{
        AccountActivitySelection, InstructionSelection, LogSelection,
        SolanaQuery, TransactionSelection,
    },
    types::LogKind,
    Address,
};
use log::info;
use std::sync::Arc;
use tokio::sync::mpsc::{self, Receiver};

use crate::svm::{
    decode::{
        SvmAccountActivity, SvmInstruction, SvmLog, SvmTransaction,
    },
    models::{Pubkey, SigBytes},
    programs::{
        registry, Venue, IX_SYSTEM_TRANSFER, IX_TRANSFER,
        IX_TRANSFER_CHECKED, VENUES,
    },
    SvmSlotBatch,
};

/// Public endpoint.
pub const DEFAULT_URL: &str = "https://solana.hypersync.xyz";

/// Row cap put on EVERY table of a response.
///
/// # Why every table, and not just the instructions
///
/// `SolanaQuery` has FIVE independent `max_num_*` caps - blocks,
/// transactions, instructions, logs and account activity - and each one
/// stops the whole response when it is hit. Raising one of them raises
/// nothing: the lowest UNSET cap simply binds first.
///
/// That is not a theoretical point. Measured on the production query shape
/// (docs/solana-research.md section 11.1.1): with only
/// `max_num_instructions` raised, a request returned **one slot**. With all
/// five raised, the same request returned **40 slots**. At 30 queries a
/// minute, one slot per query is 0.5 slots/s against a chain that produces
/// 3.76 - the pipeline could not have followed the head at all.
///
/// With the caps out of the way the binding limit becomes the server's own
/// ~5 s query execution budget, which lands at 30-40 slots. That number is
/// NOT a constant to rely on: the client follows `next_slot` and never its
/// own arithmetic.
const MAX_RESPONSE_ROWS: usize = 1_000_000;

/// Raises every response cap. See [`MAX_RESPONSE_ROWS`].
///
/// Written as a function over the whole query, rather than as five literals
/// at each call site, so that a cap added by a future version of the crate
/// is missed in exactly one place - which is what
/// `every_response_cap_is_raised` then catches.
fn raise_response_caps(query: &mut SolanaQuery) {
    query.max_num_blocks = Some(MAX_RESPONSE_ROWS);
    query.max_num_transactions = Some(MAX_RESPONSE_ROWS);
    query.max_num_instructions = Some(MAX_RESPONSE_ROWS);
    query.max_num_logs = Some(MAX_RESPONSE_ROWS);
    query.max_num_account_activity = Some(MAX_RESPONSE_ROWS);
}

/// The same trap, one layer down in the client.
///
/// `StreamConfig` auto-tunes `batch_size` to keep a response between
/// `response_bytes_floor` and `response_bytes_ceiling`, and its defaults are
/// 250 KB and 500 KB. Measured Arrow density on this query shape is ~0.5 MB
/// PER SLOT, so the auto-tuner converges on a batch of one slot and quietly
/// recreates the one-slot-per-query problem after the query caps are fixed.
/// Both thresholds are therefore raised to tens of megabytes.
fn stream_config() -> StreamConfig {
    StreamConfig {
        response_bytes_ceiling: 64 * 1024 * 1024,
        response_bytes_floor: 16 * 1024 * 1024,
        ..Default::default()
    }
}

// Field selection (docs/design.md section 8): exactly what a column stores
// or a decoder reads, nothing else.

const BLOCK_FIELDS: [BlockField; 6] = [
    BlockField::Slot,
    BlockField::Blockhash,
    // Contiguity on Solana is the parent_slot chain, NEVER slot + 1:
    // skipped slots are normal and a missing integer is not a gap.
    BlockField::ParentSlot,
    BlockField::ParentBlockhash,
    BlockField::BlockTime,
    BlockField::BlockHeight,
];

const TRANSACTION_FIELDS: [TransactionField; 8] = [
    TransactionField::Slot,
    TransactionField::TransactionIndex,
    TransactionField::TransactionId,
    TransactionField::FeePayer,
    TransactionField::Success,
    TransactionField::Fee,
    TransactionField::ComputeUnitsConsumed,
    TransactionField::HasDroppedLogMessages,
];

const INSTRUCTION_FIELDS: [InstructionField; 6] = [
    InstructionField::Slot,
    InstructionField::TransactionIndex,
    // The full CPI tree path: depth, parentage and sibling order in one
    // value. This is what makes the subtree rule a prefix test rather than
    // a heuristic, and what the position `ordinal` packs.
    InstructionField::InstructionAddress,
    InstructionField::ExecutingAccount,
    InstructionField::AccountArguments,
    InstructionField::Data,
];

const ACCOUNT_ACTIVITY_FIELDS: [AccountActivityField; 12] = [
    AccountActivityField::Slot,
    AccountActivityField::TransactionIndex,
    AccountActivityField::Account,
    AccountActivityField::Mint,
    AccountActivityField::PreOwner,
    AccountActivityField::PostOwner,
    // Decimals come free on every token row: a Solana swap needs no RPC
    // call to be valued, which is strictly better than EVM.
    AccountActivityField::TokenDecimals,
    AccountActivityField::PreTokenBalance,
    AccountActivityField::PostTokenBalance,
    // The native side, which is the only trace a bonding curve's SOL leg
    // leaves: it decrements its own lamports with no instruction at all.
    AccountActivityField::PreBalance,
    AccountActivityField::PostBalance,
    AccountActivityField::IsFeePayer,
];

/// Raydium's and Orca's swap events are `Program data:` / `ray_log:` LOG
/// lines, so this table is not optional for them.
///
/// `InstructionAddress` is the important one: it names the instruction that
/// wrote the line, which is what lets a log be attributed to exactly one
/// instruction subtree, the same way an instruction is. Without it a log
/// could only be attributed to a transaction, and a transaction holding two
/// swaps on one pool is precisely the case this module exists to get right.
/// `Kind` is what separates a `Program data:` line (an Anchor `emit!`
/// event, base64) from a `Program log:` one (free text, which is where
/// Raydium v4's `ray_log:` lives). The server stores both with the prefix
/// stripped, so without the kind the two are indistinguishable.
const LOG_FIELDS: [LogField; 6] = [
    LogField::Slot,
    LogField::TransactionIndex,
    LogField::InstructionAddress,
    LogField::ProgramId,
    LogField::Kind,
    LogField::Message,
];

fn field_selection() -> SolanaFieldSelection {
    SolanaFieldSelection {
        block: BLOCK_FIELDS.to_vec(),
        transaction: TRANSACTION_FIELDS.to_vec(),
        instruction_call: INSTRUCTION_FIELDS.to_vec(),
        log: LOG_FIELDS.to_vec(),
        account_activity: ACCOUNT_ACTIVITY_FIELDS.to_vec(),
        reward: Vec::new(),
    }
}

fn address(key: &Pubkey) -> Address {
    Address(*key)
}

/// The instruction selections: every streamed venue, plus the token and
/// System transfers the movement decoder needs.
///
/// All of them go in ONE array. Objects in the same array are OR-ed;
/// different arrays are INTERSECTED, so putting the transfers anywhere else
/// would return the intersection (nothing) instead of the union.
pub fn instruction_selections(
    venues: &[Venue],
) -> Vec<InstructionSelection> {
    let registry = registry();
    let mut selections: Vec<InstructionSelection> = venues
        .iter()
        .map(|venue| InstructionSelection {
            executing_account: vec![address(
                &crate::svm::programs::pubkey(venue.program_b58()),
            )],
            tx_success: Some(true),
            ..Default::default()
        })
        .collect();

    // SPL Token and Token-2022 `Transfer` (0x03) and `TransferChecked`
    // (0x0c). Measured cost: ~494 transfer rows per slot for the whole
    // chain, about 1.4x the DEX program row volume. That is the single
    // decision that makes the generic decoder possible.
    for program in [registry.spl_token, registry.token_2022] {
        for tag in [IX_TRANSFER, IX_TRANSFER_CHECKED] {
            selections.push(InstructionSelection {
                executing_account: vec![address(&program)],
                d1: vec![hex::encode([tag])],
                tx_success: Some(true),
                ..Default::default()
            });
        }
    }

    // System `Transfer`, for venues whose SOL leg is a real instruction
    // (wrapping, and curves that pay out through the System program).
    selections.push(InstructionSelection {
        executing_account: vec![address(&registry.system)],
        d4: vec![hex::encode(IX_SYSTEM_TRANSFER)],
        tx_success: Some(true),
        ..Default::default()
    });

    selections
}

/// The query for `[from_slot, to_slot)`.
pub fn build_query(from_slot: u64, to_slot: u64) -> SolanaQuery {
    let mut query = SolanaQuery {
        from_slot,
        to_slot: Some(to_slot),
        instruction_calls: instruction_selections(&VENUES),
        // An empty selection matches every row, which is what the decoder
        // wants: owners, mints, decimals and the native side for every
        // account of a matched transaction.
        account_activity: vec![AccountActivitySelection::default()],
        // Likewise match-all, and for a reason phase 1 did not have: half
        // the phase 2 venues publish their swap event as a LOG LINE rather
        // than a self-CPI instruction. Raydium's three programs and Orca
        // use Anchor's `emit!` (or a bare `msg!`), so without this table
        // their exact fees and pool state are simply unreachable.
        //
        // It has to be match-all rather than filtered to those four
        // programs: selections in DIFFERENT arrays are INTERSECTED, so a
        // `program_id` filter here would restrict the response to
        // transactions that ALSO carry one of those logs, dropping every
        // PumpSwap and pump.fun transaction on the chain.
        logs: vec![LogSelection::default()],
        // Slots with no match still need their header: it is the commit
        // marker and it carries the parent_slot chain.
        include_all_blocks: true,
        field_selection: field_selection(),
        ..Default::default()
    };
    raise_response_caps(&mut query);
    query
}

/// Headers only, for the fork-point search and the contiguity sweep.
///
/// Measured ~300x cheaper per slot than the data query - one request
/// returned 10,000 slots - because nothing but the block table is touched.
pub fn build_header_query(from_slot: u64, to_slot: u64) -> SolanaQuery {
    let mut query = SolanaQuery {
        from_slot,
        to_slot: Some(to_slot),
        include_all_blocks: true,
        field_selection: SolanaFieldSelection {
            block: BLOCK_FIELDS.to_vec(),
            ..Default::default()
        },
        ..Default::default()
    };
    raise_response_caps(&mut query);
    query
}

/// One slot header, for the `parent_slot` continuity check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotHeader {
    pub slot: u64,
    pub blockhash: Pubkey,
    pub parent_slot: u64,
    pub parent_blockhash: Pubkey,
    pub timestamp: u32,
}

/// A page of streamed slots plus the cursor and the server's reorg guard.
#[derive(Debug, Default)]
pub struct SolanaBatch {
    pub next_slot: u64,
    pub batches: Vec<SvmSlotBatch>,
    /// The server's in-memory head window. It describes the WINDOW, not the
    /// response's slots, and guards from different pages must not be
    /// compared with each other.
    pub rollback_guard: Option<RollbackGuard>,
}

/// The server's head window, in the same shape the EVM guard has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RollbackGuard {
    pub slot_number: u64,
    pub blockhash: Pubkey,
    pub first_slot_number: u64,
    pub first_previous_blockhash: Pubkey,
    pub timestamp: i64,
}

#[derive(Clone)]
pub struct SolanaSource {
    client: Arc<Client>,
}

impl SolanaSource {
    pub fn new(url: Option<&str>, api_token: &str) -> Result<Self> {
        if api_token.trim().is_empty() {
            bail!(
                "Solana HyperSync needs an API token: POST /query answers \
                 401 without one (only GET /height is open)"
            );
        }
        let url = url.unwrap_or(DEFAULT_URL).to_owned();
        let client = Client::new(ClientConfig {
            url: url.clone(),
            bearer_token: Some(api_token.to_owned()),
            ..Default::default()
        })
        .context("build Solana HyperSync client")?;

        info!("Using Solana HyperSync endpoint {url}.");
        Ok(Self { client: Arc::new(client) })
    }

    /// The server's head slot.
    ///
    /// Measured 4-12 slots behind the RPC's `finalized`, i.e. the server
    /// appears to serve at about finalized. That is NOT documented and must
    /// not be relied on: the tombstone and epoch machinery stays in place
    /// exactly as it is for EVM.
    pub async fn head(&self) -> Result<u64> {
        self.client
            .get_height()
            .await
            .context("get Solana HyperSync height")
    }

    /// Headers of `[from, to)` as the server has them now.
    ///
    /// Slots it does not have are simply absent, and that is NORMAL: a
    /// skipped slot has no block. The caller checks continuity with the
    /// `parent_slot` / `parent_blockhash` chain, never with `slot + 1`.
    pub async fn headers(
        &self,
        from: u64,
        to: u64,
    ) -> Result<Vec<SlotHeader>> {
        let mut headers = Vec::new();
        let mut cursor = from;

        while cursor < to {
            let response = self
                .client
                .get(&build_header_query(cursor, to))
                .await
                .with_context(|| {
                    format!("get Solana headers [{cursor}, {to})")
                })?;

            for block in &response.blocks {
                let (Some(slot), Some(blockhash)) =
                    (block.slot, block.blockhash)
                else {
                    bail!(
                        "a Solana header came back without slot or hash"
                    );
                };
                headers.push(SlotHeader {
                    slot,
                    blockhash: blockhash.0,
                    parent_slot: block.parent_slot.unwrap_or_default(),
                    parent_blockhash: block
                        .parent_blockhash
                        .map(|hash| hash.0)
                        .unwrap_or_default(),
                    timestamp: block.block_time.unwrap_or_default().max(0)
                        as u32,
                });
            }

            // A range entirely below the served history returns empty with
            // next_slot NOT advancing. That is a stop condition, never a
            // reason to spin.
            if response.next_slot <= cursor {
                break;
            }
            cursor = response.next_slot;
        }

        headers.sort_unstable_by_key(|header| header.slot);
        headers.dedup_by_key(|header| header.slot);
        Ok(headers)
    }

    /// Fetches `[from, to)` in one request, following the server's
    /// truncation cursor. Returns the decoded batch and `next_slot`.
    pub async fn fetch(&self, from: u64, to: u64) -> Result<SolanaBatch> {
        let response =
            self.client.get(&build_query(from, to)).await.with_context(
                || format!("query Solana slots [{from}, {to})"),
            )?;
        Ok(to_batch(response))
    }

    /// Streams `[from, to)`. Always a BOUNDED range: the client has no
    /// live-tail mode and errors at the head, so following the head is the
    /// caller's job.
    pub async fn stream(
        &self,
        from: u64,
        to: u64,
    ) -> Result<Receiver<Result<SolanaBatch>>> {
        let client = self.client.clone();
        let (tx, rx) = mpsc::channel(1);

        tokio::spawn(async move {
            let mut responses =
                client.stream_arrow(build_query(from, to), stream_config());

            while let Some(response) = responses.recv().await {
                let message = match response {
                    Ok(arrow) => decode_arrow(arrow).map(to_batch),
                    Err(e) => Err(e.context(format!(
                        "stream Solana slots [{from}, {to})"
                    ))),
                };
                let failed = message.is_err();
                if tx.send(message).await.is_err() || failed {
                    return;
                }
            }
        });

        Ok(rx)
    }
}

/// Arrow -> typed rows. The client exposes the typed decode only on its own
/// `get`, so the streaming path reuses the same `from_arrow` helpers.
fn decode_arrow(
    arrow: hypersync_client_solana::types::QueryResponse,
) -> Result<SolanaResponse> {
    use hypersync_client_solana::from_arrow;

    let mut response = SolanaResponse {
        next_slot: arrow.next_slot,
        rollback_guard: arrow.rollback_guard,
        response_bytes: arrow.response_bytes,
        ..Default::default()
    };
    for (name, batch) in arrow.data.tables {
        match name {
            "blocks" => {
                response.blocks = from_arrow::blocks_from_arrow(&batch)
                    .context("decode blocks")?
            }
            "transactions" => {
                response.transactions =
                    from_arrow::transactions_from_arrow(&batch)
                        .context("decode transactions")?
            }
            "instruction_calls" => {
                response.instruction_calls =
                    from_arrow::instruction_calls_from_arrow(&batch)
                        .context("decode instruction_calls")?
            }
            "account_activity" => {
                response.account_activity =
                    from_arrow::account_activity_from_arrow(&batch)
                        .context("decode account_activity")?
            }
            _ => {}
        }
    }
    Ok(response)
}

/// Groups a response's flat rows into per-slot, per-transaction batches.
pub fn to_batch(response: SolanaResponse) -> SolanaBatch {
    use std::collections::HashMap;

    // (slot, tx_index) -> transaction under construction.
    let mut transactions: HashMap<(u64, u32), SvmTransaction> =
        HashMap::new();

    for tx in &response.transactions {
        let (Some(slot), Some(tx_index)) = (tx.slot, tx.transaction_index)
        else {
            continue;
        };
        transactions.insert(
            (slot, tx_index),
            SvmTransaction {
                slot,
                tx_index,
                signature: tx
                    .transaction_id
                    .map(|id| id.0)
                    .unwrap_or([0u8; 64]),
                fee_payer: tx
                    .fee_payer
                    .map(|key| key.0)
                    .unwrap_or_default(),
                // `None` means unknown, never success.
                success: tx.success.unwrap_or(false),
                fee: tx.fee.unwrap_or_default(),
                compute_units: tx
                    .compute_units_consumed
                    .unwrap_or_default(),
                dropped_logs: tx.has_dropped_log_messages.unwrap_or(false),
                instructions: Vec::new(),
                activity: Vec::new(),
                logs: Vec::new(),
            },
        );
    }

    for instruction in &response.instruction_calls {
        let (Some(slot), Some(tx_index)) =
            (instruction.slot, instruction.transaction_index)
        else {
            continue;
        };
        let Some(entry) = transactions.get_mut(&(slot, tx_index)) else {
            continue;
        };
        entry.instructions.push(SvmInstruction {
            path: instruction
                .instruction_address
                .clone()
                .unwrap_or_default(),
            program: instruction
                .executing_account
                .map(|key| key.0)
                .unwrap_or_default(),
            accounts: instruction
                .account_arguments
                .as_ref()
                .map(|keys| keys.iter().map(|key| key.0).collect())
                .unwrap_or_default(),
            data: instruction.data.clone().unwrap_or_default(),
        });
    }

    for row in &response.account_activity {
        let (Some(slot), Some(tx_index)) =
            (row.slot, row.transaction_index)
        else {
            continue;
        };
        let Some(entry) = transactions.get_mut(&(slot, tx_index)) else {
            continue;
        };
        entry.activity.push(SvmAccountActivity {
            account: row.account.map(|key| key.0).unwrap_or_default(),
            mint: row.mint.map(|key| key.0),
            pre_owner: row.pre_owner.map(|key| key.0),
            post_owner: row.post_owner.map(|key| key.0),
            decimals: row.token_decimals,
            pre_token_balance: row.pre_token_balance,
            post_token_balance: row.post_token_balance,
            pre_balance: row.pre_balance,
            post_balance: row.post_balance,
            is_signer: row.is_signer,
            is_fee_payer: row.is_fee_payer.unwrap_or(false),
            token_program: row
                .post_program_id
                .or(row.pre_program_id)
                .map(|key| key.0),
        });
    }

    for row in &response.logs {
        let (Some(slot), Some(tx_index)) =
            (row.slot, row.transaction_index)
        else {
            continue;
        };
        let Some(entry) = transactions.get_mut(&(slot, tx_index)) else {
            continue;
        };
        // Only the two kinds that can carry an event. `invoke` / `success` /
        // `consumed` are runtime chatter, and dropping them here keeps the
        // decoder's own scan short.
        let is_data = match row.kind {
            Some(LogKind::Data) => true,
            Some(LogKind::Log) => false,
            _ => continue,
        };
        entry.logs.push(SvmLog {
            path: row.instruction_address.clone().unwrap_or_default(),
            program: row.program_id.map(|key| key.0).unwrap_or_default(),
            is_data,
            message: row.message.clone().unwrap_or_default(),
        });
    }

    let mut by_slot: HashMap<u64, Vec<SvmTransaction>> = HashMap::new();
    for ((slot, _), mut tx) in transactions {
        tx.instructions.sort_by(|a, b| a.path.cmp(&b.path));
        by_slot.entry(slot).or_default().push(tx);
    }

    let mut batches: Vec<SvmSlotBatch> = response
        .blocks
        .iter()
        .filter_map(|block| {
            let slot = block.slot?;
            let mut transactions =
                by_slot.remove(&slot).unwrap_or_default();
            transactions.sort_by_key(|tx| tx.tx_index);
            Some(SvmSlotBatch {
                slot,
                blockhash: block
                    .blockhash
                    .map(|hash| hash.0)
                    .unwrap_or_default(),
                parent_slot: block.parent_slot.unwrap_or_default(),
                parent_blockhash: block
                    .parent_blockhash
                    .map(|hash| hash.0)
                    .unwrap_or_default(),
                block_height: block.block_height.unwrap_or_default(),
                timestamp: block.block_time.unwrap_or_default().max(0)
                    as u32,
                transactions,
            })
        })
        .collect();
    batches.sort_by_key(|batch| batch.slot);

    SolanaBatch {
        next_slot: response.next_slot,
        batches,
        rollback_guard: response.rollback_guard.map(|guard| {
            RollbackGuard {
                slot_number: guard.slot_number,
                blockhash: guard.blockhash.0,
                first_slot_number: guard.first_slot_number,
                first_previous_blockhash: guard.first_previous_blockhash.0,
                timestamp: guard.timestamp,
            }
        }),
    }
}

/// Signature bytes, for callers that build a transaction filter.
pub fn signature_bytes(base58: &str) -> Result<SigBytes> {
    let mut out = [0u8; 64];
    let written = bs58::decode(base58)
        .onto(&mut out[..])
        .with_context(|| format!("decode signature {base58}"))?;
    if written != 64 {
        bail!(
            "signature {base58} decoded to {written} bytes, expected 64"
        );
    }
    Ok(out)
}

/// A query restricted to specific transactions, used by the live tests.
///
/// The `transactions` and `instruction_calls` arrays are INTERSECTED, so a
/// match-all instruction selection returns every instruction of exactly
/// those transactions - including the children of unregistered programs.
pub fn build_transaction_query(
    from_slot: u64,
    to_slot: u64,
    signatures: &[&str],
) -> Result<SolanaQuery> {
    let ids = signatures
        .iter()
        .map(|signature| {
            signature_bytes(signature)
                .map(hypersync_solana_net_types::Signature)
        })
        .collect::<Result<Vec<_>>>()?;

    let mut query = SolanaQuery {
        from_slot,
        to_slot: Some(to_slot),
        transactions: vec![TransactionSelection {
            transaction_id: ids,
            ..Default::default()
        }],
        instruction_calls: vec![InstructionSelection::default()],
        account_activity: vec![AccountActivitySelection::default()],
        logs: vec![LogSelection::default()],
        include_all_blocks: true,
        field_selection: field_selection(),
        ..Default::default()
    };
    raise_response_caps(&mut query);
    Ok(query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transfer_programs_share_the_instruction_array_with_the_venues()
    {
        let query = build_query(100, 200);
        assert_eq!(query.from_slot, 100);
        assert_eq!(query.to_slot, Some(200));
        assert!(query.include_all_blocks);

        // One array: venues OR token transfers OR system transfers. Two
        // arrays would be intersected and return nothing.
        let registry = registry();
        let programs: Vec<Pubkey> = query
            .instruction_calls
            .iter()
            .flat_map(|selection| selection.executing_account.iter())
            .map(|address| address.0)
            .collect();

        assert!(programs.contains(&registry.spl_token));
        assert!(programs.contains(&registry.token_2022));
        assert!(programs.contains(&registry.system));
        for venue in VENUES {
            assert!(programs.contains(&crate::svm::programs::pubkey(
                venue.program_b58()
            )));
        }
        assert_eq!(query.instruction_calls.len(), VENUES.len() + 5);
    }

    /// EVERY response cap must be raised, on every query this module builds.
    ///
    /// Raising one and leaving the others unset is not a partial fix, it is
    /// no fix at all: the lowest unset cap binds and stops the response.
    /// Measured on the production query shape, that was the difference
    /// between **1 slot** and **40 slots** per request
    /// (docs/solana-research.md section 11.1.1).
    #[test]
    fn every_response_cap_is_raised() {
        for (name, query) in [
            ("data", build_query(0, 1)),
            ("header", build_header_query(0, 1)),
            (
                "transaction",
                build_transaction_query(
                    448_258_071,
                    448_258_096,
                    &["Qxpfmre4JbRctxg1JKJ7vk5Rze6XGpX36bqkm34dgRe5vvCWt4mkRhDe6Zu999TomKnJ14DetevDDURGsgPLaMb"],
                )
                .expect("valid signature"),
            ),
        ] {
            for (cap, value) in [
                ("max_num_blocks", query.max_num_blocks),
                ("max_num_transactions", query.max_num_transactions),
                ("max_num_instructions", query.max_num_instructions),
                ("max_num_logs", query.max_num_logs),
                (
                    "max_num_account_activity",
                    query.max_num_account_activity,
                ),
            ] {
                assert_eq!(
                    value,
                    Some(MAX_RESPONSE_ROWS),
                    "the {name} query leaves {cap} unset, which caps the \
                     whole response no matter what the others say"
                );
            }
        }
    }

    /// And a cap the crate adds LATER must not slip through unnoticed.
    ///
    /// `SolanaQuery`'s `max_num_*` fields are plain `Option`s with no
    /// `skip_serializing_if`, so serialising a query lists every one of
    /// them, including any this file has never heard of. If the crate grows
    /// a sixth cap, this fails with its name rather than silently throttling
    /// the stream back to one slot a query.
    #[test]
    fn a_response_cap_added_by_a_future_crate_version_is_caught() {
        let value = serde_json::to_value(build_query(0, 1))
            .expect("the query serialises");
        let object = value.as_object().expect("a JSON object");

        let caps: Vec<&String> = object
            .keys()
            .filter(|key| key.starts_with("max_num_"))
            .collect();
        assert_eq!(
            caps.len(),
            5,
            "hypersync-solana-net-types now has {} response caps, not 5: \
             {caps:?}. Add the new one to raise_response_caps.",
            caps.len()
        );
        for cap in caps {
            assert!(
                !object[cap].is_null(),
                "{cap} is unset and will cap the whole response"
            );
        }
    }

    /// The client's own auto-tuner is the same trap one layer down: its
    /// default ceiling is 500 KB against a measured ~0.5 MB PER SLOT, so it
    /// converges on a batch of one slot and undoes the fix above.
    #[test]
    fn the_stream_byte_thresholds_clear_a_single_slot() {
        /// Measured Arrow bytes for one slot of the production query shape.
        const BYTES_PER_SLOT: u64 = 500_000;

        let config = stream_config();
        let default = StreamConfig::default();
        assert!(
            config.response_bytes_ceiling > default.response_bytes_ceiling,
            "the default ceiling is one slot's worth of Arrow"
        );
        assert!(
            config.response_bytes_floor >= 16 * BYTES_PER_SLOT,
            "a floor below ~16 slots lets the auto-tuner shrink the batch \
             back towards one slot"
        );
        assert!(config.response_bytes_ceiling > config.response_bytes_floor);
    }

    /// Raydium and Orca publish their swap events as LOG LINES and nothing
    /// else, so the log table is not optional. Phase 1 selected no log
    /// fields at all, which put half the phase 2 volume out of reach.
    #[test]
    fn the_log_table_is_selected_with_its_instruction_address() {
        let query = build_query(0, 1);
        assert_eq!(query.logs.len(), 1);
        assert!(
            query.logs[0].is_empty(),
            "the log selection must be match-all: selections in different \
             arrays are INTERSECTED, so filtering logs by program would \
             drop every transaction that has no such log"
        );
        assert!(query
            .field_selection
            .log
            .contains(&LogField::InstructionAddress));
        assert!(query.field_selection.log.contains(&LogField::Kind));
        assert!(query.field_selection.log.contains(&LogField::Message));
        assert!(query.field_selection.log.contains(&LogField::ProgramId));
    }

    #[test]
    fn the_header_query_asks_for_headers_only() {
        let query = build_header_query(10, 20);
        assert!(query.instruction_calls.is_empty());
        assert!(query.account_activity.is_empty());
        assert!(query.transactions.is_empty());
        assert_eq!(query.field_selection.block.len(), BLOCK_FIELDS.len());
        assert!(query.field_selection.instruction_call.is_empty());
        assert!(query.field_selection.account_activity.is_empty());
    }

    /// The parent chain is what defines continuity on Solana, so both
    /// fields must always be requested.
    #[test]
    fn headers_carry_the_parent_chain() {
        assert!(BLOCK_FIELDS.contains(&BlockField::ParentSlot));
        assert!(BLOCK_FIELDS.contains(&BlockField::ParentBlockhash));
    }

    /// The native side of account_activity is the ONLY trace a bonding
    /// curve's SOL leg leaves, so dropping it would silently lose every
    /// curve trade.
    #[test]
    fn the_native_balance_side_is_selected() {
        assert!(ACCOUNT_ACTIVITY_FIELDS
            .contains(&AccountActivityField::PreBalance));
        assert!(ACCOUNT_ACTIVITY_FIELDS
            .contains(&AccountActivityField::PostBalance));
        assert!(ACCOUNT_ACTIVITY_FIELDS
            .contains(&AccountActivityField::TokenDecimals));
    }

    #[test]
    fn an_empty_token_is_an_error_not_a_401_later() {
        assert!(SolanaSource::new(None, "  ").is_err());
    }

    #[test]
    fn a_transaction_query_intersects_with_a_match_all_instruction_set() {
        let query = build_transaction_query(
            448_258_071,
            448_258_096,
            &["Qxpfmre4JbRctxg1JKJ7vk5Rze6XGpX36bqkm34dgRe5vvCWt4mkRhDe6Zu999TomKnJ14DetevDDURGsgPLaMb"],
        )
        .expect("valid signature");
        assert_eq!(query.transactions.len(), 1);
        assert_eq!(query.transactions[0].transaction_id.len(), 1);
        // Match-all: the server short circuits an empty selection.
        assert_eq!(query.instruction_calls.len(), 1);
        assert!(query.instruction_calls[0].is_empty());
    }
}
