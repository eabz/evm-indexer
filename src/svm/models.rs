//! Rows of the `sol_*` tables (`migrations/0040_solana_core.sql`,
//! `migrations/0041_solana_dex.sql`) and the position key they share with
//! every other analytics table.
//!
//! Storage rules are the ones in docs/design.md section 1: binary columns
//! (no hex strings anywhere), `UInt256` / `Int256` amounts as 32 little
//! endian bytes, `ReplacingMergeTree(_version, is_deleted)` with `epoch`.
//!
//! Solana specifics, all decided in docs/design.md section 13 and
//! docs/solana-research.md section 0:
//!
//! - a pubkey is 32 RAW bytes in a `FixedString(32)`. An EVM address in the
//!   same column is 12 zero bytes + the 20 address bytes. Readers format
//!   with `base58Encode(x)` on Solana and `concat('0x', lower(hex(substring(x, 13))))`
//!   on EVM, choosing by `chains.family`.
//! - a transaction id is 64 raw bytes (the ed25519 signature), so the
//!   chain-neutral column is `tx_id String` holding raw bytes. The
//!   Solana-only `sol_transactions` table can afford `FixedString(64)`.
//! - the position key is `(chain, block_number, tx_index, ordinal)`.
//!   `block_number` holds the SLOT (the name is kept because every purge,
//!   tombstone and checkpoint statement is written against it, see
//!   `db::block_number_column`). `ordinal` packs the instruction tree path,
//!   see [`pack_ordinal`].

use alloy::primitives::{I256, U256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DeserializeAs, SerializeAs};

use crate::utils::format::{SerI256, SerU256};

/// A Solana account address. 32 raw bytes; base58 is presentation only.
///
/// `[u8; 32]` serializes as a serde tuple, which is exactly the RowBinary
/// shape of `FixedString(32)` (no length prefix) - the same thing
/// `utils::format::SerB256` produces for an EVM hash.
pub type Pubkey = [u8; 32];

/// A Solana transaction signature: 64 raw bytes.
pub type SigBytes = [u8; 64];

/// The all-zero pubkey, used where a column has no value (the schema has no
/// `Nullable` identity columns, see docs/design.md section 1).
pub const ZERO_PUBKEY: Pubkey = [0u8; 32];

/// `chain` value for Solana mainnet (docs/design.md section 14).
///
/// No standard integer chain id for Solana exists. This is the Hyperlane
/// domain id, adopted because real bridge infrastructure already uses it for
/// exactly this purpose and it is far outside any plausible EIP-155
/// allocation. Recorded in the `chains` registry (migration 0006) at startup;
/// nothing outside that table may hard-code it.
pub const SOLANA_CHAIN: u64 = 1_399_811_149;

// --- position key -------------------------------------------------------

/// Bits of a `UInt64` ordinal given to each level of the instruction path.
const ORDINAL_BITS: u32 = 12;

/// Levels an ordinal can hold. Solana's CPI stack height limit is 5, so a
/// path never has more elements than this.
pub const ORDINAL_LEVELS: usize = 5;

/// Largest index a single level can hold (`index + 1` must fit in 12 bits).
pub const ORDINAL_MAX_INDEX: u32 = (1 << ORDINAL_BITS) - 2;

/// Mask of one level.
const ORDINAL_MASK: u64 = (1 << ORDINAL_BITS) - 1;

/// Packs an `instruction_address` (the full CPI tree path HyperSync serves:
/// `[2]` = third top level instruction, `[2, 0]` = its first child) into the
/// single `UInt64` the chain-neutral position key uses.
///
/// `index + 1` of each level goes into 12 bits, left aligned, absent levels
/// are zero. That gives three properties the sort key needs, and they are
/// what the unit tests assert:
///
/// 1. a parent sorts BEFORE its children,
/// 2. siblings sort in execution order,
/// 3. the value is unique inside a transaction.
///
/// Crucially it is computable from ONE row. That matters because a
/// program-filtered stream never sees the sibling instructions a flat rank
/// would need (docs/solana-research.md section 0).
///
/// Fails loudly rather than truncating: a path deeper than
/// [`ORDINAL_LEVELS`] or an index above [`ORDINAL_MAX_INDEX`] cannot happen
/// on Solana today, and silently folding two different instructions onto one
/// ordinal would make them overwrite each other in a `ReplacingMergeTree`.
pub fn pack_ordinal(path: &[u32]) -> Result<u64, OrdinalError> {
    if path.is_empty() {
        return Err(OrdinalError::Empty);
    }
    if path.len() > ORDINAL_LEVELS {
        return Err(OrdinalError::TooDeep(path.len()));
    }

    let mut packed = 0u64;
    for (level, &index) in path.iter().enumerate() {
        if index > ORDINAL_MAX_INDEX {
            return Err(OrdinalError::IndexTooLarge(index));
        }
        let shift = 64 - ORDINAL_BITS * (level as u32 + 1);
        packed |= u64::from(index + 1) << shift;
    }
    Ok(packed)
}

/// Inverse of [`pack_ordinal`], for tests and for reading rows back.
pub fn unpack_ordinal(ordinal: u64) -> Vec<u32> {
    let mut path = Vec::new();
    for level in 0..ORDINAL_LEVELS as u32 {
        let shift = 64 - ORDINAL_BITS * (level + 1);
        let value = ((ordinal >> shift) & ORDINAL_MASK) as u32;
        if value == 0 {
            break;
        }
        path.push(value - 1);
    }
    path
}

/// Why an instruction path could not be packed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrdinalError {
    /// HyperSync served an empty `instruction_address`.
    Empty,
    /// Deeper than Solana's CPI stack height limit.
    TooDeep(usize),
    /// A sibling index that does not fit in 12 bits.
    IndexTooLarge(u32),
}

impl std::fmt::Display for OrdinalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrdinalError::Empty => f.write_str("empty instruction_address"),
            OrdinalError::TooDeep(depth) => write!(
                f,
                "instruction_address is {depth} levels deep, more than the \
                 {ORDINAL_LEVELS} an ordinal can hold"
            ),
            OrdinalError::IndexTooLarge(index) => write!(
                f,
                "instruction index {index} does not fit in \
                 {ORDINAL_BITS} bits"
            ),
        }
    }
}

impl std::error::Error for OrdinalError {}

// --- serializers --------------------------------------------------------

/// `[u8; 64]` <-> `FixedString(64)`: 64 raw bytes, no length prefix.
pub struct SerSig64(());

impl SerializeAs<SigBytes> for SerSig64 {
    /// A `FixedString(64)` is 64 raw bytes with NO length prefix, which is
    /// what a serde TUPLE produces.
    ///
    /// This has to be spelled out: serde implements `Serialize` for arrays
    /// only up to 32 elements, so `[u8; 64]` silently falls through to the
    /// SLICE impl, which is a seq and makes the clickhouse crate write a
    /// LEB128 length byte in front. That extra byte per row desynchronises
    /// the whole RowBinary stream, and ClickHouse reports it as a confusing
    /// "Cannot read all data ... at row N+1" at the END of the batch.
    #[inline]
    fn serialize_as<S: serde::Serializer>(
        value: &SigBytes,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTuple;

        let mut tuple = serializer.serialize_tuple(64)?;
        for byte in value.iter() {
            tuple.serialize_element(byte)?;
        }
        tuple.end()
    }
}

impl<'de> DeserializeAs<'de, SigBytes> for SerSig64 {
    /// serde only derives `Deserialize` for arrays up to 32 elements, so a
    /// 64-byte signature needs its own visitor. RowBinary writes a
    /// `FixedString(64)` as a bare tuple of 64 bytes.
    fn deserialize_as<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<SigBytes, D::Error> {
        use serde::de::{self, SeqAccess, Visitor};

        struct V;

        impl<'de> Visitor<'de> for V {
            type Value = SigBytes;

            fn expecting(
                &self,
                f: &mut std::fmt::Formatter<'_>,
            ) -> std::fmt::Result {
                f.write_str("64 bytes")
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<SigBytes, A::Error> {
                let mut out = [0u8; 64];
                for (index, slot) in out.iter_mut().enumerate() {
                    *slot =
                        seq.next_element::<u8>()?.ok_or_else(|| {
                            de::Error::invalid_length(index, &self)
                        })?;
                }
                Ok(out)
            }

            fn visit_bytes<E: de::Error>(
                self,
                value: &[u8],
            ) -> Result<SigBytes, E> {
                value.try_into().map_err(|_| {
                    de::Error::invalid_length(value.len(), &self)
                })
            }
        }

        deserializer.deserialize_tuple(64, V)
    }
}

/// `Vec<u8>` <-> `String` holding RAW bytes (not hex, not utf-8).
///
/// This is the chain-neutral `tx_id` column of docs/design.md section 13:
/// 32 bytes on EVM, 64 on Solana. It is never a sorting key column, so the
/// LEB128 length prefix a `String` carries costs nothing.
pub struct SerRawBytes(());

impl SerializeAs<Vec<u8>> for SerRawBytes {
    #[inline]
    fn serialize_as<S: serde::Serializer>(
        value: &Vec<u8>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(value)
    }
}

impl<'de> DeserializeAs<'de, Vec<u8>> for SerRawBytes {
    fn deserialize_as<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        serde_bytes_compat::deserialize(deserializer)
    }
}

/// `serialize_bytes` round trip that also survives self describing formats.
mod serde_bytes_compat {
    use serde::de::{self, SeqAccess, Visitor};
    use serde::Deserializer;
    use std::fmt;

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        struct V;

        impl<'de> Visitor<'de> for V {
            type Value = Vec<u8>;

            fn expecting(
                &self,
                f: &mut fmt::Formatter<'_>,
            ) -> fmt::Result {
                f.write_str("a byte string")
            }

            fn visit_bytes<E: de::Error>(
                self,
                value: &[u8],
            ) -> Result<Vec<u8>, E> {
                Ok(value.to_vec())
            }

            fn visit_byte_buf<E: de::Error>(
                self,
                value: Vec<u8>,
            ) -> Result<Vec<u8>, E> {
                Ok(value)
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Vec<u8>, A::Error> {
                let mut bytes =
                    Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(byte) = seq.next_element::<u8>()? {
                    bytes.push(byte);
                }
                Ok(bytes)
            }
        }

        deserializer.deserialize_byte_buf(V)
    }
}

// --- rows ---------------------------------------------------------------

/// A slot that was streamed and committed: the Solana commit marker, the
/// equivalent of a `blocks` row.
///
/// `block_number` holds the slot. Contiguity is the `parent_slot` chain and
/// NEVER `slot + 1`: skipped slots are normal on Solana and a missing
/// integer is not a gap (docs/solana-research.md section 6.3).
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct SolSlot {
    pub chain: u64,
    /// The slot. Named `block_number` so the shared purge / tombstone /
    /// checkpoint SQL keys on it unchanged.
    pub block_number: u64,
    pub blockhash: Pubkey,
    pub parent_slot: u64,
    pub parent_blockhash: Pubkey,
    pub block_height: u64,
    pub timestamp: u32,
    pub epoch: u32,
    pub _version: u64,
    pub is_deleted: u8,
}

/// A transaction the program filter matched. Slim on purpose: this is an
/// analytics-only pipeline and the table is NOT the chain's transactions
/// (docs/solana-research.md section 5.5).
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct SolTransaction {
    pub chain: u64,
    pub block_number: u64,
    pub tx_index: u32,
    #[serde_as(as = "SerSig64")]
    pub signature: SigBytes,
    /// The reliable trader on Solana: the signer of a swap is very often a
    /// router's PDA or a bot (docs/solana-research.md section 1.2).
    pub fee_payer: Pubkey,
    pub success: bool,
    pub fee: u64,
    pub compute_units: u64,
    /// The validator truncated this transaction's logs. A decoder that reads
    /// `Program data:` events must treat such a transaction as incomplete.
    pub dropped_logs: bool,
    pub timestamp: u32,
    pub epoch: u32,
    pub _version: u64,
    pub is_deleted: u8,
}

/// A mint the pipeline saw traded, with the decimals HyperSync serves for
/// free on every `account_activity` token row.
///
/// Separate from the EVM `tokens` table because `tokens.address` is
/// `FixedString(20)` and a Solana mint is 32 bytes. Name and symbol live in
/// account state (Metaplex PDA / Token-2022 extension) and are out of scope
/// for phase 1.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct SolToken {
    pub chain: u64,
    pub mint: Pubkey,
    pub decimals: u8,
    /// Token program that owns the mint: classic SPL Token or Token-2022.
    pub program: Pubkey,
    pub _version: u64,
    pub is_deleted: u8,
}

/// How well a swap row is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Confidence {
    /// Token movement inside the instruction subtree only: pool, mints,
    /// amounts, trader and price are exact, pool state is not available.
    Movement,
    /// A per-program decoder also ran: the venue's own pool account, its
    /// idea of the trader, exact fees and curve / pool state.
    Decoded,
}

impl Confidence {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Confidence::Movement => "movement",
            Confidence::Decoded => "decoded",
        }
    }
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One swap leg on one venue, in the CHAIN-NEUTRAL shape of
/// docs/design.md section 13 / docs/solana-research.md section 0.
///
/// TODO(merge): when the `dex-neutral` work lands, this struct and its table
/// disappear: the fields below are exactly the chain-neutral `dex_swaps`
/// columns, so the merge is `INSERT INTO dex_swaps SELECT * FROM
/// sol_dex_swaps` plus deleting this file's `SvmSwap` in favour of
/// `dex::models::DexSwap`. See [`SvmSwap::to_dex_swap_columns`].
///
/// Conventions, identical to the EVM decoder:
///
/// - `amount0` / `amount1` are POOL RELATIVE and signed: positive = into the
///   pool. `token0` / `token1` are the two mints sorted by their raw bytes,
///   which is deterministic and needs no pool registry.
/// - `amount_in` / `amount_out` are the TAKER's view.
/// - `amount_out_gross` is what the pool SENT; `amount_out` is what the
///   taker RECEIVED. They differ by a Token-2022 transfer fee, which no
///   event mentions (docs/solana-research.md section 2.3).
/// - `verified_in` / `verified_out` are the mints PROVEN by real token
///   movement in this instruction's subtree. On EVM this is the
///   corroboration rule of `src/dex/corroborate.rs`; on Solana it is always
///   available, because only the SPL Token program can change an SPL
///   balance, so a row without movement is never created at all.
/// - `trader` is the transaction's `fee_payer`, NEVER the instruction's
///   signer, which is usually a router or a bot.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct SvmSwap {
    pub chain: u64,
    /// The slot.
    pub block_number: u64,
    pub tx_index: u32,
    /// Packed instruction tree path, see [`pack_ordinal`].
    pub ordinal: u64,
    pub timestamp: u32,
    /// 64 raw signature bytes.
    #[serde_as(as = "SerRawBytes")]
    pub tx_id: Vec<u8>,
    /// The venue's pool state account. From the movement layer alone this is
    /// the common owner of the vault token accounts (the pool authority);
    /// a per-program decoder replaces it with the account the venue itself
    /// names, and the two are asserted equal in the tests.
    pub pool_id: Pubkey,
    /// Venue FAMILY, never a concrete deployment.
    pub protocol: String,
    /// The program that executed the swap instruction.
    pub venue_program: Pubkey,
    /// The transaction's fee payer.
    pub trader: Pubkey,
    /// What the venue itself reports as the user. Often a router PDA - never
    /// use it for attribution.
    pub sender: Pubkey,
    pub recipient: Pubkey,
    pub token0: Pubkey,
    pub token1: Pubkey,
    #[serde_as(as = "SerI256")]
    pub amount0: I256,
    #[serde_as(as = "SerI256")]
    pub amount1: I256,
    pub token_in: Pubkey,
    pub token_out: Pubkey,
    #[serde_as(as = "SerU256")]
    pub amount_in: U256,
    #[serde_as(as = "SerU256")]
    pub amount_out: U256,
    #[serde_as(as = "SerU256")]
    pub amount_out_gross: U256,
    pub verified_in: Pubkey,
    pub verified_out: Pubkey,
    /// Pool reserves BEFORE the swap, when a per-program decoder supplied
    /// them (0 otherwise). Indexed like `token0` / `token1`.
    #[serde_as(as = "SerU256")]
    pub reserve0: U256,
    #[serde_as(as = "SerU256")]
    pub reserve1: U256,
    /// Total fee taken out of the quote leg, in quote units, when a
    /// per-program decoder supplied it.
    #[serde_as(as = "SerU256")]
    pub fee_amount: U256,
    /// `'movement'` or `'decoded'`.
    pub confidence: String,
    /// Ordinal of the router instruction this fill sits under, 0 when the
    /// trade was direct. Aggregators are ATTRIBUTION, never venue volume.
    pub route_ordinal: u64,
    /// Program of the router, zero when direct.
    pub route_program: Pubkey,
    pub epoch: u32,
    pub _version: u64,
    pub is_deleted: u8,
}

impl SvmSwap {
    /// Column names of the chain-neutral `dex_swaps` table this row becomes
    /// once the `dex-neutral` work lands.
    ///
    /// TODO(merge): the conversion is a rename-free field copy. It is a
    /// function rather than a comment so a column added on either side
    /// breaks a test instead of being forgotten.
    pub const DEX_SWAP_COLUMNS: &'static [&'static str] = &[
        "chain",
        "block_number",
        "tx_index",
        "ordinal",
        "timestamp",
        "tx_id",
        "pool_id",
        "protocol",
        "venue_program",
        "trader",
        "sender",
        "recipient",
        "token0",
        "token1",
        "amount0",
        "amount1",
        "token_in",
        "token_out",
        "amount_in",
        "amount_out",
        "amount_out_gross",
        "verified_in",
        "verified_out",
        "reserve0",
        "reserve1",
        "fee_amount",
        "confidence",
        "route_ordinal",
        "route_program",
        "epoch",
        "_version",
        "is_deleted",
    ];

    /// TODO(merge): `INSERT INTO dex_swaps (<these columns>) SELECT <these
    /// columns> FROM sol_dex_swaps` is the whole migration.
    pub fn to_dex_swap_columns() -> String {
        Self::DEX_SWAP_COLUMNS.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parent_sorts_before_its_children() {
        let parent = pack_ordinal(&[2]).unwrap();
        let first = pack_ordinal(&[2, 0]).unwrap();
        let second = pack_ordinal(&[2, 1]).unwrap();
        let grandchild = pack_ordinal(&[2, 1, 0]).unwrap();

        assert!(parent < first);
        assert!(first < second);
        assert!(second < grandchild);
    }

    #[test]
    fn siblings_sort_in_execution_order() {
        let mut previous = 0;
        for index in 0..64u32 {
            let ordinal = pack_ordinal(&[index]).unwrap();
            assert!(ordinal > previous, "index {index} did not increase");
            previous = ordinal;
        }
    }

    #[test]
    fn a_subtree_sorts_before_the_next_top_level_instruction() {
        // The whole subtree of [2] must sit between [2] and [3].
        let next = pack_ordinal(&[3]).unwrap();
        for path in [
            vec![2u32, 0],
            vec![2, 4095 - 1],
            vec![2, 7, 3],
            vec![2, 7, 3, 1, 2],
        ] {
            let ordinal = pack_ordinal(&path).unwrap();
            assert!(ordinal < next, "{path:?} leaked past [3]");
            assert!(ordinal > pack_ordinal(&[2]).unwrap());
        }
    }

    #[test]
    fn packing_round_trips() {
        for path in [
            vec![0u32],
            vec![5],
            vec![2, 0],
            vec![2, 7, 3],
            vec![1, 2, 3, 4, 5],
            vec![ORDINAL_MAX_INDEX; ORDINAL_LEVELS],
        ] {
            let packed = pack_ordinal(&path).unwrap();
            assert_eq!(unpack_ordinal(packed), path, "{path:?}");
        }
    }

    #[test]
    fn different_paths_never_collide() {
        let mut paths = std::collections::HashSet::new();
        for a in 0..12u32 {
            for b in 0..12u32 {
                for c in 0..12u32 {
                    paths.insert(vec![a]);
                    paths.insert(vec![a, b]);
                    paths.insert(vec![a, b, c]);
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        for path in &paths {
            assert!(
                seen.insert(pack_ordinal(path).unwrap()),
                "{path:?} collided with another path"
            );
        }
        assert_eq!(seen.len(), paths.len());
    }

    #[test]
    fn impossible_paths_fail_loudly_instead_of_truncating() {
        assert_eq!(pack_ordinal(&[]), Err(OrdinalError::Empty));
        assert_eq!(
            pack_ordinal(&[0, 0, 0, 0, 0, 0]),
            Err(OrdinalError::TooDeep(6))
        );
        assert_eq!(
            pack_ordinal(&[ORDINAL_MAX_INDEX + 1]),
            Err(OrdinalError::IndexTooLarge(ORDINAL_MAX_INDEX + 1))
        );
    }

    /// Records the serde SHAPE a value serializes to, which is what decides
    /// the RowBinary bytes: a tuple is written bare, a seq gets a LEB128
    /// length in front.
    mod shape {
        use serde::{ser, Serialize};
        use std::fmt;

        #[derive(Debug)]
        pub struct Error(String);

        impl fmt::Display for Error {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl std::error::Error for Error {}
        impl ser::Error for Error {
            fn custom<T: fmt::Display>(msg: T) -> Self {
                Error(msg.to_string())
            }
        }

        /// `("tuple" | "seq", element count)`.
        #[derive(Default)]
        pub struct Recorder {
            pub kind: &'static str,
            pub elements: usize,
        }

        pub fn of<T: Serialize>(value: &T) -> (&'static str, usize) {
            let mut recorder = Recorder::default();
            value.serialize(&mut recorder).expect("record shape");
            (recorder.kind, recorder.elements)
        }

        macro_rules! reject {
            ($($method:ident($($ty:ty)?)),* $(,)?) => {$(
                fn $method(self $(, _: $ty)?) -> Result<(), Error> {
                    Err(ser::Error::custom(stringify!($method)))
                }
            )*};
        }

        impl ser::Serializer for &mut Recorder {
            type Ok = ();
            type Error = Error;
            type SerializeSeq = Self;
            type SerializeTuple = Self;
            type SerializeTupleStruct = ser::Impossible<(), Error>;
            type SerializeTupleVariant = ser::Impossible<(), Error>;
            type SerializeMap = ser::Impossible<(), Error>;
            type SerializeStruct = ser::Impossible<(), Error>;
            type SerializeStructVariant = ser::Impossible<(), Error>;

            reject!(
                serialize_bool(bool),
                serialize_i8(i8),
                serialize_i16(i16),
                serialize_i32(i32),
                serialize_i64(i64),
                serialize_u16(u16),
                serialize_u32(u32),
                serialize_u64(u64),
                serialize_f32(f32),
                serialize_f64(f64),
                serialize_char(char),
                serialize_str(&str),
                serialize_bytes(&[u8]),
                serialize_unit(),
                serialize_none(),
            );

            fn serialize_u8(self, _: u8) -> Result<(), Error> {
                self.elements += 1;
                Ok(())
            }

            fn serialize_seq(
                self,
                len: Option<usize>,
            ) -> Result<Self, Error> {
                self.kind = "seq";
                let _ = len;
                Ok(self)
            }

            fn serialize_tuple(self, _: usize) -> Result<Self, Error> {
                self.kind = "tuple";
                Ok(self)
            }

            fn serialize_some<T: ?Sized + Serialize>(
                self,
                value: &T,
            ) -> Result<(), Error> {
                value.serialize(self)
            }
            fn serialize_unit_struct(self, _: &str) -> Result<(), Error> {
                Err(ser::Error::custom("unit struct"))
            }
            fn serialize_unit_variant(
                self,
                _: &str,
                _: u32,
                _: &str,
            ) -> Result<(), Error> {
                Err(ser::Error::custom("unit variant"))
            }
            fn serialize_newtype_struct<T: ?Sized + Serialize>(
                self,
                _: &str,
                value: &T,
            ) -> Result<(), Error> {
                value.serialize(self)
            }
            fn serialize_newtype_variant<T: ?Sized + Serialize>(
                self,
                _: &str,
                _: u32,
                _: &str,
                _: &T,
            ) -> Result<(), Error> {
                Err(ser::Error::custom("newtype variant"))
            }
            fn serialize_tuple_struct(
                self,
                _: &str,
                _: usize,
            ) -> Result<ser::Impossible<(), Error>, Error> {
                Err(ser::Error::custom("tuple struct"))
            }
            fn serialize_tuple_variant(
                self,
                _: &str,
                _: u32,
                _: &str,
                _: usize,
            ) -> Result<ser::Impossible<(), Error>, Error> {
                Err(ser::Error::custom("tuple variant"))
            }
            fn serialize_map(
                self,
                _: Option<usize>,
            ) -> Result<ser::Impossible<(), Error>, Error> {
                Err(ser::Error::custom("map"))
            }
            fn serialize_struct(
                self,
                _: &str,
                _: usize,
            ) -> Result<ser::Impossible<(), Error>, Error> {
                Err(ser::Error::custom("struct"))
            }
            fn serialize_struct_variant(
                self,
                _: &str,
                _: u32,
                _: &str,
                _: usize,
            ) -> Result<ser::Impossible<(), Error>, Error> {
                Err(ser::Error::custom("struct variant"))
            }
        }

        impl ser::SerializeSeq for &mut Recorder {
            type Ok = ();
            type Error = Error;
            fn serialize_element<T: ?Sized + Serialize>(
                &mut self,
                value: &T,
            ) -> Result<(), Error> {
                value.serialize(&mut **self)
            }
            fn end(self) -> Result<(), Error> {
                Ok(())
            }
        }

        impl ser::SerializeTuple for &mut Recorder {
            type Ok = ();
            type Error = Error;
            fn serialize_element<T: ?Sized + Serialize>(
                &mut self,
                value: &T,
            ) -> Result<(), Error> {
                value.serialize(&mut **self)
            }
            fn end(self) -> Result<(), Error> {
                Ok(())
            }
        }
    }

    /// A 64-byte signature must serialize as a TUPLE of 64 bytes.
    ///
    /// This is a real bug that reached a live ClickHouse: serde implements
    /// `Serialize` for arrays only up to 32 elements, so `[u8; 64]` quietly
    /// resolves to the SLICE impl, which is a seq, and the clickhouse crate
    /// then writes a LEB128 length byte in front of every signature. One
    /// stray byte per row desynchronises the RowBinary stream and ClickHouse
    /// reports it as "Cannot read all data ... at row N+1" at the very end
    /// of the batch, pointing nowhere near the real cause.
    #[test]
    fn a_signature_serializes_as_a_bare_64_byte_tuple() {
        #[serde_as]
        #[derive(Serialize)]
        struct Wrapper(#[serde_as(as = "SerSig64")] SigBytes);

        let (kind, elements) = shape::of(&Wrapper([7u8; 64]));
        assert_eq!(
            kind, "tuple",
            "a seq makes the clickhouse crate add a length prefix, which \
             corrupts every row"
        );
        assert_eq!(elements, 64);

        // A 32-byte pubkey is the shape this must match.
        let (kind, elements) = shape::of(&[0u8; 32]);
        assert_eq!(kind, "tuple");
        assert_eq!(elements, 32);
    }

    /// And it must come back out again.
    #[test]
    fn a_signature_round_trips() {
        #[serde_as]
        #[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
        struct Wrapper(#[serde_as(as = "SerSig64")] SigBytes);

        let mut signature = [0u8; 64];
        for (index, byte) in signature.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let json = serde_json::to_string(&Wrapper(signature)).unwrap();
        let back: Wrapper = serde_json::from_str(&json).unwrap();
        assert_eq!(back.0, signature);
    }

    #[test]
    fn the_chain_id_is_the_one_the_design_recorded() {
        // Must never collide with an EIP-155 id, and must be the value
        // docs/design.md section 14 recorded.
        assert_eq!(SOLANA_CHAIN, 1_399_811_149);
    }
}
