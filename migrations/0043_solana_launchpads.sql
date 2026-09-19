-- Solana launchpads: the three things the shared launchpad_* tables cannot
-- hold, and nothing else.
--
-- pump.fun, Meteora DBC and Raydium LaunchLab write their launches, curve
-- trades, graduations and fee sweeps straight into launchpad_tokens,
-- launchpad_trades, launchpad_graduations and launchpad_creator_fees
-- (migration 0030). Those tables have been chain neutral since the day they
-- were written - every identity column is FixedString(32), tx_id is a raw
-- String and the position key is (chain, block_number, tx_index, ordinal) -
-- so a Solana pubkey, a 64 byte signature and a packed instruction path fit
-- with no change to them at all. Nothing in 0030-0033 is altered here.
--
-- Three things genuinely have no EVM counterpart:
--
--   sol_dex_programs        which programs an operator calls markets
--   sol_launchpad_configs   which front end a launch belongs to
--   sol_token_balances      who holds a Solana token (erc20_transfers is
--                           EVM only, so the holder view of 0032 cannot
--                           answer for a pubkey)
--
-- Storage follows the shared rules (docs/design.md section 1, section 2):
-- binary columns and no hex strings anywhere, ReplacingMergeTree, and
-- nothing is ever deleted. Migrations carry NO SEED ROWS - a migration has
-- to be replayable and a lint in db::migrate refuses an INSERT - so the
-- verified program ids ship in src/svm/README.md as ready-to-run INSERTs.
--
-- PRINTING AN ID. Same rule as everywhere else, with the family from the
-- chains registry of 0006:
--   base58Encode(substring(id, 1, 32))           for 'svm'
--   concat('0x', lower(hex(substring(id, 13))))  for 'evm'
-- The substring() is not decoration: toString(FixedString) and CAST AS
-- String TRIM TRAILING ZERO BYTES and would silently shorten a pubkey.

-- Which Solana programs are markets, and who says so.
--
-- WHY THIS IS A TABLE AND NOT A CONSTANT. The prop / "dark" AMMs -
-- HumidiFi, Tessera, Scorch, QuantumAMM, GoonFi, AlphaQ, Deriverse, SolFi
-- V2, BisonFi and the rest - are together about 32% of Solana DEX volume
-- (docs/solana-research.md section 1.3) and publish no IDL and, for most of
-- them, no event at all. The movement layer already decodes them perfectly:
-- it reads real SPL transfers, so amounts, mints, price and trader are
-- exact for every one of them.
--
-- The judgement is the hard part, not the decoding. The generic rule
-- identifies token movement precisely and "this was a trade on a market"
-- only probabilistically (section 3.2, last row) - staking, lending and NFT
-- sales also move two mints across one counterparty. Promoting a program to
-- a VENUE is therefore a false-positive decision, and a false positive here
-- is a fabricated market on somebody's screen. That belongs to an operator,
-- with a confidence and a source, revisable without a release.
--
--   kind        'venue'    a market whose swaps are real volume
--               'router'   an aggregator. ATTRIBUTION ONLY: 40% of Solana
--                          DEX volume is routed, so counting a router's
--                          instruction as a trade double counts half the
--                          chain
--               'prop_amm' a proprietary market maker: a real market, no
--                          IDL, usually no event
--               'frontend' an app or bot routing into somebody else's
--                          venue. Its volume PARTITIONS a venue's and is
--                          never added to it
--   confidence  0..100. A low number is not a reason to hide the row, it
--               is a reason for a screen to say so.
--   source      a URL, an IDL, 'observed live', a Dune query. A judgement
--               with no provenance is not reviewable, which is the whole
--               reason the column exists.
--
-- What a row DOES: it sets the `protocol` name a swap of that program is
-- stored under. What it deliberately does NOT do: add the program to the
-- streaming query. That is still one line in svm::programs::VENUES, because
-- a streamed program costs bandwidth on every slot for ever and that is a
-- different decision from naming one. An unlisted program keeps its
-- built-in name, so this table can only ever ADD knowledge.
CREATE TABLE IF NOT EXISTS sol_dex_programs (
  program_id FixedString(32),
  name LowCardinality(String),
  kind LowCardinality(String),
  confidence UInt8 DEFAULT 0,
  source String DEFAULT '',
  _version UInt64 DEFAULT toUnixTimestamp64Milli(now64(3))
)
ENGINE = ReplacingMergeTree(_version)
ORDER BY program_id;

-- Which FRONT END a launch belongs to.
--
-- bags.fm, StonkFun, BONK.fun / LetsBonk, Jupiter Studio and the rest are
-- not programs. They are CONFIGURATIONS of one of the three launchpad
-- programs (docs/launchpads-research.md section 4.2), and the only thing on
-- chain that names them is the config account's fee claimer. A Meteora DBC
-- launch names its `config` and a Raydium LaunchLab launch its
-- `platform_config`; both are stored in launchpad_tokens.launch_config_id
-- and join here.
--
-- launch_config_id is a UInt256 holding the 32 account bytes BIG ENDIAN,
-- because the shared column is numeric on EVM. Turn it back into an account
-- with
--
--   reverse(reinterpretAsFixedString(launch_config_id))
--
-- - the reverse() is needed because reinterpretAsFixedString writes the
-- integer's little endian memory. An integration test pins that round trip
-- against a real config account.
--
-- Filled by the decoder from the venue's own EvtCreateConfig /
-- EvtCreateConfigV2 events, so it is chain data and not an operator's
-- guess. The operator's part is putting fee_claimer into
-- launchpad_frontends, which is what gives it a NAME.
CREATE TABLE IF NOT EXISTS sol_launchpad_configs (
  chain UInt64,
  family LowCardinality(String),
  config FixedString(32),
  quote_mint FixedString(32),
  -- The partner's wallet. THIS is the address that goes into
  -- launchpad_frontends.
  fee_claimer FixedString(32),
  leftover_receiver FixedString(32),
  block_number UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  tx_id String,
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, config);

-- Who holds a Solana launchpad token.
--
-- THE ONE THING IN THE LAUNCHPAD MODULE THAT IS GENUINELY EVM ONLY.
-- launchpad_token_holders_v (migration 0032) sums erc20_transfers, whose
-- token_address / from / to are FixedString(20); it pads them up to 32
-- bytes, so a Solana pubkey simply finds no row - missing numbers rather
-- than wrong ones, which is the right failure, but it is still no answer.
--
-- On Solana the balances do not have to be summed from transfers at all.
-- HyperSync's account_activity carries the POST balance of every token
-- account of a matched transaction, straight from validator metadata, so a
-- balance is READ rather than accumulated - and a missed transfer cannot
-- drift it, which is strictly better than the EVM path.
--
-- SCOPE, stated plainly because a partial table presented as a complete one
-- is worse than no table: this holds balances for LAUNCHPAD TOKENS ONLY -
-- the mints named by a launch, a curve trade or a graduation in the same
-- batch - and only as of transactions the program filter matched. It is
-- what the "top holders" screen of one launchpad token needs and it is not
-- a chain-wide balance table. There must never be one built on it.
--
--   _version is the POSITION (slot, tx_index packed), so the newest
--   observation of an account wins a merge on its own and a replayed range
--   cannot move a balance backwards.
CREATE TABLE IF NOT EXISTS sol_token_balances (
  chain UInt64,
  mint FixedString(32),
  -- The wallet, not the token account: that is what a holder list shows.
  owner FixedString(32),
  -- The SPL token account itself, because one owner can hold several.
  account FixedString(32),
  balance UInt256,
  block_number UInt64 CODEC(Delta, ZSTD),
  tx_index UInt32,
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, mint, owner, account)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- The Solana twin of launchpad_token_holders_v (0032).
--
-- Same shape and same column names, so a UI switches on chains.family and
-- changes nothing else. `share_of_initial_supply` divides by the launch's
-- own initial_supply exactly as the EVM view does, and falls back to 1 -
-- raw balances - when no launch row is trusted, again exactly as the EVM
-- view does.
--
-- The token parameter is PLAIN HEX with no 0x: 64 characters for a 32 byte
-- pubkey. The `length(...) = 64` conjunct is the empty-parameter guard of
-- 0032: unhex('') is the empty string and toFixedString('', 32) is 32 zero
-- bytes, which is a REAL bucket here, so an unset field in a UI would
-- otherwise return it.
CREATE VIEW IF NOT EXISTS sol_launchpad_token_holders_v AS
WITH toFixedString(unhex({token:String}), 32) AS token_id
SELECT
  owner AS account,
  toFloat64(balance) AS balance_raw,
  toFloat64(balance) / greatest(
    (SELECT max(toFloat64(initial_supply)) FROM launchpad_tokens FINAL
     WHERE chain = {chain:UInt64} AND token = token_id
       AND is_deleted = 0
       AND emitter IN (
         SELECT curve FROM launchpad_trusted_curves_v
         WHERE chain = {chain:UInt64})), 1.) AS share_of_initial_supply,
  max(block_number) AS last_block
FROM sol_token_balances FINAL
WHERE chain = {chain:UInt64} AND mint = token_id
  AND is_deleted = 0 AND block_number <= {as_of_block:UInt64}
  AND length({token:String}) = 64
GROUP BY owner, balance
HAVING balance_raw > 0
ORDER BY balance_raw DESC;

-- A launch with the front end that hosted it.
--
-- The join a UI needs and the one place the "a front end is never a venue"
-- rule is expressed in SQL: this view ATTRIBUTES a launch, it does not
-- create a venue. frontend_name is empty until an operator puts the fee
-- claimer in launchpad_frontends, and the launch is still perfectly
-- readable without it.
CREATE VIEW IF NOT EXISTS sol_launchpad_attribution_v AS
SELECT
  t.chain AS chain,
  t.token AS token,
  t.family AS family,
  t.curve AS curve,
  t.creator AS creator,
  t.name AS name,
  t.symbol AS symbol,
  t.timestamp AS launch_time,
  t.block_number AS launch_block,
  -- The config account, back from the numeric column it shares with EVM.
  reverse(reinterpretAsFixedString(t.launch_config_id)) AS config,
  c.fee_claimer AS fee_claimer,
  f.name AS frontend_name,
  f.kind AS frontend_kind
FROM launchpad_tokens AS t FINAL
LEFT JOIN sol_launchpad_configs AS c FINAL
  ON c.chain = t.chain
 AND c.config = reverse(reinterpretAsFixedString(t.launch_config_id))
LEFT JOIN launchpad_frontends AS f FINAL
  ON f.chain = t.chain AND f.address = c.fee_claimer
WHERE t.is_deleted = 0;
