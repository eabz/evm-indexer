-- Solana (SVM) core tables. Reserved range 0040-0049 (docs/design.md §14).
--
-- READ THIS BEFORE QUERYING ANY sol_* TABLE.
--
-- This is an ANALYTICS-ONLY, PROGRAM-FILTERED pipeline. Solana produces
-- ~150M non-vote transactions a day, ~30x the row rate the EVM pipeline is
-- tuned for, and the value is concentrated in a couple of dozen programs.
-- So the indexer streams DEX and launchpad programs only, and these tables
-- hold ONLY the slots and transactions those programs appear in.
--
-- What that means, stated here rather than left to tribal knowledge:
--   * sol_transactions is NOT the chain's transactions. It is the matched
--     ones. A count over it is not a transaction count.
--   * There is no chain-wide SPL transfer table and there must not be one:
--     "every transfer of mint X" would be partial and therefore wrong.
--   * There is no wallet history. A Solana address page could only show its
--     DEX activity, never its balance or its full history.
--   * There is no daily_block_stats equivalent, deliberately. A statistic
--     over a filtered subset presented as chain activity misleads, which is
--     the same reason design.md §9 refuses a contract-deployment aggregate.
--
-- Storage rules are the shared ones (docs/design.md §1, §2): binary columns
-- and no hex strings, ReplacingMergeTree(_version, is_deleted) with epoch,
-- month-only partitions on base tables, and NOTHING is ever deleted - a
-- rollback INSERTs tombstones and FINAL hides them. Query every table here
-- with FINAL.
--
-- The deduplication window settings are added by the later cross-module
-- migration (0090), not here.

-- The commit marker, the Solana equivalent of `blocks`.
--
-- SKIPPED SLOTS ARE NORMAL. A slot with no block is not a gap and not a
-- reorg: Solana simply produces no block for it. Continuity is therefore
-- the parent_slot / parent_blockhash CHAIN, never `slot + 1`, and any gap
-- check that assumes every integer has a row will produce endless false
-- gaps on this chain.
--
-- The column is called block_number and holds the SLOT. The name is kept
-- because every shared purge, tombstone and checkpoint statement is written
-- against it (`db::block_number_column`); renaming it buys nothing and
-- costs everything.
CREATE TABLE IF NOT EXISTS sol_slots (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  blockhash FixedString(32),
  parent_slot UInt64 CODEC(Delta, ZSTD),
  parent_blockhash FixedString(32),
  block_height UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- Transactions the program filter matched, and ONLY those. Slim on purpose:
-- everything here is needed to attribute or audit a swap, nothing is here
-- to serve a transaction explorer.
--
-- signature is the 64-byte ed25519 signature (Solana's transaction id), raw
-- bytes. Readers format it with base58Encode(signature).
--
-- fee_payer is the reliable trader on Solana. The signer of a swap
-- instruction is very often a router's PDA or a bot - 40% of Solana DEX
-- volume is routed - so it must never be used for attribution.
CREATE TABLE IF NOT EXISTS sol_transactions (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  tx_index UInt32,
  signature FixedString(64),
  fee_payer FixedString(32),
  success Bool,
  fee UInt64,
  compute_units UInt64,
  -- The validator truncated this transaction's logs. Anything decoded from
  -- a `Program data:` log line is incomplete when this is true; the
  -- self-CPI events this module uses are instructions and are never
  -- dropped.
  dropped_logs Bool DEFAULT false,
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, tx_index)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- Mints seen traded, with their decimals.
--
-- A separate table from `tokens` because `tokens.address` is
-- FixedString(20) and a Solana mint is 32 bytes. Decimals arrive free on
-- every account_activity token row, so unlike EVM a Solana swap needs no
-- RPC call to be valued at all. Name, symbol and URI live in account state
-- (the Metaplex metadata PDA, or the Token-2022 metadata extension) and are
-- NOT populated in phase 1; they need an account read, which HyperSync does
-- not serve.
--
-- Not block scoped, exactly like `tokens`: a mint's decimals do not change
-- with a fork, so no purge ever touches this table.
CREATE TABLE IF NOT EXISTS sol_tokens (
  chain UInt64,
  mint FixedString(32),
  decimals UInt8,
  -- Classic SPL Token or Token-2022. A Token-2022 mint can carry a
  -- transfer fee, which is why swaps store both a gross and a net amount.
  program FixedString(32),
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, mint)
SETTINGS do_not_merge_across_partitions_select_final = 1;
