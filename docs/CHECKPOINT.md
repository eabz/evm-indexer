# Checkpoint — how to pick this project up from cold

Living file, kept current by the dev lead (Claude) so a usage-limit cut never loses the
thread. Delete it in the end-of-project cleanup.

**Last updated:** 2026-09-19 04:45 (America/Mexico_City)

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
| hardening (round 2) | Opus | MAIN tree (uncommitted edits possible) | flaky-under-load root cause, bounded rebuild / interval validity rule, checkpoint compaction, month-split flush, epoch-moves-mid-flush test, purged range into dex/launchpads rebuild SQL | commits `hardening:` + tirith notes |
| solana-launchpads | Opus | worktree (see `git worktree list`) | Solana launchpads into `launchpad_*` (pump.fun, Meteora DBC, LaunchLab), pump.fun 1.8-3.8% disagreement, `sol_dex_programs` registry, live proof | commits + tirith notes |
| solana-run | DONE, NOT MERGED | worktree `agent-a703082e8726f3841` (4 commits + a merge of the branch) | BLOCKED on an interface drift: hardening round 2 changed `reorg::ReorgStore` (`min_timestamp` gone, `rebuild_derived` 7 params) and `src/pipeline/solana_store.rs` implements the old trait. When hardening finishes and posts its trait-change list, resume solana-run (or a fresh Opus agent in that worktree) to adapt, re-run its 13 acceptance scenarios, then merge. Live proof already done: 10 min head following, 202,105 swaps, 6.2 queries/min, 20/20 vs public RPC, restart resumes exactly. | commits in the worktree |
| (old row) solana-run | Opus | worktree | `indexer run --chain solana`: head follower in NEW `src/pipeline/solana*.rs`, `sol_slots` commit marker, block_height contiguity, tripwire, lease, verify, acceptance tests + 10 min live run | commits + tirith notes |

MAIN TREE IS CLEAN and pushed (HEAD 7a4487e+). NO worktrees exist. Everything below is merged: pipeline wiring +
zero-flag proof, migrator fixes, launchpads (+ wiring, id alignment, review fixes), chain
neutral DEX + predictions (+ review round 2 fixes), Solana phase 1, reorg core, hardening
round 1 (side-table orphan repair, shrinking chain, lease fencing, review round 3 blockers
and majors), SQL fixes. Last gates: 662 unit tests; 78/78 ClickHouse tests serial on a fresh
server (hardening engineer), plus the lead's targeted runs at each merge.

NEXT TO LAUNCH (all Opus), when the 5-hour window allows:
1. hardening round 2 (tirith task 64ed4968, still in_progress): bounded rebuild / interval
   validity rule, checkpoint compaction, month-splitting a flush over 100 partitions, test
   for the epoch moving mid-flush, root cause of the tests that fail once under load
   (`eight_chains...`, `hostile_amounts...`), pass the purged range into dex + launchpads
   rebuild SQL.
2. Solana launchpads into `launchpad_*` (pump.fun curve, Meteora DBC, LaunchLab); also the
   pump.fun decoder disagreeing with the movement layer on 1.8-3.8% of curve trades, and a
   curated `sol_dex_programs` registry for the prop AMMs (~32% of Solana volume: movement
   layer decodes them, promoting a program to a venue is a false-positive decision).
3. `indexer run --chain solana`: head follower, `sol_slots` commit marker, contiguity by
   `block_height`/parent chain, reorg handling off with a parent-hash tripwire, chain
   registration via `svm::REGISTER_CHAIN_SQL` (plan: docs/solana-research.md section 11).
4. Review round 4 (hardening round, Solana, SQL fixes). 5. Layout refactor (design 12) -
   LAST among code changes, it moves files everyone else edits. 6. Final combined gate,
   live end-to-end run, docs final pass, research cleanup.
OWNER DECISIONS WAITING: tirith task 115edb17 (Envio $70 month for the Solana backfill,
history depth, hardware).

Briefs for the four Opus agents: `<session scratchpad>/handoff/*.md` (original brief +
lead follow-ups + progress notes). Agent sessions do not survive a cut, but a fresh agent
given the handoff file plus `git status`/`git log` of the worktree can continue.

## Still to do, in order

1. DONE: pipeline wiring (zero-flag proof, 8/8 acceptance tests, review round 2 core fixes). DONE: migrator review fixes merged. DONE: review round 2 (findings routed; predictions fixes are with predictions-neutral). Open backlog: tirith tasks 'Pipeline hardening backlog' and 'Live validation'.
2. Merge launchpads + migrator fixes (validate each in its worktree against the current
   branch first, then a real `git merge`, then remove the worktree).
3. Review round 2 findings -> fixes. Then review the pipeline wiring itself.
4. Layout refactor to feature modules (design section 12).
5. Final combined gate: all unit tests + ALL ClickHouse integration tests in parallel.
6. Live end-to-end run on the final binary (zero flags: DEX rows + token metadata),
   tune the HyperSync `StreamConfig`.
7. Docs final pass (README flag table vs real CLI; single-provider RPC note).
8. Owner decided 2026-09-18: analytics tables chain-neutral NOW (design 13) and Solana is a GO (design 14). Merge order after pipeline wiring: dex-neutral -> predictions-neutral -> launchpads -> solana phase 1, each validated in its worktree against the current branch first. Then Solana phase 2 (Raydium/Orca/Meteora decoders, launchpads on Solana, head follower + reorg variant, history backfill). Perps are deferred.
9. END cleanup: delete research docs (`perps-`, `launchpads-`, `solana-research.md`,
   `data-model-proposals.md`) and this file; `docs/` keeps decisions only.

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
