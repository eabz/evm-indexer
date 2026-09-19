# Checkpoint — how to pick this project up from cold

Living file, kept current by the dev lead (Claude) so a usage-limit cut never loses the
thread. Delete it in the end-of-project cleanup.

**Last updated:** 2026-09-19 00:20 (America/Mexico_City)

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
| pipeline | DONE (HEAD 8d39855, pushed) | main tree, clean | wire tokens/DEX/predictions/metrics/reorg into the running binary, checkpoints, insert dedup (migration 0090), `indexer verify` / `backfill`, zero-flag acceptance tests | commits prefixed `pipeline:` when the tree compiles; WIP notes on tirith |
| launchpads | Opus | worktree `.claude/worktrees/agent-ab96eb1af8d691753` | `src/launchpads/` (Pons V2, Flap Portal; design section 11) | commits in the worktree + tirith notes |
| migrator | Opus | worktree `.claude/worktrees/agent-a81326682e269a32c` | review fixes 14-19 for the migration runner | commits in the worktree |
| solana-research | done | `docs/solana-research.md` | finished and committed | - |
| dex-neutral | Opus | new worktree (see `git worktree list`) | DEX tables chain-neutral (design 13) + shared `SerId32`/`SerTxId` + `chains` registry (0006) | commits + tirith notes |
| predictions-neutral | Opus | new worktree | predictions tables chain-neutral (design 13) | commits + tirith notes |
| solana | Opus | new worktree | Solana phase 1: `src/svm/`, `src/source/solana.rs`, migrations 0040+, generic token-movement decoder + PumpSwap/pump.fun, live proof (design 14, `docs/solana-research.md`) | commits + tirith notes |
| review-c | Opus | read-only | review round 2: predictions, reorg proof, core schema SQL | findings sent to `lead` on tirith |

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
