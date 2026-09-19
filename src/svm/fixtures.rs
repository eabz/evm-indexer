//! Recorded live transactions, used by the unit and integration tests.
//!
//! `fixtures/recorded.json` holds five real mainnet transactions exactly as
//! `https://solana.hypersync.xyz/query` served them - the server's own field
//! names and values, nothing normalised, nothing hand written. They are kept
//! verbatim so a decoder change is tested against what the wire actually
//! carries rather than against a developer's idea of it.
//!
//! | Name | Why it is here |
//! |---|---|
//! | `pumpswap_buy` | a plain PumpSwap buy with four fee transfers, a Token-2022 base mint and a self-CPI `BuyEvent` |
//! | `pumpfun_sell` | a bonding curve sell whose SOL leg has NO instruction at all: the curve just decrements its own lamports |
//! | `two_opposite_swaps` | docs/solana-research.md section 2.2: ONE transaction, TWO PumpSwap swaps on the SAME pool in OPPOSITE directions. Reading transaction level balances reports ~0.03 SOL instead of two ~6.2 SOL trades |
//! | `jupiter_three_hop` | a Jupiter v6 route over BisonFi -> Meteora DLMM -> Raydium CPMM: must become three swaps, each attributed to its own venue and all three carrying the router as attribution |
//! | `bisonfi_quote_update` | a direct prop-AMM call with 379 compute units and zero token movement. Must decode to NOTHING |

use std::sync::OnceLock;

use serde::Deserialize;

use crate::svm::{
    decode::{
        SvmAccountActivity, SvmInstruction, SvmLog, SvmTransaction,
    },
    models::{Pubkey, SigBytes},
    programs::pubkey,
};

/// The recorded dump, embedded at compile time.
const RECORDED: &str = include_str!("fixtures/recorded.json");

#[derive(Debug, Deserialize)]
struct RawFixture {
    name: String,
    slot: u64,
    block_time: i64,
    blockhash: String,
    parent_slot: u64,
    parent_blockhash: String,
    transaction: RawTransaction,
    instruction_calls: Vec<RawInstruction>,
    account_activity: Vec<RawActivity>,
    /// Phase 2. Absent from the phase 1 recordings, which is why it
    /// defaults: Raydium's and Orca's swap events are LOG lines, so a
    /// fixture for those venues has to carry the log table too.
    #[serde(default)]
    logs: Vec<RawLog>,
}

#[derive(Debug, Deserialize)]
struct RawLog {
    #[serde(default)]
    instruction_address: Vec<u32>,
    program_id: String,
    /// HyperSync's `LogKind`: `data` for a `Program data:` line, `log` for a
    /// `Program log:` one. The message is stored with the prefix already
    /// stripped, exactly as the server serves it.
    kind: String,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct RawTransaction {
    transaction_index: u32,
    transaction_id: String,
    fee_payer: String,
    success: bool,
    #[serde(default)]
    fee: u64,
    #[serde(default)]
    compute_units_consumed: u64,
    #[serde(default)]
    has_dropped_log_messages: bool,
}

#[derive(Debug, Deserialize)]
struct RawInstruction {
    instruction_address: Vec<u32>,
    executing_account: String,
    #[serde(default)]
    account_arguments: Vec<String>,
    #[serde(default)]
    data: String,
}

#[derive(Debug, Deserialize)]
struct RawActivity {
    account: String,
    #[serde(default)]
    mint: Option<String>,
    #[serde(default)]
    pre_owner: Option<String>,
    #[serde(default)]
    post_owner: Option<String>,
    #[serde(default)]
    token_decimals: Option<u8>,
    /// Raw SPL amounts are u64 but travel as JSON strings, because a bare
    /// number would lose precision above 2^53 in a JavaScript consumer.
    #[serde(default)]
    pre_token_balance: Option<String>,
    #[serde(default)]
    post_token_balance: Option<String>,
    #[serde(default)]
    pre_balance: Option<u64>,
    #[serde(default)]
    post_balance: Option<u64>,
    #[serde(default)]
    is_signer: Option<bool>,
    #[serde(default)]
    is_fee_payer: bool,
    #[serde(default)]
    post_program_id: Option<String>,
    #[serde(default)]
    pre_program_id: Option<String>,
}

/// One recorded transaction, decoded into the shapes the decoder takes.
pub struct Fixture {
    pub name: String,
    pub slot: u64,
    pub block_time: i64,
    pub blockhash: Pubkey,
    pub parent_slot: u64,
    pub parent_blockhash: Pubkey,
    pub transaction: SvmTransaction,
}

impl Fixture {
    /// Block time as the `DateTime` column holds it.
    pub fn timestamp(&self) -> u32 {
        self.block_time.max(0) as u32
    }
}

fn signature(base58: &str) -> SigBytes {
    let mut out = [0u8; 64];
    let written = bs58::decode(base58)
        .onto(&mut out[..])
        .unwrap_or_else(|e| panic!("bad signature {base58:?}: {e}"));
    assert_eq!(written, 64, "signature {base58:?} is not 64 bytes");
    out
}

fn parse() -> Vec<Fixture> {
    let raw: Vec<RawFixture> = serde_json::from_str(RECORDED)
        .expect("fixtures/recorded.json parses");

    raw.into_iter()
        .map(|fixture| {
            let instructions = fixture
                .instruction_calls
                .into_iter()
                .map(|instruction| SvmInstruction {
                    path: instruction.instruction_address,
                    program: pubkey(&instruction.executing_account),
                    accounts: instruction
                        .account_arguments
                        .iter()
                        .map(|account| pubkey(account))
                        .collect(),
                    data: hex::decode(&instruction.data)
                        .expect("instruction data is hex"),
                })
                .collect();

            let activity = fixture
                .account_activity
                .into_iter()
                .map(|row| SvmAccountActivity {
                    account: pubkey(&row.account),
                    mint: row.mint.as_deref().map(pubkey),
                    pre_owner: row.pre_owner.as_deref().map(pubkey),
                    post_owner: row.post_owner.as_deref().map(pubkey),
                    decimals: row.token_decimals,
                    pre_token_balance: row
                        .pre_token_balance
                        .as_deref()
                        .map(|v| v.parse().expect("u64 token balance")),
                    post_token_balance: row
                        .post_token_balance
                        .as_deref()
                        .map(|v| v.parse().expect("u64 token balance")),
                    pre_balance: row.pre_balance,
                    post_balance: row.post_balance,
                    is_signer: row.is_signer,
                    is_fee_payer: row.is_fee_payer,
                    token_program: row
                        .post_program_id
                        .as_deref()
                        .or(row.pre_program_id.as_deref())
                        .map(pubkey),
                })
                .collect();

            let logs = fixture
                .logs
                .into_iter()
                .map(|log| SvmLog {
                    path: log.instruction_address,
                    program: pubkey(&log.program_id),
                    is_data: log.kind == "data",
                    message: log.message,
                })
                .collect();

            Fixture {
                name: fixture.name,
                slot: fixture.slot,
                block_time: fixture.block_time,
                blockhash: pubkey(&fixture.blockhash),
                parent_slot: fixture.parent_slot,
                parent_blockhash: pubkey(&fixture.parent_blockhash),
                transaction: SvmTransaction {
                    slot: fixture.slot,
                    tx_index: fixture.transaction.transaction_index,
                    signature: signature(
                        &fixture.transaction.transaction_id,
                    ),
                    fee_payer: pubkey(&fixture.transaction.fee_payer),
                    success: fixture.transaction.success,
                    fee: fixture.transaction.fee,
                    compute_units: fixture
                        .transaction
                        .compute_units_consumed,
                    dropped_logs: fixture
                        .transaction
                        .has_dropped_log_messages,
                    instructions,
                    activity,
                    logs,
                },
            }
        })
        .collect()
}

/// Every recorded transaction.
pub fn all() -> &'static [Fixture] {
    static FIXTURES: OnceLock<Vec<Fixture>> = OnceLock::new();
    FIXTURES.get_or_init(parse)
}

/// One recorded transaction by name. Panics if it is missing, so a renamed
/// fixture is a loud test failure.
pub fn get(name: &str) -> &'static Fixture {
    all()
        .iter()
        .find(|fixture| fixture.name == name)
        .unwrap_or_else(|| panic!("no fixture named {name:?}"))
}

// --- values the event tests assert against -------------------------------

/// Pool of the recorded PumpSwap buy, as the venue's own event names it.
pub const PUMPSWAP_BUY_POOL: &str =
    "EJTBQyiF4GMwXSjucW1qnBMVa7iJD21yyvCvMDFrkSrR";
/// Mint traded in the recorded pump.fun sell.
pub const PUMPFUN_SELL_MINT: &str =
    "GeZNqzY8pDDhhsxkWruUhd6tVb4jd3gG8a72g4Gkpump";
/// The user the recorded pump.fun `TradeEvent` names.
pub const PUMPFUN_SELL_USER: &str =
    "BwWK17cbHxwWBKZkUYvzxLcNQ1YVyaFezduWbtm2de6s";

/// The self-CPI `BuyEvent` instruction data of the recorded PumpSwap buy.
pub fn pumpswap_buy_event_bytes() -> Vec<u8> {
    event_bytes("pumpswap_buy")
}

/// The self-CPI `TradeEvent` instruction data of the recorded pump.fun sell.
pub fn pumpfun_sell_event_bytes() -> Vec<u8> {
    event_bytes("pumpfun_sell")
}

fn event_bytes(name: &str) -> Vec<u8> {
    use crate::svm::programs::EVENT_CPI_PREFIX;
    get(name)
        .transaction
        .instructions
        .iter()
        .find(|instruction| {
            instruction.data.starts_with(&EVENT_CPI_PREFIX)
        })
        .unwrap_or_else(|| panic!("{name} has no self-CPI event"))
        .data
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fixture_parses() {
        let fixtures = all();
        assert_eq!(fixtures.len(), 5, "a fixture went missing");
        for fixture in fixtures {
            assert!(
                !fixture.transaction.instructions.is_empty(),
                "{} has no instructions",
                fixture.name
            );
            assert!(
                fixture.transaction.success,
                "{} should be a committed transaction",
                fixture.name
            );
            // Every instruction path must be packable into an ordinal.
            for instruction in &fixture.transaction.instructions {
                crate::svm::models::pack_ordinal(&instruction.path)
                    .unwrap_or_else(|e| panic!("{}: {e}", fixture.name));
            }
        }
    }

    /// The fixtures are the transactions the research names, at the slots
    /// the research records.
    #[test]
    fn the_research_transactions_are_the_ones_recorded() {
        assert_eq!(get("two_opposite_swaps").slot, 448_258_071);
        assert_eq!(get("jupiter_three_hop").slot, 448_258_095);
        assert_eq!(get("bisonfi_quote_update").slot, 448_258_076);
    }
}
