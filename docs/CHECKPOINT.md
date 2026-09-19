# Checkpoint — how to pick this project up from cold

Living file, kept current by the dev lead (Claude) so a usage-limit cut never loses the
thread. Delete it in the end-of-project cleanup.

**Last updated:** 2026-09-19 06:50 (America/Mexico_City)

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
| module-followups | Opus | worktree `agent-afd2d96e1d5a702bb` | dex/predictions/launchpads/svm test harness re-read loops, launchpads rebuild SQL excludes the purged range, README signatures (hand-overs from hardening round 2: `<scratchpad>/handoff/hardening2-report.md`) | commits + tirith notes |

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

1. Merge module-followups when it reports (validate in its worktree against the branch,
   real `git merge`, remove the worktree).
2. Review round 4 (independent, read-only, Opus): hardening rounds 1+2 (bounded validity
   rule, side-table repair, checkpoint compaction, month-split flush, lease fencing), the
   whole Solana path (`src/svm/**`, `src/source/solana.rs`, `src/pipeline/solana*.rs`),
   the SQL fixes. Route findings to fresh Opus engineers.
3. Known open items (tirith tasks exist):
   - Solana flush latency went from ~50 ms to 0.6-5.9 s once launchpad tables + their ten
     MVs joined the flush; `sol_token_balances` is `PARTITION BY chain` (one partition for
     all of Solana, merged on every insert) - prime suspect. Fix BEFORE any history backfill.
   - `sol_token_balances` uses the POSITION as `_version`: it is excluded from tombstoning
     and from `Database::seed_version` (a clock version can never outrank it). Confirm the
     design is sound in review round 4.
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
