-- Chain registry (docs/design.md section 13).
--
-- The analytics data modules (dex_*, launchpad_*, prediction_*) store every
-- identity - pool, token, trader, emitter, factory, recipient, tx_from... -
-- as a chain neutral FixedString(32):
--
--   EVM address   12 zero bytes + the 20 address bytes
--   Solana pubkey 32 raw bytes
--
-- The bytes alone do not say how to PRINT an id, and nothing in them can:
-- a Solana pubkey whose first 12 bytes happen to be zero is a valid pubkey.
-- This table is the mapping, user / indexer populated: no rows ship, and the
-- analytics tables never join it (a missing row costs formatting, not data).
--
-- family is 'evm' or 'svm'. Register a chain once:
--
--   INSERT INTO chains (chain, name, family) VALUES
--     (1, 'ethereum', 'evm'),
--     (8453, 'base', 'evm'),
--     (1399811149, 'solana', 'svm');
--
-- Correct a row by inserting it again (ReplacingMergeTree keeps the newest
-- _version), and read the registry through chains_v.
--
-- THE expression that prints an id, the only one any view / query should
-- copy (`id` is the FixedString(32) column, `family` comes from chains_v):
--
--   if(family = 'svm',
--      base58Encode(substring(id, 1, 32)),
--      concat('0x', lower(hex(substring(id, 13)))))
--
-- substring() is NOT decoration. Converting a FixedString to String -
-- toString(id), CAST(id AS String), and the implicit conversion
-- base58Encode(id) performs - TRIMS TRAILING ZERO BYTES, so base58Encode(id)
-- silently encodes a shortened pubkey: verified on ClickHouse 25.12,
-- base58Encode(toFixedString(unhex('0102030000'), 5)) = 'Ldp' (3 bytes)
-- while the correct answer, base58Encode(substring(id, 1, 5)), is '7bWp9m'.
-- substring() and concat(id, '') keep every byte. hex(substring(id, 13))
-- is safe for the same reason.
--
-- A pool id is NOT an address even on EVM (a Uniswap V4 / Balancer pool id
-- is a native 32 byte value), so dex_pools_v.pool prints all 32 bytes and
-- must not go through the 'evm' branch.
--
-- Deliberately NOT a ClickHouse UDF (CREATE FUNCTION): a UDF is a server
-- global object, outside the database of the connection, and would outlive
-- the database it was created for and leak between tenants. Migrations only
-- create objects inside their own database.

CREATE TABLE IF NOT EXISTS chains (
  chain UInt64,
  name String,
  family LowCardinality(String),
  _version UInt64 DEFAULT toUnixTimestamp64Milli(now64(3))
)
ENGINE = ReplacingMergeTree(_version)
ORDER BY chain;

-- The registry as readers should see it: newest row per chain, no FINAL to
-- remember. Join it to format ids with the expression above.
CREATE VIEW IF NOT EXISTS chains_v AS
SELECT chain, name, family
FROM chains FINAL;
