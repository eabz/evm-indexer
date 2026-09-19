//! Limits a PROVIDER imposes on a TOKEN, shared by every chain in the
//! process (docs/design.md section 15).
//!
//! The rule the fleet has to respect: one HyperSync token serves all
//! chains, and Envio meters the token. A supervisor that gave every chain
//! its own budget would multiply the allowance by the number of chains and
//! collect 429s.
//!
//! Two budgets live here, and they are not the same kind of thing:
//!
//! * **Solana queries.** Metered, per token, documented (30 a minute on the
//!   free tier). [`pipeline::solana::Budget`](crate::pipeline::solana::Budget)
//!   already implements the governor; the fleet simply builds ONE and hands
//!   the same handle to every Solana chain.
//! * **Memory.** Every chain buffers rows before it writes them. The sum,
//!   not the per-chain number, is what makes the process run out of memory,
//!   so the fleet divides one cap over the chains that are running: adding
//!   a chain makes everybody's write batches smaller instead of growing the
//!   process.
//!
//! **The EVM query rate is NOT budgeted here, on purpose.** See
//! `src/fleet/README.md`, "What is not shared yet".

use crate::pipeline::solana::Budget;
use std::sync::Arc;

/// Rough memory one buffered row costs, averaged over the tables an EVM
/// chain writes (a log with its data blob is far bigger than a withdrawal).
/// It only has to be the right order of magnitude: it turns a number in
/// megabytes, which an operator can reason about, into a row count, which
/// the writer understands.
const BYTES_PER_BUFFERED_ROW: u64 = 512;

/// Never squeeze a chain below this: a flush of a handful of rows makes one
/// ClickHouse part per handful, which is worse than using a little more
/// memory.
const MIN_FLUSH_ROWS: usize = 1_000;

/// The provider limits of one fleet process.
pub struct Budgets {
    /// Shared by every Solana chain in the process.
    solana: Arc<Budget>,
    max_inflight_mb: u64,
}

impl Budgets {
    pub fn new(
        max_inflight_mb: u64,
        solana_queries_per_minute: u32,
    ) -> Self {
        Self {
            solana: Arc::new(Budget::new(solana_queries_per_minute)),
            max_inflight_mb: max_inflight_mb.max(1),
        }
    }

    /// The ONE Solana query budget. Every Solana chain in the process gets
    /// this same handle, so together they stay inside the token's limit.
    pub fn solana(&self) -> Arc<Budget> {
        self.solana.clone()
    }

    /// The rows one chain may buffer when `running` chains share the cap.
    pub fn flush_rows(&self, running: usize) -> usize {
        let running =
            u64::from(u32::try_from(running.max(1)).unwrap_or(1));
        let rows = self.max_inflight_mb * 1024 * 1024
            / BYTES_PER_BUFFERED_ROW
            / running;

        usize::try_from(rows).unwrap_or(usize::MAX).max(MIN_FLUSH_ROWS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_solana_chain_draws_from_one_budget() {
        let budgets = Budgets::new(1_024, 25);
        let a = budgets.solana();
        let b = budgets.solana();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn one_more_chain_makes_every_chain_buffer_less() {
        let budgets = Budgets::new(2_048, 25);

        let alone = budgets.flush_rows(1);
        let four = budgets.flush_rows(4);

        assert_eq!(alone, 2_048 * 1024 * 1024 / 512);
        assert_eq!(four, alone / 4);
        assert!(four < alone);
    }

    #[test]
    fn a_crowded_fleet_never_flushes_a_handful_of_rows() {
        let budgets = Budgets::new(1, 25);
        assert_eq!(budgets.flush_rows(10_000), MIN_FLUSH_ROWS);
        // And a nonsense chain count is not a division by zero.
        assert!(budgets.flush_rows(0) >= MIN_FLUSH_ROWS);
    }
}
