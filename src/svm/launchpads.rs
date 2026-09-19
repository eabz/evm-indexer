//! Solana launchpads, into the SAME `launchpad_*` tables as every EVM
//! chain (docs/design.md §11, §13).
//!
//! One UI screen shows a pump.fun launch, its curve trades, its graduation
//! and then its PumpSwap candles continuously, because all four row kinds
//! land in `launchpad_tokens` / `launchpad_trades` /
//! `launchpad_graduations` / `launchpad_creator_fees` and the graduation
//! row's `pool_id` is the `sol_dex_swaps.pool_id` the DEX decoder writes.
//!
//! # Why this file has its own row structs
//!
//! `src/launchpads/models.rs` types every identity column as an alloy
//! `Address` and pads it to 32 bytes with `SerId32`, because the EVM
//! decoder only ever sees 20-byte addresses. A Solana pubkey IS 32 bytes
//! and would not survive that type. The COLUMNS are identical - the tables
//! have been `FixedString(32)` since migration `0030` - so these structs
//! write the same rows into the same tables by name, and
//! `the_solana_rows_have_the_evm_columns` asserts the two column lists are
//! equal so a column added on either side breaks a test. Nothing in
//! `src/launchpads/**` changes shape, and no EVM behaviour moves.
//!
//! # Trust on Solana is structural, not a registry
//!
//! On EVM anyone can deploy a contract that emits `TokenLaunched`, which is
//! why `launchpad_trusted_emitters` exists. On Solana the emitter is the
//! PROGRAM ID, which the runtime stamps on every instruction and which
//! cannot be forged. Two consequences:
//!
//! * `launchpad_tokens.emitter` is the PROGRAM, and the operator lists
//!   exactly three of them (README `§ Solana launchpads`). Everything else
//!   follows from that one row per venue.
//! * `launchpad_tokens.curve` / `launchpad_trades.emitter` is the CURVE
//!   ACCOUNT - the bonding curve, the DBC virtual pool, the LaunchLab pool
//!   state - so `launchpad_trusted_curves_v` (the listed singletons UNION
//!   every curve a listed emitter announced) keeps working unchanged, and
//!   a trade joins its token exactly as it does on EVM.
//!
//! And there is something EVM cannot do at all: every one of those curve
//! accounts is a Program Derived Address of the launch's own fields, so
//! this module RE-DERIVES it and compares. A pump.fun curve is
//! `["bonding-curve", mint]`, a LaunchLab pool is
//! `["pool", base_mint, quote_mint]`, a DBC pool is
//! `["pool", config, max(mints), min(mints)]`. The check is what lets the
//! LaunchLab decoder read the mint out of an account META at all: the index
//! is not assumed, it is proposed and then proved.
//!
//! # Front ends are never venues
//!
//! bags.fm, StonkFun, BONK.fun and the rest are CONFIGURATIONS of one of
//! these three programs, not programs of their own (docs/launchpads-research
//! §4.2). A DBC launch names its `config` and a LaunchLab launch its
//! `platform_config`; both are stored in `launchpad_tokens.launch_config_id`
//! and joined to `launchpad_frontends` through `sol_launchpad_configs`,
//! which this module fills from `EvtCreateConfig(V2)`. Their volume
//! PARTITIONS the venue's and is never added to it.

use alloy::primitives::U256;
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::{
    svm::{
        decode::{SvmInstruction, SvmTransaction},
        models::{
            pack_ordinal, Pubkey, SerRawBytes, SvmSwap, ZERO_PUBKEY,
        },
        programs::{
            pubkey, registry, Venue, DISC_DBC_CLAIM_CREATOR_FEE,
            DISC_DBC_CLAIM_TRADING_FEE, DISC_DBC_CREATE_CONFIG,
            DISC_DBC_CREATE_CONFIG_V2, DISC_DBC_CREATOR_SURPLUS,
            DISC_DBC_CURVE_COMPLETE, DISC_DBC_INITIALIZE_POOL,
            DISC_DBC_PARTNER_SURPLUS, DISC_DBC_SWAP2,
            DISC_LAUNCHLAB_POOL_CREATE, DISC_LAUNCHLAB_TRADE,
            DISC_PUMPFUN_CREATE, DISC_PUMPFUN_CREATOR_FEE,
            DISC_PUMPFUN_MIGRATED, DISC_PUMPFUN_TRADE_EVENT,
            EVENT_CPI_PREFIX,
        },
        venues::{DbcSwap2, LaunchlabTrade},
    },
    utils::format::SerU256,
};

// --- the families --------------------------------------------------------

/// Value of the `family` column for a Solana launch.
///
/// A separate enum from `launchpads::models::Family` on purpose: that one
/// is the EVM decoder's closed list and its `has_curve()` is asserted by an
/// EVM test. Both write the same `LowCardinality(String)` column, and
/// `solana_families_do_not_collide_with_the_evm_ones` keeps the vocabulary
/// disjoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SolFamily {
    /// The pump.fun bonding curve.
    PumpFun,
    /// Meteora's Dynamic Bonding Curve. bags.fm and the other partners are
    /// `config` accounts under it, never families of their own.
    MeteoraDbc,
    /// Raydium LaunchLab. StonkFun, BONK.fun / LetsBonk and Raydium's own
    /// launches are `platform_config` accounts under it.
    RaydiumLaunchlab,
}

impl SolFamily {
    pub const ALL: [SolFamily; 3] = [
        SolFamily::PumpFun,
        SolFamily::MeteoraDbc,
        SolFamily::RaydiumLaunchlab,
    ];

    pub const fn as_str(&self) -> &'static str {
        match self {
            SolFamily::PumpFun => "pumpfun",
            SolFamily::MeteoraDbc => "meteora_dbc",
            SolFamily::RaydiumLaunchlab => "raydium_launchlab",
        }
    }

    /// The venue whose program emits this family's events.
    pub const fn venue(&self) -> Venue {
        match self {
            SolFamily::PumpFun => Venue::PumpFun,
            SolFamily::MeteoraDbc => Venue::MeteoraDbc,
            SolFamily::RaydiumLaunchlab => Venue::RaydiumLaunchlab,
        }
    }

    pub fn program(&self) -> Pubkey {
        pubkey(self.venue().program_b58())
    }

    pub fn from_program(program: &Pubkey) -> Option<Self> {
        SolFamily::ALL
            .into_iter()
            .find(|family| family.program() == *program)
    }
}

impl std::fmt::Display for SolFamily {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `launchpad_trades.side`, the TRADER's point of view.
const SIDE_BUY: &str = "buy";
const SIDE_SELL: &str = "sell";

/// `launchpad_creator_fees.kind`.
const KIND_CREATOR: &str = "creator";
const KIND_PROTOCOL: &str = "protocol";

/// `launchpad_creator_fees.phase`: every fee this module writes was taken
/// while the token was still on its curve.
const PHASE_CURVE: &str = "curve";

/// `pool_kind`. A Solana pubkey is a NATIVE 32-byte id, never an address
/// left-padded with 12 zero bytes, so a reader must print all 32 bytes and
/// must not strip anything - which is exactly what `'pool_id'` means.
const POOL_KIND_ID: &str = "pool_id";

// --- rows ----------------------------------------------------------------

/// `launchpad_tokens`, with 32-byte Solana ids.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct SolLaunchpadToken {
    pub chain: u64,
    pub token: Pubkey,
    pub family: String,
    /// The PROGRAM. On Solana this is what cannot be forged.
    pub emitter: Pubkey,
    /// The curve account: bonding curve, DBC virtual pool, LaunchLab pool
    /// state. Re-derived as a PDA of the launch's own fields.
    pub curve: Pubkey,
    pub creator: Pubkey,
    pub name: String,
    pub symbol: String,
    pub metadata_uri: String,
    pub quote_token: Pubkey,
    #[serde_as(as = "SerU256")]
    pub initial_supply: U256,
    #[serde_as(as = "SerU256")]
    pub graduation_threshold: U256,
    /// Zero: a curve family's destination pool only exists at graduation.
    pub pool_id: Pubkey,
    pub pool_kind: String,
    /// The venue's per-launch configuration account, as 32 big-endian
    /// bytes in a `UInt256`. See [`config_as_u256`].
    #[serde_as(as = "SerU256")]
    pub launch_config_id: U256,
    pub block_number: u64,
    pub timestamp: u32,
    #[serde_as(as = "SerRawBytes")]
    pub tx_id: Vec<u8>,
    pub tx_index: u32,
    pub ordinal: u64,
    pub tx_from: Pubkey,
    pub epoch: u32,
    pub _version: u64,
}

/// `launchpad_trades`, with 32-byte Solana ids.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct SolLaunchpadTrade {
    pub chain: u64,
    pub block_number: u64,
    pub timestamp: u32,
    #[serde_as(as = "SerRawBytes")]
    pub tx_id: Vec<u8>,
    pub tx_index: u32,
    pub ordinal: u64,
    pub family: String,
    /// The CURVE account, so the join to `launchpad_tokens.curve` and the
    /// trusted-curve filter work exactly as they do on EVM.
    pub emitter: Pubkey,
    pub token: Pubkey,
    pub token_verified: u8,
    pub quote_token: Pubkey,
    pub quote_verified: u8,
    pub side: String,
    pub trader: Pubkey,
    pub caller: Pubkey,
    #[serde_as(as = "SerU256")]
    pub token_amount: U256,
    #[serde_as(as = "SerU256")]
    pub quote_amount: U256,
    #[serde_as(as = "SerU256")]
    pub fee_amount: U256,
    #[serde_as(as = "SerU256")]
    pub tax_amount: U256,
    #[serde_as(as = "SerU256")]
    pub progress_wad: U256,
    pub graduating: u8,
    /// Always 1 on Solana: every quote leg is a real token or lamport
    /// movement, so there is never an unverified one to disambiguate.
    pub sole_unverified_quote: u8,
    pub tx_from: Pubkey,
    pub tx_to: Pubkey,
    #[serde_as(as = "SerU256")]
    pub tx_value: U256,
    pub epoch: u32,
    pub _version: u64,
}

/// `launchpad_graduations`, with 32-byte Solana ids.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct SolLaunchpadGraduation {
    pub chain: u64,
    pub block_number: u64,
    pub timestamp: u32,
    #[serde_as(as = "SerRawBytes")]
    pub tx_id: Vec<u8>,
    pub tx_index: u32,
    pub ordinal: u64,
    pub family: String,
    pub emitter: Pubkey,
    pub token: Pubkey,
    /// THE join key into `sol_dex_swaps.pool_id`. Non-zero only where the
    /// venue's own event names the destination pool - pump.fun does, the
    /// other two do not (see the module README).
    pub pool_id: Pubkey,
    pub pool_kind: String,
    pub quote_token: Pubkey,
    #[serde_as(as = "SerU256")]
    pub token_amount: U256,
    #[serde_as(as = "SerU256")]
    pub quote_amount: U256,
    #[serde_as(as = "SerU256")]
    pub position_id: U256,
    pub tx_from: Pubkey,
    pub epoch: u32,
    pub _version: u64,
}

/// `launchpad_creator_fees`, with 32-byte Solana ids.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct SolLaunchpadCreatorFee {
    pub chain: u64,
    pub block_number: u64,
    pub timestamp: u32,
    #[serde_as(as = "SerRawBytes")]
    pub tx_id: Vec<u8>,
    pub tx_index: u32,
    pub ordinal: u64,
    pub component: u32,
    pub family: String,
    pub emitter: Pubkey,
    pub token: Pubkey,
    pub pool_id: Pubkey,
    pub phase: String,
    pub kind: String,
    pub recipient: Pubkey,
    pub recipient_known: u8,
    pub quote_token: Pubkey,
    #[serde_as(as = "SerU256")]
    pub amount: U256,
    pub tx_from: Pubkey,
    pub epoch: u32,
    pub _version: u64,
}

/// `sol_launchpad_configs` (migration `0042`): the partner configuration a
/// DBC launch points at, and the FEE CLAIMER that identifies the front end.
///
/// Not a `launchpad_*` table because it has no EVM counterpart: on EVM a
/// front end is an address an operator recognises, on Solana it is an
/// on-chain account the venue itself publishes.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct SolLaunchpadConfig {
    pub chain: u64,
    pub family: String,
    pub config: Pubkey,
    pub quote_mint: Pubkey,
    /// The partner's wallet: bags.fm, StonkFun, BONK.fun ... This is the
    /// address an operator puts in `launchpad_frontends`.
    pub fee_claimer: Pubkey,
    pub leftover_receiver: Pubkey,
    pub block_number: u64,
    pub timestamp: u32,
    #[serde_as(as = "SerRawBytes")]
    pub tx_id: Vec<u8>,
    pub epoch: u32,
    pub _version: u64,
}

/// `sol_token_balances` (migration `0042`): who holds a launchpad token.
///
/// The one thing in the launchpad module that is genuinely EVM only is the
/// holder screen: `launchpad_token_holders_v` sums `erc20_transfers`, which
/// has no Solana rows. On Solana the balance does not have to be summed at
/// all - HyperSync's `account_activity` carries the POST balance of every
/// token account of a matched transaction, from validator metadata - so it
/// is READ rather than accumulated, and a missed transfer cannot make it
/// drift.
///
/// `_version` is the POSITION, so the newest observation of an account wins
/// a merge on its own and a replayed range cannot move a balance backwards.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct SolTokenBalance {
    pub chain: u64,
    pub mint: Pubkey,
    /// The WALLET, which is what a holder list shows.
    pub owner: Pubkey,
    /// The SPL token account: one owner can hold several.
    pub account: Pubkey,
    #[serde_as(as = "SerU256")]
    pub balance: U256,
    pub block_number: u64,
    pub tx_index: u32,
    pub timestamp: u32,
    pub epoch: u32,
    pub _version: u64,
    pub is_deleted: u8,
}

/// `_version` for a balance: the slot in the high bits and the transaction
/// index in the low ones, so a later observation always wins a merge.
///
/// 32 bits of transaction index is far more than Solana's ~5,000 per block
/// needs, and a slot fits the remaining 32 until slot 4.29 billion - about
/// 35 years at the current 0.266 s a slot.
pub fn balance_version(slot: u64, tx_index: u32) -> u64 {
    (slot << 32) | u64::from(tx_index)
}

/// A 32-byte account as the `UInt256` the shared `launch_config_id` column
/// holds, big-endian so the bytes read in their natural order.
///
/// ClickHouse turns it back into a pubkey with
/// `base58Encode(reverse(reinterpretAsFixedString(launch_config_id)))` -
/// the `reverse()` is there because `reinterpretAsFixedString` writes the
/// integer's little-endian memory. A ClickHouse test pins the round trip.
pub fn config_as_u256(account: &Pubkey) -> U256 {
    U256::from_be_bytes(*account)
}

/// Inverse of [`config_as_u256`].
pub fn u256_as_config(value: U256) -> Pubkey {
    value.to_be_bytes()
}

/// What one batch of Solana slots produced for the launchpad tables.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SolLaunchpadRows {
    pub tokens: Vec<SolLaunchpadToken>,
    pub trades: Vec<SolLaunchpadTrade>,
    pub graduations: Vec<SolLaunchpadGraduation>,
    pub creator_fees: Vec<SolLaunchpadCreatorFee>,
    pub configs: Vec<SolLaunchpadConfig>,
    pub balances: Vec<SolTokenBalance>,
    pub diagnostics: LaunchpadDiagnostics,
}

/// Why a launchpad event did not become a row. Counted, never guessed at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LaunchpadDiagnostics {
    /// A launch event whose curve account could NOT be re-derived from the
    /// launch's own fields. Never written: an account meta index that has
    /// moved must fail loudly.
    pub curve_not_derived: u64,
    /// A trade event with no swap row at the same ordinal, so the movement
    /// layer did not corroborate it. The row is written with
    /// `token_verified = 0`.
    pub trade_unverified: u64,
    /// An event this module knows the discriminator of but could not parse
    /// at the length it arrived in - i.e. the layout has changed.
    pub bad_length: u64,
}

impl LaunchpadDiagnostics {
    pub fn merge(&mut self, other: &LaunchpadDiagnostics) {
        self.curve_not_derived += other.curve_not_derived;
        self.trade_unverified += other.trade_unverified;
        self.bad_length += other.bad_length;
    }
}

impl SolLaunchpadRows {
    pub fn rows(&self) -> usize {
        self.tokens.len()
            + self.trades.len()
            + self.graduations.len()
            + self.creator_fees.len()
            + self.configs.len()
            + self.balances.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows() == 0
    }

    pub fn append(&mut self, other: &mut SolLaunchpadRows) {
        self.tokens.append(&mut other.tokens);
        self.trades.append(&mut other.trades);
        self.graduations.append(&mut other.graduations);
        self.creator_fees.append(&mut other.creator_fees);
        self.configs.append(&mut other.configs);
        self.balances.append(&mut other.balances);
        self.diagnostics.merge(&other.diagnostics);
    }

    pub fn set_version(&mut self, version: u64) {
        self.tokens.iter_mut().for_each(|r| r._version = version);
        self.trades.iter_mut().for_each(|r| r._version = version);
        self.graduations.iter_mut().for_each(|r| r._version = version);
        self.creator_fees.iter_mut().for_each(|r| r._version = version);
        self.configs.iter_mut().for_each(|r| r._version = version);
        // NOT stamped with the flush version: a balance's `_version` is its
        // POSITION, so a replay of an older range can never overwrite a
        // newer balance with a stale one.
    }

    pub fn set_epoch(&mut self, epoch: u32) {
        self.tokens.iter_mut().for_each(|r| r.epoch = epoch);
        self.trades.iter_mut().for_each(|r| r.epoch = epoch);
        self.graduations.iter_mut().for_each(|r| r.epoch = epoch);
        self.creator_fees.iter_mut().for_each(|r| r.epoch = epoch);
        self.configs.iter_mut().for_each(|r| r.epoch = epoch);
        self.balances.iter_mut().for_each(|r| r.epoch = epoch);
    }
}

// --- byte readers --------------------------------------------------------

fn u64_at(data: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn pubkey_at(data: &[u8], offset: usize) -> Option<Pubkey> {
    let mut out = [0u8; 32];
    out.copy_from_slice(data.get(offset..offset + 32)?);
    Some(out)
}

/// A Borsh `String`: `u32` length then the bytes. Returns the text and the
/// offset just past it.
///
/// Capped at 512 bytes. A launch name, symbol or metadata URI longer than
/// that means the offsets have moved, and reading a multi-megabyte
/// "symbol" out of a misaligned length prefix is exactly the failure a cap
/// prevents.
fn borsh_string(data: &[u8], offset: usize) -> Option<(String, usize)> {
    let len =
        u32::from_le_bytes(data.get(offset..offset + 4)?.try_into().ok()?)
            as usize;
    if len > 512 {
        return None;
    }
    let bytes = data.get(offset + 4..offset + 4 + len)?;
    Some((String::from_utf8_lossy(bytes).into_owned(), offset + 4 + len))
}

/// Where a self-CPI event's body starts: 8 bytes of `emit_cpi!` marker then
/// the 8-byte event discriminator.
const BODY: usize = 16;

// --- pump.fun events -----------------------------------------------------

/// pump.fun's `CreateEvent`, from the deployed program's own on-chain IDL.
///
/// Three Borsh `String`s sit at the FRONT, so nothing in it is at a fixed
/// offset and the whole struct has to be walked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PumpFunCreate {
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub user: Pubkey,
    pub creator: Pubkey,
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub token_total_supply: u64,
    pub token_program: Pubkey,
    /// All-zero on a SOL curve.
    pub quote_mint: Pubkey,
}

impl PumpFunCreate {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.get(8..16)? != DISC_PUMPFUN_CREATE {
            return None;
        }
        let (name, at) = borsh_string(data, BODY)?;
        let (symbol, at) = borsh_string(data, at)?;
        let (uri, at) = borsh_string(data, at)?;

        let mint = pubkey_at(data, at)?;
        let bonding_curve = pubkey_at(data, at + 32)?;
        let user = pubkey_at(data, at + 64)?;
        let creator = pubkey_at(data, at + 96)?;
        // at + 128 is timestamp: i64.
        let virtual_token_reserves = u64_at(data, at + 136)?;
        let virtual_sol_reserves = u64_at(data, at + 144)?;
        let real_token_reserves = u64_at(data, at + 152)?;
        let token_total_supply = u64_at(data, at + 160)?;
        let token_program = pubkey_at(data, at + 168)?;
        // at + 200 is_mayhem_mode, + 201 is_cashback_enabled - both newer
        // than the rest, so the quote mint behind them is optional.
        let quote_mint = pubkey_at(data, at + 202).unwrap_or(ZERO_PUBKEY);

        Some(Self {
            name,
            symbol,
            uri,
            mint,
            bonding_curve,
            user,
            creator,
            virtual_token_reserves,
            virtual_sol_reserves,
            real_token_reserves,
            token_total_supply,
            token_program,
            quote_mint,
        })
    }
}

/// pump.fun's `CompletePumpAmmMigrationEvent`: the curve's liquidity
/// actually moved into a PumpSwap pool. 192 fixed bytes, and the only
/// launchpad event on this chain that NAMES its destination pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PumpFunMigrated {
    pub user: Pubkey,
    pub mint: Pubkey,
    pub mint_amount: u64,
    pub sol_amount: u64,
    pub pool_migration_fee: u64,
    pub bonding_curve: Pubkey,
    /// The PumpSwap pool. THE join key into `sol_dex_swaps.pool_id`.
    pub pool: Pubkey,
    pub quote_mint: Pubkey,
}

impl PumpFunMigrated {
    pub const LEN: usize = BODY + 192;

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.get(8..16)? != DISC_PUMPFUN_MIGRATED {
            return None;
        }
        if data.len() != Self::LEN {
            return None;
        }
        Some(Self {
            user: pubkey_at(data, BODY)?,
            mint: pubkey_at(data, BODY + 32)?,
            mint_amount: u64_at(data, BODY + 64)?,
            sol_amount: u64_at(data, BODY + 72)?,
            pool_migration_fee: u64_at(data, BODY + 80)?,
            bonding_curve: pubkey_at(data, BODY + 88)?,
            // BODY + 120 is timestamp: i64.
            pool: pubkey_at(data, BODY + 128)?,
            quote_mint: pubkey_at(data, BODY + 160)?,
        })
    }
}

/// pump.fun's `CollectCreatorFeeEvent`: a creator swept their vault. Note
/// the unusual field order - `timestamp` comes FIRST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PumpFunCreatorFee {
    pub creator: Pubkey,
    pub creator_fee: u64,
    pub quote_mint: Pubkey,
}

impl PumpFunCreatorFee {
    pub const LEN: usize = BODY + 80;

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.get(8..16)? != DISC_PUMPFUN_CREATOR_FEE {
            return None;
        }
        if data.len() != Self::LEN {
            return None;
        }
        Some(Self {
            // BODY is timestamp: i64.
            creator: pubkey_at(data, BODY + 8)?,
            creator_fee: u64_at(data, BODY + 40)?,
            quote_mint: pubkey_at(data, BODY + 48)?,
        })
    }
}

// --- Meteora DBC events --------------------------------------------------

/// `EvtInitializePool`: the DBC launch. 137 fixed bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbcInitializePool {
    pub pool: Pubkey,
    /// The partner's configuration - the front-end attribution key.
    pub config: Pubkey,
    pub creator: Pubkey,
    pub base_mint: Pubkey,
}

impl DbcInitializePool {
    pub const LEN: usize = BODY + 137;

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.get(8..16)? != DISC_DBC_INITIALIZE_POOL {
            return None;
        }
        if data.len() != Self::LEN {
            return None;
        }
        Some(Self {
            pool: pubkey_at(data, BODY)?,
            config: pubkey_at(data, BODY + 32)?,
            creator: pubkey_at(data, BODY + 64)?,
            base_mint: pubkey_at(data, BODY + 96)?,
            // BODY + 128 pool_type u8, + 129 activation_point u64.
        })
    }
}

/// `EvtCurveComplete`: the curve filled. 80 fixed bytes, and it names no
/// destination pool because there is none yet - the DBC migration
/// instructions emit nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbcCurveComplete {
    pub pool: Pubkey,
    pub config: Pubkey,
    pub base_reserve: u64,
    pub quote_reserve: u64,
}

impl DbcCurveComplete {
    pub const LEN: usize = BODY + 80;

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.get(8..16)? != DISC_DBC_CURVE_COMPLETE {
            return None;
        }
        if data.len() != Self::LEN {
            return None;
        }
        Some(Self {
            pool: pubkey_at(data, BODY)?,
            config: pubkey_at(data, BODY + 32)?,
            base_reserve: u64_at(data, BODY + 64)?,
            quote_reserve: u64_at(data, BODY + 72)?,
        })
    }
}

/// `EvtClaimTradingFee` and `EvtClaimCreatorTradingFee` share a layout:
/// pool, then the base and quote amounts swept. 48 fixed bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbcClaimFee {
    pub pool: Pubkey,
    pub token_base_amount: u64,
    pub token_quote_amount: u64,
}

impl DbcClaimFee {
    pub const LEN: usize = BODY + 48;

    pub fn parse(data: &[u8], discriminator: [u8; 8]) -> Option<Self> {
        if data.get(8..16)? != discriminator {
            return None;
        }
        if data.len() != Self::LEN {
            return None;
        }
        Some(Self {
            pool: pubkey_at(data, BODY)?,
            token_base_amount: u64_at(data, BODY + 32)?,
            token_quote_amount: u64_at(data, BODY + 40)?,
        })
    }
}

/// `EvtCreatorWithdrawSurplus` / `EvtPartnerWithdrawSurplus`: 40 fixed
/// bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbcSurplus {
    pub pool: Pubkey,
    pub surplus_amount: u64,
}

impl DbcSurplus {
    pub const LEN: usize = BODY + 40;

    pub fn parse(data: &[u8], discriminator: [u8; 8]) -> Option<Self> {
        if data.get(8..16)? != discriminator {
            return None;
        }
        if data.len() != Self::LEN {
            return None;
        }
        Some(Self {
            pool: pubkey_at(data, BODY)?,
            surplus_amount: u64_at(data, BODY + 32)?,
        })
    }
}

/// The FIXED PREFIX of `EvtCreateConfig` and `EvtCreateConfigV2`.
///
/// Everything after these four pubkeys is a `ConfigParameters` whose length
/// depends on two `Option`s and a `Vec`, and none of it is attribution. The
/// prefix is identical in both versions, which is the only reason one
/// parser serves both - and the reason it is a PREFIX read rather than an
/// exact-length one, the single place in this module that relaxes that rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbcCreateConfig {
    pub config: Pubkey,
    pub quote_mint: Pubkey,
    /// The partner: bags.fm, and the other DBC front ends.
    pub fee_claimer: Pubkey,
    pub leftover_receiver: Pubkey,
}

impl DbcCreateConfig {
    pub const MIN_LEN: usize = BODY + 128;

    pub fn parse(data: &[u8]) -> Option<Self> {
        let discriminator = data.get(8..16)?;
        if discriminator != DISC_DBC_CREATE_CONFIG_V2
            && discriminator != DISC_DBC_CREATE_CONFIG
        {
            return None;
        }
        if data.len() < Self::MIN_LEN {
            return None;
        }
        Some(Self {
            config: pubkey_at(data, BODY)?,
            quote_mint: pubkey_at(data, BODY + 32)?,
            fee_claimer: pubkey_at(data, BODY + 64)?,
            leftover_receiver: pubkey_at(data, BODY + 96)?,
        })
    }
}

// --- Raydium LaunchLab events --------------------------------------------

/// `PoolCreateEvent`: the LaunchLab launch.
///
/// It names the pool, the creator and the GLOBAL config - **not** the
/// platform config and **not the mint**. Both of those come from the
/// instruction's own account metas, and the pool PDA is what proves the
/// indices are right (see [`launchlab_mints`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchlabPoolCreate {
    pub pool_state: Pubkey,
    pub creator: Pubkey,
    pub global_config: Pubkey,
    pub decimals: u8,
    pub name: String,
    pub symbol: String,
    pub uri: String,
    /// `supply` of the curve variant, the closest thing to a total supply.
    pub supply: u64,
    /// Quote the curve has to raise to graduate.
    pub total_quote_fund_raising: u64,
}

impl LaunchlabPoolCreate {
    /// `CurveParams` variant tags.
    const CURVE_CONSTANT: u8 = 0;

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.get(8..16)? != DISC_LAUNCHLAB_POOL_CREATE {
            return None;
        }
        let pool_state = pubkey_at(data, BODY)?;
        let creator = pubkey_at(data, BODY + 32)?;
        let global_config = pubkey_at(data, BODY + 64)?;

        let at = BODY + 96;
        let decimals = *data.get(at)?;
        let (name, at) = borsh_string(data, at + 1)?;
        let (symbol, at) = borsh_string(data, at)?;
        let (uri, at) = borsh_string(data, at)?;

        // CurveParams: a 1-byte variant tag, then the variant's fields.
        // Constant carries an extra `total_base_sell` between `supply` and
        // `total_quote_fund_raising`; Fixed and Linear do not.
        let variant = *data.get(at)?;
        let supply = u64_at(data, at + 1)?;
        let total_quote_fund_raising = if variant == Self::CURVE_CONSTANT {
            u64_at(data, at + 17)?
        } else {
            u64_at(data, at + 9)?
        };

        Some(Self {
            pool_state,
            creator,
            global_config,
            decimals,
            name,
            symbol,
            uri,
            supply,
            total_quote_fund_raising,
        })
    }
}

/// Account meta indices of LaunchLab's three `initialize*` instructions.
///
/// The event names neither mint, so they have to come from here - and an
/// index is never simply trusted: [`launchlab_mints`] re-derives the pool
/// PDA from the pair it read and refuses the launch when it does not match.
const LAUNCHLAB_PLATFORM_CONFIG: usize = 3;
const LAUNCHLAB_POOL_STATE: usize = 5;
const LAUNCHLAB_BASE_MINT: usize = 6;
const LAUNCHLAB_QUOTE_MINT: usize = 7;

/// The same two mints on a `buy_*` / `sell_*` instruction, where the
/// account list is a different one. Observed live, and proved by the same
/// PDA check rather than trusted.
const LAUNCHLAB_TRADE_POOL_STATE: usize = 4;
const LAUNCHLAB_TRADE_BASE_MINT: usize = 9;
const LAUNCHLAB_TRADE_QUOTE_MINT: usize = 10;

/// The `(base, quote)` mints of a LaunchLab launch, PROVEN against the pool
/// PDA `["pool", base_mint, quote_mint]`.
///
/// Tries the documented index pair and then the reverse, so a layout that
/// swapped them is handled rather than silently mis-decoded, and gives up
/// if neither derives the pool the instruction actually used.
fn launchlab_mints(
    instruction: &SvmInstruction,
    pool_state: &Pubkey,
    program: &Pubkey,
) -> Option<(Pubkey, Pubkey)> {
    launchlab_mints_at(
        instruction,
        pool_state,
        program,
        LAUNCHLAB_BASE_MINT,
        LAUNCHLAB_QUOTE_MINT,
    )
}

fn launchlab_mints_at(
    instruction: &SvmInstruction,
    pool_state: &Pubkey,
    program: &Pubkey,
    first: usize,
    second: usize,
) -> Option<(Pubkey, Pubkey)> {
    let a = instruction.account(first)?;
    let b = instruction.account(second)?;
    for (base, quote) in [(a, b), (b, a)] {
        let derived = crate::svm::pda::find_program_address(
            &[b"pool", &base, &quote],
            program,
        );
        if derived.map(|(address, _)| address) == Some(*pool_state) {
            return Some((base, quote));
        }
    }
    None
}

// --- decoding ------------------------------------------------------------

/// Self-CPI event rows of `instruction`, i.e. the direct children the
/// program emitted for itself.
fn events_of<'a>(
    tx: &'a SvmTransaction,
    instruction: &'a SvmInstruction,
) -> impl Iterator<Item = &'a SvmInstruction> {
    tx.instructions.iter().filter(move |candidate| {
        candidate.program == instruction.program
            && candidate.path.len() == instruction.path.len() + 1
            && candidate.path.starts_with(&instruction.path)
            && candidate.data.starts_with(&EVENT_CPI_PREFIX)
    })
}

/// Every self-CPI event of a launchpad program in this transaction, with
/// the instruction that emitted it.
fn launchpad_calls(
    tx: &SvmTransaction,
) -> Vec<(SolFamily, &SvmInstruction)> {
    tx.instructions
        .iter()
        .filter(|instruction| {
            !instruction.data.starts_with(&EVENT_CPI_PREFIX)
        })
        .filter_map(|instruction| {
            SolFamily::from_program(&instruction.program)
                .map(|family| (family, instruction))
        })
        .collect()
}

/// Context every row needs: where it is and who paid for it.
struct Position<'a> {
    chain: u64,
    timestamp: u32,
    tx: &'a SvmTransaction,
}

impl Position<'_> {
    fn tx_id(&self) -> Vec<u8> {
        self.tx.signature.to_vec()
    }
}

/// Decodes the launchpad rows of ONE transaction.
///
/// `swaps` are the rows the movement layer already produced for this same
/// transaction. They are the CORROBORATION: a curve trade's token and quote
/// legs count as verified only where a swap row at the same ordinal proves
/// the same mints moved. That is the Solana form of the EVM module's
/// `corroborate.rs` rule, and here it is always available.
pub fn decode_transaction(
    chain: u64,
    timestamp: u32,
    tx: &SvmTransaction,
    swaps: &[SvmSwap],
) -> SolLaunchpadRows {
    let mut rows = SolLaunchpadRows::default();
    if !tx.success {
        return rows;
    }
    let position = Position { chain, timestamp, tx };
    let wsol = registry().wsol;

    for (family, instruction) in launchpad_calls(tx) {
        let ordinal = pack_ordinal(&instruction.path).unwrap_or(0);
        let swap = swaps.iter().find(|swap| swap.ordinal == ordinal);

        for event in events_of(tx, instruction) {
            let data = &event.data;
            let Some(discriminator) = data.get(8..16) else { continue };

            match (family, discriminator) {
                (SolFamily::PumpFun, d) if d == DISC_PUMPFUN_CREATE => {
                    decode_pumpfun_create(
                        &position,
                        instruction,
                        data,
                        ordinal,
                        wsol,
                        &mut rows,
                    );
                }
                (SolFamily::PumpFun, d)
                    if d == DISC_PUMPFUN_TRADE_EVENT =>
                {
                    decode_pumpfun_trade(
                        &position,
                        tx,
                        instruction,
                        data,
                        ordinal,
                        wsol,
                        swap,
                        &mut rows,
                    );
                }
                (SolFamily::PumpFun, d) if d == DISC_PUMPFUN_MIGRATED => {
                    decode_pumpfun_migration(
                        &position, data, ordinal, wsol, &mut rows,
                    );
                }
                (SolFamily::PumpFun, d)
                    if d == DISC_PUMPFUN_CREATOR_FEE =>
                {
                    decode_pumpfun_creator_fee(
                        &position, data, ordinal, wsol, &mut rows,
                    );
                }

                (SolFamily::MeteoraDbc, d)
                    if d == DISC_DBC_INITIALIZE_POOL =>
                {
                    decode_dbc_launch(&position, data, ordinal, &mut rows);
                }
                (SolFamily::MeteoraDbc, d) if d == DISC_DBC_SWAP2 => {
                    decode_dbc_trade(
                        &position,
                        tx,
                        instruction,
                        data,
                        ordinal,
                        swap,
                        &mut rows,
                    );
                }
                (SolFamily::MeteoraDbc, d)
                    if d == DISC_DBC_CURVE_COMPLETE =>
                {
                    decode_dbc_graduation(
                        &position, data, ordinal, &mut rows,
                    );
                }
                (SolFamily::MeteoraDbc, d)
                    if d == DISC_DBC_CREATE_CONFIG_V2
                        || d == DISC_DBC_CREATE_CONFIG =>
                {
                    decode_dbc_config(&position, data, &mut rows);
                }
                (SolFamily::MeteoraDbc, d)
                    if d == DISC_DBC_CLAIM_CREATOR_FEE
                        || d == DISC_DBC_CLAIM_TRADING_FEE =>
                {
                    let kind = if d == DISC_DBC_CLAIM_CREATOR_FEE {
                        KIND_CREATOR
                    } else {
                        KIND_PROTOCOL
                    };
                    let mut disc = [0u8; 8];
                    disc.copy_from_slice(d);
                    decode_dbc_claim(
                        &position, data, disc, kind, ordinal, &mut rows,
                    );
                }
                (SolFamily::MeteoraDbc, d)
                    if d == DISC_DBC_CREATOR_SURPLUS
                        || d == DISC_DBC_PARTNER_SURPLUS =>
                {
                    let kind = if d == DISC_DBC_CREATOR_SURPLUS {
                        KIND_CREATOR
                    } else {
                        KIND_PROTOCOL
                    };
                    let mut disc = [0u8; 8];
                    disc.copy_from_slice(d);
                    decode_dbc_surplus(
                        &position, data, disc, kind, ordinal, &mut rows,
                    );
                }

                (SolFamily::RaydiumLaunchlab, d)
                    if d == DISC_LAUNCHLAB_POOL_CREATE =>
                {
                    decode_launchlab_launch(
                        &position,
                        instruction,
                        data,
                        ordinal,
                        &mut rows,
                    );
                }
                (SolFamily::RaydiumLaunchlab, d)
                    if d == DISC_LAUNCHLAB_TRADE =>
                {
                    decode_launchlab_trade(
                        &position,
                        instruction,
                        data,
                        ordinal,
                        swap,
                        &mut rows,
                    );
                }
                _ => {}
            }
        }
    }

    collect_balances(&position, &mut rows);
    rows
}

/// Balances of every LAUNCHPAD TOKEN this transaction touched.
///
/// Bounded on purpose. Writing every `account_activity` post balance would
/// be ~130 million rows a day and would create a chain-wide balance table
/// that is not chain wide - a partial table presented as a complete one,
/// which is worse than none. The mints a launch, a curve trade or a
/// graduation in this transaction NAMED are exactly the tokens whose holder
/// list a launchpad screen asks for, and nothing else is written.
fn collect_balances(position: &Position<'_>, rows: &mut SolLaunchpadRows) {
    let mut mints: Vec<Pubkey> = rows
        .tokens
        .iter()
        .map(|row| row.token)
        .chain(rows.trades.iter().map(|row| row.token))
        .chain(rows.graduations.iter().map(|row| row.token))
        .filter(|mint| *mint != ZERO_PUBKEY)
        .collect();
    if mints.is_empty() {
        return;
    }
    mints.sort_unstable();
    mints.dedup();

    let version = balance_version(position.tx.slot, position.tx.tx_index);

    for row in &position.tx.activity {
        let (Some(mint), Some(balance)) =
            (row.mint, row.post_token_balance)
        else {
            continue;
        };
        if !mints.contains(&mint) {
            continue;
        }
        // The OWNER after the transaction: an account closed and reopened
        // inside it belongs to whoever holds it at the end.
        let Some(owner) = row.post_owner.or(row.pre_owner) else {
            continue;
        };
        rows.balances.push(SolTokenBalance {
            chain: position.chain,
            mint,
            owner,
            account: row.account,
            balance: U256::from(balance),
            block_number: position.tx.slot,
            tx_index: position.tx.tx_index,
            timestamp: position.timestamp,
            epoch: 0,
            _version: version,
            is_deleted: 0,
        });
    }
}

fn decode_pumpfun_create(
    position: &Position<'_>,
    instruction: &SvmInstruction,
    data: &[u8],
    ordinal: u64,
    wsol: Pubkey,
    rows: &mut SolLaunchpadRows,
) {
    let Some(event) = PumpFunCreate::parse(data) else {
        rows.diagnostics.bad_length += 1;
        return;
    };
    let program = SolFamily::PumpFun.program();

    // PROVE the curve: it must be the program's own PDA for this mint.
    // A forged launch cannot satisfy this, and neither can a decoder whose
    // offsets have drifted.
    let derived = crate::svm::pda::find_program_address(
        &[b"bonding-curve", &event.mint],
        &program,
    );
    if derived.map(|(address, _)| address) != Some(event.bonding_curve) {
        rows.diagnostics.curve_not_derived += 1;
        return;
    }

    let quote_token = if event.quote_mint == ZERO_PUBKEY {
        wsol
    } else {
        event.quote_mint
    };

    rows.tokens.push(SolLaunchpadToken {
        chain: position.chain,
        token: event.mint,
        family: SolFamily::PumpFun.as_str().to_owned(),
        emitter: program,
        curve: event.bonding_curve,
        creator: event.creator,
        name: crate::launchpads::decode::sanitize(&event.name),
        symbol: crate::launchpads::decode::sanitize(&event.symbol),
        metadata_uri: crate::launchpads::decode::sanitize(&event.uri),
        quote_token,
        initial_supply: U256::from(event.token_total_supply),
        // pump.fun's threshold is a curve parameter in the program, not a
        // field of any event. Never inferred.
        graduation_threshold: U256::ZERO,
        pool_id: ZERO_PUBKEY,
        pool_kind: POOL_KIND_ID.to_owned(),
        launch_config_id: U256::ZERO,
        block_number: position.tx.slot,
        timestamp: position.timestamp,
        tx_id: position.tx_id(),
        tx_index: position.tx.tx_index,
        ordinal,
        tx_from: position.tx.fee_payer,
        epoch: 0,
        _version: 0,
    });
    let _ = instruction;
}

#[allow(clippy::too_many_arguments)]
fn decode_pumpfun_trade(
    position: &Position<'_>,
    tx: &SvmTransaction,
    instruction: &SvmInstruction,
    data: &[u8],
    ordinal: u64,
    wsol: Pubkey,
    swap: Option<&SvmSwap>,
    rows: &mut SolLaunchpadRows,
) {
    let Some(event) = crate::svm::events::PumpFunTrade::parse(data) else {
        rows.diagnostics.bad_length += 1;
        return;
    };
    let (quote_token, quote_amount) = event.quote_leg(wsol);
    let curve = crate::svm::pda::find_program_address(
        &[b"bonding-curve", &event.mint],
        &SolFamily::PumpFun.program(),
    )
    .map(|(address, _)| address)
    .unwrap_or(ZERO_PUBKEY);

    // The curve filled in THIS transaction: pump.fun says so with a
    // `CompleteEvent` beside the trade.
    let graduating = events_of(tx, instruction).any(|event| {
        event.data.get(8..16)
            == Some(&crate::svm::programs::DISC_PUMPFUN_COMPLETE[..])
    });

    let (token_verified, quote_verified) =
        verify_legs(swap, event.mint, quote_token);
    if token_verified == 0 {
        rows.diagnostics.trade_unverified += 1;
    }

    rows.trades.push(SolLaunchpadTrade {
        chain: position.chain,
        block_number: position.tx.slot,
        timestamp: position.timestamp,
        tx_id: position.tx_id(),
        tx_index: position.tx.tx_index,
        ordinal,
        family: SolFamily::PumpFun.as_str().to_owned(),
        emitter: curve,
        token: event.mint,
        token_verified,
        quote_token,
        quote_verified,
        side: if event.is_buy { SIDE_BUY } else { SIDE_SELL }.to_owned(),
        // pump.fun names the beneficiary itself; the fee payer is very
        // often a bot or a router.
        trader: event.user,
        caller: position.tx.fee_payer,
        token_amount: U256::from(event.token_amount),
        quote_amount: U256::from(quote_amount),
        fee_amount: U256::from(event.total_fee()),
        tax_amount: U256::ZERO,
        // Real reserves against the curve's own target are not in the
        // event, so progress is left at 0 rather than guessed.
        progress_wad: U256::ZERO,
        graduating: u8::from(graduating),
        sole_unverified_quote: 1,
        tx_from: position.tx.fee_payer,
        tx_to: ZERO_PUBKEY,
        tx_value: U256::ZERO,
        epoch: 0,
        _version: 0,
    });
}

fn decode_pumpfun_migration(
    position: &Position<'_>,
    data: &[u8],
    ordinal: u64,
    wsol: Pubkey,
    rows: &mut SolLaunchpadRows,
) {
    let Some(event) = PumpFunMigrated::parse(data) else {
        rows.diagnostics.bad_length += 1;
        return;
    };
    let quote_token = if event.quote_mint == ZERO_PUBKEY {
        wsol
    } else {
        event.quote_mint
    };

    rows.graduations.push(SolLaunchpadGraduation {
        chain: position.chain,
        block_number: position.tx.slot,
        timestamp: position.timestamp,
        tx_id: position.tx_id(),
        tx_index: position.tx.tx_index,
        ordinal,
        family: SolFamily::PumpFun.as_str().to_owned(),
        emitter: event.bonding_curve,
        token: event.mint,
        // The PumpSwap pool: this is the row that makes a token's chart
        // continue after the curve is gone.
        pool_id: event.pool,
        pool_kind: POOL_KIND_ID.to_owned(),
        quote_token,
        token_amount: U256::from(event.mint_amount),
        quote_amount: U256::from(event.sol_amount),
        position_id: U256::ZERO,
        tx_from: position.tx.fee_payer,
        epoch: 0,
        _version: 0,
    });
}

fn decode_pumpfun_creator_fee(
    position: &Position<'_>,
    data: &[u8],
    ordinal: u64,
    wsol: Pubkey,
    rows: &mut SolLaunchpadRows,
) {
    let Some(event) = PumpFunCreatorFee::parse(data) else {
        rows.diagnostics.bad_length += 1;
        return;
    };
    if event.creator_fee == 0 {
        return;
    }
    let quote_token = if event.quote_mint == ZERO_PUBKEY {
        wsol
    } else {
        event.quote_mint
    };

    rows.creator_fees.push(SolLaunchpadCreatorFee {
        chain: position.chain,
        block_number: position.tx.slot,
        timestamp: position.timestamp,
        tx_id: position.tx_id(),
        tx_index: position.tx.tx_index,
        ordinal,
        component: 0,
        family: SolFamily::PumpFun.as_str().to_owned(),
        emitter: SolFamily::PumpFun.program(),
        // A creator vault is per CREATOR, not per token: one sweep covers
        // every coin that creator launched, so naming a token here would
        // be an invention.
        token: ZERO_PUBKEY,
        pool_id: ZERO_PUBKEY,
        phase: PHASE_CURVE.to_owned(),
        kind: KIND_CREATOR.to_owned(),
        recipient: event.creator,
        recipient_known: 1,
        quote_token,
        amount: U256::from(event.creator_fee),
        tx_from: position.tx.fee_payer,
        epoch: 0,
        _version: 0,
    });
}

fn decode_dbc_launch(
    position: &Position<'_>,
    data: &[u8],
    ordinal: u64,
    rows: &mut SolLaunchpadRows,
) {
    let Some(event) = DbcInitializePool::parse(data) else {
        rows.diagnostics.bad_length += 1;
        return;
    };

    // DBC has no name, symbol or URI in its event - they live in a
    // Metaplex metadata account this pipeline does not read. Empty, never
    // guessed.
    rows.tokens.push(SolLaunchpadToken {
        chain: position.chain,
        token: event.base_mint,
        family: SolFamily::MeteoraDbc.as_str().to_owned(),
        emitter: SolFamily::MeteoraDbc.program(),
        curve: event.pool,
        creator: event.creator,
        name: String::new(),
        symbol: String::new(),
        metadata_uri: String::new(),
        // The quote mint is a property of the CONFIG, joined through
        // `sol_launchpad_configs`.
        quote_token: ZERO_PUBKEY,
        initial_supply: U256::ZERO,
        graduation_threshold: U256::ZERO,
        pool_id: ZERO_PUBKEY,
        pool_kind: POOL_KIND_ID.to_owned(),
        launch_config_id: config_as_u256(&event.config),
        block_number: position.tx.slot,
        timestamp: position.timestamp,
        tx_id: position.tx_id(),
        tx_index: position.tx.tx_index,
        ordinal,
        tx_from: position.tx.fee_payer,
        epoch: 0,
        _version: 0,
    });
}

#[allow(clippy::too_many_arguments)]
fn decode_dbc_trade(
    position: &Position<'_>,
    tx: &SvmTransaction,
    instruction: &SvmInstruction,
    data: &[u8],
    ordinal: u64,
    swap: Option<&SvmSwap>,
    rows: &mut SolLaunchpadRows,
) {
    let Some(event) = DbcSwap2::parse(data) else {
        rows.diagnostics.bad_length += 1;
        return;
    };
    // `trade_direction` 0 is base to quote, i.e. the trader SOLD.
    let is_buy = event.trade_direction != 0;

    // The event names only the pool, so the mints come from the swap row
    // the movement layer proved - which is the right way round: the
    // transfers are ground truth and the event is the enrichment.
    let (token, quote_token) = match swap {
        Some(swap) if is_buy => (swap.token_out, swap.token_in),
        Some(swap) => (swap.token_in, swap.token_out),
        None => (ZERO_PUBKEY, ZERO_PUBKEY),
    };
    let (token_verified, quote_verified) =
        verify_legs(swap, token, quote_token);
    if token_verified == 0 {
        rows.diagnostics.trade_unverified += 1;
    }

    let (token_amount, quote_amount) = legs(
        swap,
        is_buy,
        event.included_fee_input_amount,
        event.output_amount,
    );

    let graduating = events_of(tx, instruction).any(|event| {
        event.data.get(8..16) == Some(&DISC_DBC_CURVE_COMPLETE[..])
    });

    rows.trades.push(SolLaunchpadTrade {
        chain: position.chain,
        block_number: position.tx.slot,
        timestamp: position.timestamp,
        tx_id: position.tx_id(),
        tx_index: position.tx.tx_index,
        ordinal,
        family: SolFamily::MeteoraDbc.as_str().to_owned(),
        emitter: event.pool,
        token,
        token_verified,
        quote_token,
        quote_verified,
        side: if is_buy { SIDE_BUY } else { SIDE_SELL }.to_owned(),
        // DBC's event names no user at all, so the fee payer is the best
        // available answer and the row says so by putting it in both.
        trader: position.tx.fee_payer,
        caller: position.tx.fee_payer,
        token_amount,
        quote_amount,
        fee_amount: U256::from(event.total_fee()),
        tax_amount: U256::ZERO,
        // DBC is the one family that states its own progress: the event
        // carries both the quote raised and the threshold.
        progress_wad: event.progress_wad(),
        graduating: u8::from(graduating),
        sole_unverified_quote: 1,
        tx_from: position.tx.fee_payer,
        tx_to: ZERO_PUBKEY,
        tx_value: U256::ZERO,
        epoch: 0,
        _version: 0,
    });
}

fn decode_dbc_graduation(
    position: &Position<'_>,
    data: &[u8],
    ordinal: u64,
    rows: &mut SolLaunchpadRows,
) {
    let Some(event) = DbcCurveComplete::parse(data) else {
        rows.diagnostics.bad_length += 1;
        return;
    };

    rows.graduations.push(SolLaunchpadGraduation {
        chain: position.chain,
        block_number: position.tx.slot,
        timestamp: position.timestamp,
        tx_id: position.tx_id(),
        tx_index: position.tx.tx_index,
        ordinal,
        family: SolFamily::MeteoraDbc.as_str().to_owned(),
        emitter: event.pool,
        // The event names the POOL, not the mint; the token is resolved by
        // joining `launchpad_tokens.curve`.
        token: ZERO_PUBKEY,
        // DBC's migration instructions emit NOTHING, so the destination
        // pool is not knowable from any event. Zero, never a guess - see
        // the README's "what is still missing".
        pool_id: ZERO_PUBKEY,
        pool_kind: POOL_KIND_ID.to_owned(),
        quote_token: ZERO_PUBKEY,
        token_amount: U256::from(event.base_reserve),
        quote_amount: U256::from(event.quote_reserve),
        position_id: U256::ZERO,
        tx_from: position.tx.fee_payer,
        epoch: 0,
        _version: 0,
    });
}

fn decode_dbc_config(
    position: &Position<'_>,
    data: &[u8],
    rows: &mut SolLaunchpadRows,
) {
    let Some(event) = DbcCreateConfig::parse(data) else {
        rows.diagnostics.bad_length += 1;
        return;
    };
    rows.configs.push(SolLaunchpadConfig {
        chain: position.chain,
        family: SolFamily::MeteoraDbc.as_str().to_owned(),
        config: event.config,
        quote_mint: event.quote_mint,
        fee_claimer: event.fee_claimer,
        leftover_receiver: event.leftover_receiver,
        block_number: position.tx.slot,
        timestamp: position.timestamp,
        tx_id: position.tx_id(),
        epoch: 0,
        _version: 0,
    });
}

fn decode_dbc_claim(
    position: &Position<'_>,
    data: &[u8],
    discriminator: [u8; 8],
    kind: &str,
    ordinal: u64,
    rows: &mut SolLaunchpadRows,
) {
    let Some(event) = DbcClaimFee::parse(data, discriminator) else {
        rows.diagnostics.bad_length += 1;
        return;
    };

    // One sweep, two assets: that is exactly what `component` is for.
    for (component, amount) in
        [event.token_base_amount, event.token_quote_amount]
            .into_iter()
            .enumerate()
    {
        if amount == 0 {
            continue;
        }
        rows.creator_fees.push(SolLaunchpadCreatorFee {
            chain: position.chain,
            block_number: position.tx.slot,
            timestamp: position.timestamp,
            tx_id: position.tx_id(),
            tx_index: position.tx.tx_index,
            ordinal,
            component: component as u32,
            family: SolFamily::MeteoraDbc.as_str().to_owned(),
            emitter: event.pool,
            token: ZERO_PUBKEY,
            pool_id: event.pool,
            phase: PHASE_CURVE.to_owned(),
            kind: kind.to_owned(),
            // The event names no recipient; the claimer is whoever the
            // config points at, which is a join and not a claim of this
            // row.
            recipient: ZERO_PUBKEY,
            recipient_known: 0,
            quote_token: ZERO_PUBKEY,
            amount: U256::from(amount),
            tx_from: position.tx.fee_payer,
            epoch: 0,
            _version: 0,
        });
    }
}

fn decode_dbc_surplus(
    position: &Position<'_>,
    data: &[u8],
    discriminator: [u8; 8],
    kind: &str,
    ordinal: u64,
    rows: &mut SolLaunchpadRows,
) {
    let Some(event) = DbcSurplus::parse(data, discriminator) else {
        rows.diagnostics.bad_length += 1;
        return;
    };
    if event.surplus_amount == 0 {
        return;
    }
    rows.creator_fees.push(SolLaunchpadCreatorFee {
        chain: position.chain,
        block_number: position.tx.slot,
        timestamp: position.timestamp,
        tx_id: position.tx_id(),
        tx_index: position.tx.tx_index,
        ordinal,
        component: 0,
        family: SolFamily::MeteoraDbc.as_str().to_owned(),
        emitter: event.pool,
        token: ZERO_PUBKEY,
        pool_id: event.pool,
        phase: PHASE_CURVE.to_owned(),
        kind: kind.to_owned(),
        recipient: ZERO_PUBKEY,
        recipient_known: 0,
        quote_token: ZERO_PUBKEY,
        amount: U256::from(event.surplus_amount),
        tx_from: position.tx.fee_payer,
        epoch: 0,
        _version: 0,
    });
}

fn decode_launchlab_launch(
    position: &Position<'_>,
    instruction: &SvmInstruction,
    data: &[u8],
    ordinal: u64,
    rows: &mut SolLaunchpadRows,
) {
    let Some(event) = LaunchlabPoolCreate::parse(data) else {
        rows.diagnostics.bad_length += 1;
        return;
    };
    let program = SolFamily::RaydiumLaunchlab.program();

    // The pool the instruction used must be the one it names, or the
    // account indices below mean something else entirely.
    if instruction.account(LAUNCHLAB_POOL_STATE) != Some(event.pool_state)
    {
        rows.diagnostics.curve_not_derived += 1;
        return;
    }
    // The event names NEITHER mint. The pair is read from account metas
    // and then PROVED against the pool's own PDA seeds.
    let Some((base_mint, quote_mint)) =
        launchlab_mints(instruction, &event.pool_state, &program)
    else {
        rows.diagnostics.curve_not_derived += 1;
        return;
    };
    // `platform_config`, NOT the `config` the event carries - that one is
    // the GLOBAL config and is the same for every launch, which is exactly
    // the mistake that would attribute every token to one front end.
    let platform_config = instruction
        .account(LAUNCHLAB_PLATFORM_CONFIG)
        .unwrap_or(ZERO_PUBKEY);

    rows.tokens.push(SolLaunchpadToken {
        chain: position.chain,
        token: base_mint,
        family: SolFamily::RaydiumLaunchlab.as_str().to_owned(),
        emitter: program,
        curve: event.pool_state,
        creator: event.creator,
        name: crate::launchpads::decode::sanitize(&event.name),
        symbol: crate::launchpads::decode::sanitize(&event.symbol),
        metadata_uri: crate::launchpads::decode::sanitize(&event.uri),
        quote_token: quote_mint,
        initial_supply: U256::from(event.supply),
        graduation_threshold: U256::from(event.total_quote_fund_raising),
        pool_id: ZERO_PUBKEY,
        pool_kind: POOL_KIND_ID.to_owned(),
        launch_config_id: config_as_u256(&platform_config),
        block_number: position.tx.slot,
        timestamp: position.timestamp,
        tx_id: position.tx_id(),
        tx_index: position.tx.tx_index,
        ordinal,
        tx_from: position.tx.fee_payer,
        epoch: 0,
        _version: 0,
    });
}

fn decode_launchlab_trade(
    position: &Position<'_>,
    instruction: &SvmInstruction,
    data: &[u8],
    ordinal: u64,
    swap: Option<&SvmSwap>,
    rows: &mut SolLaunchpadRows,
) {
    let Some(event) = LaunchlabTrade::parse(data) else {
        rows.diagnostics.bad_length += 1;
        return;
    };
    let is_buy = event.is_buy();

    // Prefer the movement layer, which PROVED the mints moved. Where it
    // produced no row - a LaunchLab trade the two-sided rule could not
    // resolve - fall back to the instruction's own account metas, proved
    // against the pool's PDA seeds exactly as the launch is. Without this
    // about half of LaunchLab's trades would land under 32 zero bytes,
    // measured live.
    let (token, quote_token) = match swap {
        Some(swap) if is_buy => (swap.token_out, swap.token_in),
        Some(swap) => (swap.token_in, swap.token_out),
        None => launchlab_mints_at(
            instruction,
            &event.pool_state,
            &SolFamily::RaydiumLaunchlab.program(),
            LAUNCHLAB_TRADE_BASE_MINT,
            LAUNCHLAB_TRADE_QUOTE_MINT,
        )
        .filter(|_| {
            instruction.account(LAUNCHLAB_TRADE_POOL_STATE)
                == Some(event.pool_state)
        })
        .unwrap_or((ZERO_PUBKEY, ZERO_PUBKEY)),
    };
    let (token_verified, quote_verified) =
        verify_legs(swap, token, quote_token);
    if token_verified == 0 {
        rows.diagnostics.trade_unverified += 1;
    }

    let (token_amount, quote_amount) =
        legs(swap, is_buy, event.amount_in, event.amount_out);

    rows.trades.push(SolLaunchpadTrade {
        chain: position.chain,
        block_number: position.tx.slot,
        timestamp: position.timestamp,
        tx_id: position.tx_id(),
        tx_index: position.tx.tx_index,
        ordinal,
        family: SolFamily::RaydiumLaunchlab.as_str().to_owned(),
        emitter: event.pool_state,
        token,
        token_verified,
        quote_token,
        quote_verified,
        side: if is_buy { SIDE_BUY } else { SIDE_SELL }.to_owned(),
        trader: position.tx.fee_payer,
        caller: position.tx.fee_payer,
        token_amount,
        quote_amount,
        fee_amount: U256::from(event.total_fee()),
        tax_amount: U256::ZERO,
        progress_wad: U256::ZERO,
        // `PoolStatus::Migrate` means the curve filled on this very trade.
        graduating: u8::from(
            event.pool_status == LaunchlabTrade::STATUS_MIGRATE,
        ),
        sole_unverified_quote: 1,
        tx_from: position.tx.fee_payer,
        tx_to: ZERO_PUBKEY,
        tx_value: U256::ZERO,
        epoch: 0,
        _version: 0,
    });
}

/// The `(token_amount, quote_amount)` of a curve trade.
///
/// Prefers the SWAP row, which holds what was actually SENT, over the
/// event's own figures, which some venues report net of a fee the taker
/// never saw. Raydium LaunchLab's `TradeEvent` states the amount the pool
/// was CREDITED, so on a Token-2022 mint it is 1-3% below the transfer the
/// taker signed - and a trade screen has to show what the trader parted
/// with. It also keeps `launchpad_trades` and `sol_dex_swaps` reporting
/// the same number for the same trade, which is what a UI joining them
/// expects.
///
/// Falls back to the event where the movement layer produced no row.
fn legs(
    swap: Option<&SvmSwap>,
    is_buy: bool,
    event_in: u64,
    event_out: u64,
) -> (U256, U256) {
    match swap {
        // A buy takes the quote in and pays the token out.
        Some(swap) if is_buy => (swap.amount_out_gross, swap.amount_in),
        Some(swap) => (swap.amount_in, swap.amount_out_gross),
        None if is_buy => (U256::from(event_out), U256::from(event_in)),
        None => (U256::from(event_in), U256::from(event_out)),
    }
}

/// A leg is VERIFIED when the movement layer's swap row at this ordinal
/// carries the same mint on the same side.
///
/// This is the Solana form of `launchpads::decode`'s corroboration rule:
/// there, an ERC-20 `Transfer` has to back the amount; here, the swap row
/// only exists because real SPL transfers proved both mints, so pointing at
/// it proves the same thing and costs nothing.
fn verify_legs(
    swap: Option<&SvmSwap>,
    token: Pubkey,
    quote_token: Pubkey,
) -> (u8, u8) {
    let Some(swap) = swap else { return (0, 0) };
    let pair = [swap.verified_in, swap.verified_out];
    (
        u8::from(token != ZERO_PUBKEY && pair.contains(&token)),
        u8::from(
            quote_token != ZERO_PUBKEY && pair.contains(&quote_token),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launchpads::models::{
        LaunchpadCreatorFee, LaunchpadGraduation, LaunchpadToken,
        LaunchpadTrade,
    };

    /// Column names of a `Row`, which is what the clickhouse crate inserts
    /// by.
    fn columns<T: Row>() -> Vec<String> {
        T::COLUMN_NAMES.iter().map(|name| name.to_string()).collect()
    }

    /// The Solana rows and the EVM rows are the SAME rows.
    ///
    /// They must be, because they go into the same four tables. Only the
    /// Rust types of the identity columns differ - `Pubkey` here, a padded
    /// `Address` there - and the column NAMES are what the insert uses. A
    /// column added on either side breaks this rather than being discovered
    /// as a RowBinary desync at the end of a batch.
    #[test]
    fn the_solana_rows_have_the_evm_columns() {
        for (name, solana, evm) in [
            (
                "launchpad_tokens",
                columns::<SolLaunchpadToken>(),
                columns::<LaunchpadToken>(),
            ),
            (
                "launchpad_trades",
                columns::<SolLaunchpadTrade>(),
                columns::<LaunchpadTrade>(),
            ),
            (
                "launchpad_graduations",
                columns::<SolLaunchpadGraduation>(),
                columns::<LaunchpadGraduation>(),
            ),
            (
                "launchpad_creator_fees",
                columns::<SolLaunchpadCreatorFee>(),
                columns::<LaunchpadCreatorFee>(),
            ),
        ] {
            assert_eq!(solana, evm, "{name}");
        }
    }

    /// One vocabulary for the `family` column across both decoders.
    #[test]
    fn solana_families_do_not_collide_with_the_evm_ones() {
        for family in SolFamily::ALL {
            assert!(
                family
                    .as_str()
                    .parse::<crate::launchpads::Family>()
                    .is_err(),
                "{family} is also an EVM family",
            );
            assert!(
                family
                    .as_str()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_'),
                "{family} is not snake_case",
            );
        }
        let mut names: Vec<&str> =
            SolFamily::ALL.iter().map(|f| f.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len());
    }

    /// Every family's program is a venue the source actually streams,
    /// otherwise the decoder would be waiting for events that never come.
    #[test]
    fn every_family_is_a_streamed_venue() {
        for family in SolFamily::ALL {
            assert!(
                crate::svm::programs::VENUES.contains(&family.venue()),
                "{family} is not streamed",
            );
            assert_eq!(
                SolFamily::from_program(&family.program()),
                Some(family)
            );
        }
    }

    /// The config account survives the trip through `launch_config_id`.
    #[test]
    fn a_config_account_round_trips_through_the_u256_column() {
        for base58 in [
            "dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN",
            "So11111111111111111111111111111111111111112",
            "11111111111111111111111111111111",
        ] {
            let account = pubkey(base58);
            assert_eq!(u256_as_config(config_as_u256(&account)), account);
        }
    }

    /// A Borsh string is read by its own length prefix, and a length that
    /// cannot be right is refused rather than used to slice.
    #[test]
    fn a_borsh_string_is_bounded_and_never_panics() {
        let mut data = Vec::new();
        data.extend_from_slice(&5u32.to_le_bytes());
        data.extend_from_slice(b"hello");
        assert_eq!(borsh_string(&data, 0), Some(("hello".to_owned(), 9)));

        // A length past the end of the buffer.
        let mut bad = 9_999u32.to_le_bytes().to_vec();
        bad.extend_from_slice(b"hi");
        assert_eq!(borsh_string(&bad, 0), None);

        // And any truncation of a valid one.
        for length in 0..data.len() {
            let _ = borsh_string(&data[..length], 0);
        }
    }

    /// Every event parser must survive arbitrary truncation: a length that
    /// has changed is "not this event", never a panic and never a
    /// plausible number.
    #[test]
    fn truncated_launchpad_events_are_never_a_panic() {
        let mut data = vec![0u8; 512];
        for (offset, disc) in [
            (0usize, DISC_PUMPFUN_CREATE),
            (0, DISC_PUMPFUN_MIGRATED),
            (0, DISC_PUMPFUN_CREATOR_FEE),
            (0, DISC_DBC_INITIALIZE_POOL),
            (0, DISC_DBC_CURVE_COMPLETE),
            (0, DISC_DBC_CREATE_CONFIG_V2),
            (0, DISC_LAUNCHLAB_POOL_CREATE),
        ] {
            data[..8].copy_from_slice(&EVENT_CPI_PREFIX);
            data[8..16].copy_from_slice(&disc);
            let _ = offset;
            for length in 0..data.len() {
                let slice = &data[..length];
                let _ = PumpFunCreate::parse(slice);
                let _ = PumpFunMigrated::parse(slice);
                let _ = PumpFunCreatorFee::parse(slice);
                let _ = DbcInitializePool::parse(slice);
                let _ = DbcCurveComplete::parse(slice);
                let _ = DbcCreateConfig::parse(slice);
                let _ = LaunchlabPoolCreate::parse(slice);
                let _ =
                    DbcClaimFee::parse(slice, DISC_DBC_CLAIM_CREATOR_FEE);
                let _ = DbcSurplus::parse(slice, DISC_DBC_CREATOR_SURPLUS);
            }
        }
    }
}
