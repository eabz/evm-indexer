# The core EVM dataset (docs/design.md §12)

Everything an EVM chain gives you without interpreting anybody's contract:
blocks, transactions, logs, withdrawals, and the ERC-20 / ERC-721 / ERC-1155
transfers that are decoded out of the logs by signature alone. No registry, no
allow list, no chain specific code - a chain is a chain id and a HyperSync
endpoint.

This is a DATA MODULE like `dex`, `predictions` and `launchpads`: it owns its
rows, its event signatures, its decoding, its table constants and its
aggregates. `db` owns none of them.

Migration range: **`0001-0004`**.

## Files

| file | what is in it |
|---|---|
| `mod.rs` | the public surface: `BASE_TABLES`, `SIDE_TABLES`, `CORE_DERIVED`, `decode`, and `RowBatch` + `store` (the flush) |
| `models/` | one file per table, field names = column names |
| `events.rs` | the three `Transfer` signatures, as `keccak` constants |
| `convert.rs` | HyperSync wire types -> alloy primitives, saturating, never panicking |
| `decode.rs` | response -> rows: every `from_hypersync`, the transfer decoding, and `decode()` which joins one response into a `RowBatch` |
| `derived.rs` | `CORE_DERIVED`: the aggregates of `0003` and the SQL that rebuilds them after a purge |

There is no `integration_tests.rs` here. The ClickHouse suite of this dataset is
`db::integration_tests`: it drives the INFRASTRUCTURE - the insert path,
tombstones, the validity rule, epochs, missing ranges - and core is the only
dataset that write path has rows for, so the two share one fixture and are not
separable. Everything in this module that needs no server is a unit test next to
the code.

## Tables

`BASE_TABLES` - written directly by a flush, tombstoned by a purge in this order
(children first, the commit marker LAST):

| table | row | notes |
|---|---|---|
| `erc20_transfers` | `DatabaseERC20Transfer` | `Transfer` with 3 topics |
| `erc721_transfers` | `DatabaseERC721Transfer` | the same `topic0` with 4 topics: the id is indexed |
| `erc1155_transfers` | `DatabaseERC1155Transfer` | `TransferSingle` is stored as one-element arrays |
| `logs` | `DatabaseLog` | topics are NOT nullable in SQL; `topic_count` is what tells "three topics" from "four, the last one zero" |
| `withdrawals` | `DatabaseWithdrawal` | post-Shanghai, from the block header |
| `transactions` | `DatabaseTransaction` | receipt fields included; `contracts` is a VIEW over the successful deployments here, not a table |
| `blocks` | `DatabaseBlock` | **the commit marker**: written last, so a block row exists only once all of its children are durable |

`SIDE_TABLES` - never written or tombstoned directly. Materialized views of
`0002` feed them and pass `_version`, `is_deleted` and `epoch` through, so a
tombstone in the base table kills exactly the side rows of that base row:
`tx_lookup`, `block_lookup`, `transactions_by_address`, `logs_by_address`,
`erc20_transfers_by_account`, `nft_transfers_by_account`.

`CORE_DERIVED` - the `AggregatingMergeTree` aggregates of `0003`, each fed by
one materialized view: `daily_block_stats`, `daily_transaction_stats`,
`daily_erc20_transfer_stats`. **Read them through their `*_v` view**
(`0004`), never directly: the view applies the epoch validity rule of §2.
Amounts are aggregated as `Float64`, never as raw `UInt256` sums - a unit test
rejects any `sum(` over a 256-bit column.

Not in any of those lists, on purpose: `tokens` (chain state, not block scoped -
it belongs to `src/tokens`), `reorgs` / `checkpoints` (infrastructure, `db`) and
the `contracts` view.

## Conventions

- **Nothing narrows silently.** The columns are as wide as the protocol
  (`UInt64` numbers and gas, `UInt256` amounts and prices). Where a column is
  narrower than the wire type, `convert.rs` SATURATES and logs once per process
  and per width: a saturated value is wrong data and says so.
- **A missing field is not an error, a missing identity is.** `number` + `hash`
  on a block and `hash` on a transaction are the row's identity and the commit
  marker, so they fail loudly; everything else falls back to a zero value or
  NULL.
- **Log data is attacker controlled.** Every `from_log` returns `Option` and
  nothing in it can panic, allocate on a claimed length, or read out of bounds -
  a hostile token emitting a malformed `TransferBatch` is skipped, not fatal.
- **Nothing is ever deleted** (§2): a purge INSERTs tombstones into
  `BASE_TABLES`, the side tables follow through their views, and the aggregates
  are repaired per `epoch` from `CORE_DERIVED`.
- `_version` and `epoch` are stamped once per flush on every row of the batch
  (`RowBatch::set_version` / `set_epoch`), module rows included.
