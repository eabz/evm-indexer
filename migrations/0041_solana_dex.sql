-- Solana DEX swaps, in the CHAIN-NEUTRAL shape of docs/design.md §13 and
-- docs/solana-research.md §0.
--
-- TODO(merge): THIS TABLE IS TEMPORARY. The `dex-neutral` work converts
-- `dex_swaps` to exactly the shape below - 32-byte identity columns,
-- `tx_id String`, and the position key (chain, block_number, tx_index,
-- ordinal). When it lands, this table disappears and the whole migration is
--
--   INSERT INTO dex_swaps (<columns>) SELECT <columns> FROM sol_dex_swaps;
--
-- with no renames and no casts. `svm::models::SvmSwap::DEX_SWAP_COLUMNS`
-- holds that column list in Rust so a column added on either side breaks a
-- test rather than being forgotten. Nothing outside src/svm reads this
-- table.
--
-- Conventions, identical to the EVM decoder so the merged table stays
-- coherent:
--
--   * Identity columns are FixedString(32). A Solana pubkey is 32 raw
--     bytes; an EVM address in the same column is 12 zero bytes + the 20
--     address bytes. Readers pick the format with `chains.family`:
--     base58Encode(x) on Solana, concat('0x', lower(hex(substring(x, 13))))
--     on EVM. No hex strings are stored, ever.
--   * block_number holds the SLOT.
--   * ordinal packs the instruction tree path, 12 bits per level, left
--     aligned, and the HOP sub-index of a multi-fill instruction in the
--     four bits left over at the bottom. A parent sorts before its
--     children, siblings sort in execution order, hop 0 sorts before hop 1
--     of the same instruction, and the value is unique inside a
--     transaction. It is computable from ONE row, which matters because a
--     program-filtered stream never sees the sibling instructions a flat
--     rank would need.
--   * pool_id is the venue's own pool account, and NEVER a vault
--     authority: five of the ten streamed venues own every pool's vaults
--     with ONE program-wide PDA, so storing that owner would key the whole
--     venue into a single candle series. A row whose pool could not be
--     named carries 32 zero bytes here and is excluded from the
--     pool-keyed aggregates of 0042 rather than mis-keyed into them.
--   * amount0 / amount1 are POOL RELATIVE and signed: positive = into the
--     pool. token0 / token1 are the two mints sorted by raw bytes.
--   * amount_in / amount_out are the TAKER's view.
--   * amount_out_gross is what the pool SENT, amount_out what the taker
--     RECEIVED. They differ by a Token-2022 transfer fee that no event
--     mentions, so a single amount_out column would silently be wrong for
--     every Token-2022 pair.
--   * verified_in / verified_out are the mints PROVEN by real token
--     movement in the instruction's own subtree. On EVM that is the
--     corroboration rule of src/dex/corroborate.rs; on Solana it is always
--     available, because only the SPL Token program can change an SPL
--     balance. A "swap" that moved no tokens never becomes a row, which is
--     also what keeps prop-AMM QUOTE UPDATES out - counting instructions
--     instead would overstate those venues about fivefold.
--   * trader is the transaction's fee payer, NEVER the instruction's
--     signer, which is usually a router or a bot.
--   * route_ordinal / route_program attribute a fill to the aggregator that
--     routed it. Aggregator volume is ATTRIBUTION ONLY and must never be
--     added to venue volume: 40% of Solana DEX volume is routed, so doing
--     so would double count almost half the chain.
--   * confidence is 'movement' (token movement only: pool, mints, amounts,
--     trader and price are exact, pool state is not available) or 'decoded'
--     (a per-program decoder also ran and AGREED with the movement layer,
--     adding exact fees and pool or curve state).
CREATE TABLE IF NOT EXISTS sol_dex_swaps (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  tx_index UInt32,
  ordinal UInt64,
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  -- Raw bytes: 64 on Solana, 32 on EVM. Never a sorting key column, so the
  -- length prefix a String carries costs nothing.
  tx_id String,
  pool_id FixedString(32),
  protocol LowCardinality(String),
  venue_program FixedString(32),
  trader FixedString(32),
  sender FixedString(32),
  recipient FixedString(32),
  token0 FixedString(32),
  token1 FixedString(32),
  amount0 Int256,
  amount1 Int256,
  token_in FixedString(32),
  token_out FixedString(32),
  amount_in UInt256,
  amount_out UInt256,
  amount_out_gross UInt256,
  verified_in FixedString(32),
  verified_out FixedString(32),
  reserve0 UInt256,
  reserve1 UInt256,
  -- The fee taken out of ONE leg, and the mint it is denominated in. The
  -- mint column is what makes the amount interpretable: a swap can pay a
  -- fee in lamports and another in the token, and a single column summing
  -- both would report base units added to lamports. fee_mint is 32 zero
  -- bytes when no fee was identified.
  fee_amount UInt256,
  fee_mint FixedString(32),
  confidence LowCardinality(String),
  route_ordinal UInt64 DEFAULT 0,
  route_program FixedString(32),
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- The chain registry (docs/design.md §13). Tiny, and it is what lets a view
-- or a UI know how to PRINT an identity column without hard-coding a chain
-- id anywhere.
--
-- Solana's id is 1399811149. No standard integer chain id for Solana
-- exists: CAIP-2 uses a string, Wormhole's id 1 collides with Ethereum
-- mainnet, and SLIP-44 501 / token-list 101 are small enough to collide
-- with a future EVM chain. 1399811149 is the Hyperlane domain id, already
-- used by real bridge infrastructure for exactly this purpose and far
-- outside any plausible EIP-155 allocation. This table is what makes the
-- choice reversible.
--
-- The `chains` registry itself is created by migration 0006_chains.sql.
-- Migrations carry no seed rows (they must be replayable): the indexer
-- registers its chain at startup - `indexer run --chain solana` writes
-- (1399811149, 'solana', 'svm') - so nothing is inserted here.
