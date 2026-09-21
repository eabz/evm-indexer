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
-- substring() is NOT decoration, but the reason is narrower than it looks.
-- What trims trailing zero bytes is turning a FixedString into a String
-- EXPLICITLY: toString(id) and CAST(id AS String). Verified on ClickHouse
-- 25.12.1.322,
--   length(toString(toFixedString(unhex('0102030000'), 5)))  = 3   (trimmed)
--   length(substring(toFixedString(unhex('0102030000'),5),1,5)) = 5 (kept)
--   length(concat(toFixedString(unhex('0102030000'), 5), '')) = 5   (kept)
-- so anything that routes an id through toString / CAST shortens a pubkey
-- whose last bytes are zero, and it would print as a different key.
--
-- base58Encode(id) does NOT do that. It takes the FixedString directly and
-- keeps every byte, verified on the same build:
--   base58Encode(toFixedString(unhex('0102030000'), 5))          = '7bWp9m'
--   base58Encode(substring(toFixedString(unhex('0102030000'),5), 1, 5))
--                                                                = '7bWp9m'
-- ('Ldp' is what the TRIMMED three bytes encode to - the value you get by
-- writing base58Encode(toString(id)), not base58Encode(id). An earlier
-- version of this header attributed 'Ldp' to base58Encode(id) itself and
-- was wrong.) hex(id) keeps every byte for the same reason, which is why
-- hex(substring(id, 13)) is safe.
--
-- The substring() form above stays MANDATORY anyway, and not as a
-- workaround: it is what makes the intent explicit and checkable at the
-- call site - 'all 32 bytes' on the svm branch, 'the low 20' on the evm
-- one - and it does not depend on which conversions a future ClickHouse
-- build decides to trim. Copy it verbatim, and never write
-- base58Encode(id) bare on the strength of the note above.
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
