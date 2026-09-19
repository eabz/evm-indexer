# Checkpoint — how to pick this project up from cold

Living file, kept current by the dev lead (Claude) so a usage-limit cut never loses the
thread. Delete it in the end-of-project cleanup.

**Last updated:** 2026-09-19 19:30 (America/Mexico_City)

## Where things are

- Branch `rework/hypersync-streams`, PR #16 <https://github.com/eabz/evm-indexer/pull/16>
  (owner authorised pushing freely; push only after a green local gate: build, clippy
  `-D warnings`, fmt, tests). CI is red until the pipeline wiring compiles.
- Binding design: `docs/design.md` (never call the project "v3"). Code layout decision:
  feature modules (section 12); the move of `src/db/models` -> `src/core` is a dedicated
  refactor AFTER the pipeline wiring lands.
- Coordination board: tirith daemon for this repo on **port 7480**
  (`tirith --url http://127.0.0.1:7480/mcp status | task list | decision list | message list`).
  If it is down: `nohup tirith serve --root <repo> --bind 127.0.0.1:7480 --no-tray &`.
  State persists in `.tirith/`.
- `ENVIO_API_TOKEN` is in the git-ignored `.env`. Never print or commit it.

## Merged and verified on the branch

HyperSync streaming ingest · clean binary schema · insert-only reorg support in the
schema (tombstones + epochs + validity rule; the indexer never issues DELETE) ·
month-only partitions (50+ chains per database) · traces removed, `contracts` is a view ·
embedded versioned migrations · token worker off the commit path, `--rpc` default `auto`,
hardened against untrusted public endpoints (two-provider agreement, strict on all
chains by owner decision) · metrics module · DEX module incl. forgery hardening
(USD only for swap legs corroborated by ERC-20 transfers) · prediction markets module ·
reorg core (`src/reorg/`, proven in memory) · first live HyperSync run OK on commit
2b1e66d (10 mainnet blocks matched a public RPC).

## In flight (check before touching)

| Who | Model | Where | What | Saved how |
|---|---|---|---|---|
| module-followups | DONE, merged e49d01c | - | dex flaky test 30/30 after the harness fix; launchpads rebuild excludes the purged range | - |
| layout | DONE, merged 1271be1 | - | one code layout (feature modules); `tests/layout.rs` enforces it; 737 unit + 106 database tests green | - |
| fix-a, fix-b, fix-d | DONE, merged + pushed | - | review round 4: all 3 BLOCKERS + majors 3-10, M4-M9 fixed. Gate on the merged tree: 772 unit, database suites 116/116 (lead script: scratchpad `lead-gate.sh`, URL must be `http://default@host:port/<name>_test`) | - |
| fix-c, fix-e, fleet + control panel | DONE, merged + pushed | - | round 4 SQL + follow-ups; `indexer fleet` + `src/admin` (security review: safe to merge after fixes; report scratchpad `handoff/review-sec-report.md`) | - |
| fix-f | Opus | worktree | 5 MAJORs of the re-review (scratchpad `handoff/review-f-report.md`): Solana candle rebuild pool guard, solana verify false alarm, prediction dust floor, CLMM hop log, bounded-run stale queue | commits |
| coverage | Opus | worktree | design 16 coverage floor (default start = 1 year, `--start-date`, persisted floor, `coverage_v`, predictions registry-only pass) + 3 admin residuals. Task 75a26828 | commits |
| review-e | DONE | - | review round 4: 2 BLOCKER + 11 MAJOR + 11 MINOR; full report committed as `docs/review-round-4.md` (paths are PRE-refactor, snapshot 8c23e33) | - |

MAIN TREE IS CLEAN and pushed (HEAD 8c23e33+). EVERYTHING BELOW IS MERGED: HyperSync ingest,
clean binary schema, insert-only reorgs (tombstones + epochs + BOUNDED validity rule),
migrator (+ review fixes), token worker (+ untrusted-endpoint hardening, `--rpc` default auto),
metrics, DEX (+ forgery hardening, chain-neutral), predictions (+ review fixes, chain
neutral), EVM launchpads (+ id alignment, review fixes), reorg core, pipeline wiring +
zero-flag proof, hardening rounds 1 and 2, SQL fixes, Solana: phase 1, 10 venues, launchpads
(pump.fun / Meteora DBC / LaunchLab), `sol_dex_programs` registry, and
`indexer run --chain solana` (head follower; live 5 min: 224.8 slots/min, 141,655 swaps,
13,918 curve trades, verify CONSISTENT). Last gates: 736 unit tests; 90 database tests
suite by suite on a fresh ClickHouse (EVM acceptance 19/19, Solana acceptance 15/15).
CI on PR #16 has been green on every completed run since the pipeline wiring landed.

## Still to do, in order

OWNER DECISIONS 2026-09-19: STAY ON HYPERSYNC (QuickNode researched and rejected: `docs/quicknode-research.md`; generic RPC source parked, task f99c02d6); default start = ONE YEAR before first launch, Solana = head, no full backfill (design 16). After fix-f and coverage merge: final combined gate on a QUIET machine (lead script scratchpad `lead-gate.sh`; test server needs `max_server_memory_usage` 0), live run through `indexer fleet` (EVM + Solana, one DB), docs pass, END cleanup. The -1 and 0 items below are DONE.

-1. FOLLOW-UP ROUND after fix-c merges (one Opus engineer, Solana pipeline files): (a) `src/pipeline/solana.rs` has the same destructive stale-span drain fix-b fixed for EVM (MAJOR 3 twin) and no restart recovery - reuse `Database::stale_flush_ranges`; (b) `solana_store::timestamp_span` must ignore zero timestamps like the EVM store; (c) LEAD DECISION: `solana_verify` must report a pending repair as a problem, same as EVM verify; (d) `tombstone_until_gone` stopping on the first zero-count attempt (other half of MAJOR 7, src/reorg); (e) a discriminating test for MAJOR 9 (test-only stale-read hook). Then a short re-review (Opus, read-only) of all round 4 fixes.

0. NEW OWNER REQUEST (2026-09-19): one process syncing many chains + a password protected
   HTML control panel. Designed in `docs/design.md` section 15 (`indexer fleet`, `src/admin`).
   Two tirith tasks exist. Build AFTER fix-b merges (both touch `src/pipeline/mod.rs`), Opus,
   then an independent security review of the login/session surface before merging.
   Owner said (2026-09-19): "finish everything code related"; the Envio/hardware decisions come later.

1. Merge the layout refactor when it reports (validate, real `git merge`, remove worktree).
   Review round 4 findings will cite PRE-refactor paths (snapshot 8c23e33): map them.
2. FIX REVIEW ROUND 4 (`docs/review-round-4.md`) - launch AFTER the layout refactor merges (same files), all Opus, three engineers with disjoint files: (A) Solana pipeline `src/pipeline/solana*.rs` + `src/source/solana.rs`: BLOCKER 1 (heal must purge holes that hold live `sol_slots`; checkpoints are not a safe resume oracle), BLOCKER 2 (compact checkpoints + page the tiling, no LIMIT), MAJOR 4, 7, 8, 9, 10; (B) core `src/pipeline/{mod,store,backfill,verify}.rs`, `src/reorg`, `src/db`: MAJOR 3 (stale-span queue drained destructively - also closes open task 5e52af12), 5 (`reason != 'redecode'` in the debris rule), 6 (clamp/refuse `from_ts` = 0), MINOR 14-18, 24; (C) SQL + svm `migrations/0031,0042,0043`, `src/svm`: MAJOR 11 (holders view), 12 (position-space tombstone for `sol_token_balances`), 13 (dust guard on Solana + launchpad candles), MINOR 19-23. (D) Solana DECODER `src/svm/{decode,venues,events,launchpads,registry,programs}.rs` + `migrations/0041/0042` (ADDENDUM of the report): BLOCKER B3 (`pool_id` = a program-wide vault authority for Raydium v4/CPMM, DAMM v2, DBC, LaunchLab on movement-confidence rows -> all pairs collapse into one candle series: never store an authority as pool_id), M4 (`amount_out` takes a fee recipient's delta when the taker account is unreadable), M5 (two-hop instructions lose hops; ordinal needs a sub-index), M6 (`trader` = fee payer; use the event's user like launchpads does), M7 (`has_dropped_log_messages` never consulted), M8 (fee_amount sums across mints), M9 (register more routers), the minors; (C) and (D) both touch src/svm: give (C) only migrations 0031/0043 + `sol_token_balances`, (D) the decoder. Each fix = failing test first, real transaction signatures from the report as fixtures. Then a short re-review of the fixes. DO NOT run the Solana indexer for more than a few days, and do not backfill, before BLOCKERS 1, 2, B3 are fixed.
   (previous text of this item:) Review round 4 (independent, read-only, Opus): hardening rounds 1+2 (bounded validity
   rule, side-table repair, checkpoint compaction, month-split flush, lease fencing), the
   whole Solana path (`src/svm/**`, `src/source/solana.rs`, `src/pipeline/solana*.rs`),
   the SQL fixes. Route findings to fresh Opus engineers.
3. Known open items (tirith tasks exist):
   - ~~Solana flush latency~~ MEASURED (fix-c, round 4). `svm::profile::flush_cost_per_table`
     is the benchmark: fresh ClickHouse, the real insert path, per-table timings, batch size
     and flush count from `FLUSH_BENCH_COPIES` / `FLUSH_BENCH_FLUSHES`. At the DEFAULT
     `--flush-rows 100000` a flush costs ~250 ms on an M-series laptop with a local server,
     flat over 400 consecutive flushes and linear in rows (~2.5 us/row), so the 0.6-5.9 s
     seen live is that same cost on a loaded host - not a pathology. WHERE it goes:
     `sol_dex_swaps` 158 ms of the 250 (its three candle MVs are 105 ms of that, measured
     against a view-free copy of the table), `launchpad_trades` 39 ms (its five views 26 ms),
     everything else under 15 ms. The PRIME SUSPECT IS WRONG: `sol_token_balances`
     partitioned by chain and partitioned by month cost the same 14 ms for the same rows.
     The lever, if the flush ever has to be cheaper, is the candle MVs (chain 1h/1d off the
     1m table instead of re-reading the swap block three times) or a smaller `--flush-rows`;
     both are design decisions, neither was taken.
   - ~~`sol_token_balances` uses the POSITION as `_version`~~ FIXED (round 4, MAJOR 12): it
     is an append log of observations now, an ordinary purge child and seeded version table.
   - The sink's queue of flush spans that raced another process's purge is in memory only
     (task 5e52af12): persist it or verify the days of the newest `reorgs` rows at startup.
   - Solana history backfill driver (blocked on OWNER DECISION: Envio Starter $70 for one
     month ~13 days vs free 48 days / ~11 weeks), nightly sample re-fetch for verify,
     `sol_tokens.program` overwrite, DBC/LaunchLab graduations name no destination pool.
   - Predictions: market-list cost only partly bounded (needs `market_id` denormalised onto
     `prediction_trades`); four.meme launchpad family not built.
4. Layout refactor to feature modules (design section 12): `src/db/models` -> `src/core`,
   `src/utils` gone, `src/db` infrastructure only. Mechanical, no behaviour change, ONE
   engineer, nobody else editing at the same time. Task 59f32d7e.
5. Final combined gate: all unit tests + ALL database tests (fresh ClickHouse per suite).
6. Live end-to-end run of the FINAL binary with zero flags on an EVM chain (DEX rows +
   token metadata through `--rpc auto`), tune the HyperSync `StreamConfig`; and Solana
   next to it in the same database.
7. Docs final pass: README (Solana operator section exists from solana-run; check flag
   table vs `src/configs`), compose example with a Solana service, CI integration filter
   should include predictions/launchpads/svm/pipeline acceptance suites.
8. END cleanup (owner request): delete research docs (`perps-research.md`,
   `launchpads-research.md`, `solana-research.md`, `data-model-proposals.md`) and this file
   after folding their DECISIONS into `docs/design.md`; `docs/` keeps decisions only.
OWNER DECISIONS WAITING (tirith task 115edb17): Envio $70 month for the Solana backfill;
history depth (accept 2026-01-03 start - lead recommends yes); hardware (~8 TB NVMe,
64-128 GB RAM for year one). Perps are deferred by the owner.

## Standing instructions from the owner (2026-09-18, before going offline)

"Continue the process; if Solana phase 1 finishes, continue until everything is ready."
So, without asking: validate + merge + push each finished stream; launch Solana phase 2
when phase 1 lands; send review findings back for fixes; then layout refactor, final
combined gate, live end-to-end run, docs final pass; research cleanup LAST. Decisions
only the owner can make (none open right now) wait on the tirith board. Stop only at the
usage thresholds below.

## Usage policy (owner, 2026-09-18)

Fable only for the lead and the pipeline engineer (or something that truly needs it);
everything else on Opus. The lead checks plan usage every time it is active. **Urgent
threshold: weekly Fable >= 97% or 5-hour >= 90% (owner, checked every 5 minutes)** -> stop all agents, update this file,
commit and push it, and stop.
