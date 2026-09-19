//! Reorg DETECTION (no rewind, by design - see issue #15).
//!
//! The parent hash of every streamed block is compared with the hash of
//! the block stored right before it. A mismatch means the chain was
//! reorganized after that block was indexed.

use crate::{
    db::models::block::DatabaseBlock, utils::convert::hash_to_b256,
};
use alloy::primitives::B256;
use hypersync_client::net_types::RollbackGuard;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReorgEvidence {
    /// Block whose parent hash does not match.
    pub block_number: u64,
    pub expected_parent: B256,
    pub actual_parent: B256,
}

#[derive(Debug, Default)]
pub struct ReorgDetector {
    /// Number and hash of the last block handed to the writer.
    last: Option<(u64, B256)>,
}

impl ReorgDetector {
    pub fn last(&self) -> Option<(u64, B256)> {
        self.last
    }

    /// True when the hash of block `number - 1` is known.
    pub fn knows_parent_of(&self, number: u64) -> bool {
        matches!(self.last, Some((last, _)) if last.checked_add(1) == Some(number))
    }

    /// Seeds the detector with an already stored block.
    pub fn seed(&mut self, number: u64, hash: B256) {
        self.last = Some((number, hash));
    }

    /// Checks `blocks` (ordered by number) against the last known block
    /// and against each other, then remembers the last one.
    pub fn observe(
        &mut self,
        blocks: &[DatabaseBlock],
    ) -> Vec<ReorgEvidence> {
        let mut evidence = Vec::new();

        for block in blocks {
            let number = block.number;

            if let Some((last_number, last_hash)) = self.last {
                if last_number.checked_add(1) == Some(number)
                    && block.parent_hash != last_hash
                {
                    evidence.push(ReorgEvidence {
                        block_number: number,
                        expected_parent: last_hash,
                        actual_parent: block.parent_hash,
                    });
                }
            }

            self.last = Some((number, block.hash));
        }

        evidence
    }

    /// Same check using the rollback guard of a response, which describes
    /// the unfinalized window the server scanned. Call BEFORE `observe`.
    pub fn check_guard(
        &self,
        guard: &RollbackGuard,
    ) -> Option<ReorgEvidence> {
        let (last_number, last_hash) = self.last?;

        if last_number.checked_add(1) != Some(guard.first_block_number) {
            return None;
        }

        let actual_parent = hash_to_b256(&guard.first_parent_hash);

        (actual_parent != last_hash).then_some(ReorgEvidence {
            block_number: guard.first_block_number,
            expected_parent: last_hash,
            actual_parent,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypersync_client::format::Hash;

    fn block(number: u64, hash: u8, parent: u8) -> DatabaseBlock {
        crate::db::models::block::test_support::block_row(
            number, hash, parent,
        )
    }

    #[test]
    fn a_consistent_chain_raises_nothing() {
        let mut detector = ReorgDetector::default();

        assert!(detector
            .observe(&[block(1, 1, 0), block(2, 2, 1)])
            .is_empty());
        assert!(detector.observe(&[block(3, 3, 2)]).is_empty());
        assert_eq!(detector.last(), Some((3, B256::repeat_byte(3))));
    }

    #[test]
    fn parent_mismatch_across_batches_is_detected() {
        let mut detector = ReorgDetector::default();
        detector.observe(&[block(1, 1, 0), block(2, 2, 1)]);

        // Block 3 builds on a different block 2.
        let evidence = detector.observe(&[block(3, 3, 0xee)]);

        assert_eq!(
            evidence,
            vec![ReorgEvidence {
                block_number: 3,
                expected_parent: B256::repeat_byte(2),
                actual_parent: B256::repeat_byte(0xee),
            }]
        );
    }

    #[test]
    fn non_contiguous_blocks_are_not_compared() {
        let mut detector = ReorgDetector::default();
        detector.observe(&[block(1, 1, 0)]);

        // Gap filling jumps around: block 10 does not follow block 1.
        assert!(detector.observe(&[block(10, 10, 0xee)]).is_empty());
        assert!(!detector.knows_parent_of(10));
        assert!(detector.knows_parent_of(11));
    }

    #[test]
    fn seeded_from_the_database() {
        let mut detector = ReorgDetector::default();
        detector.seed(99, B256::repeat_byte(0x99));

        assert!(detector.knows_parent_of(100));
        assert_eq!(detector.observe(&[block(100, 1, 0x98)]).len(), 1);
    }

    #[test]
    fn rollback_guard_is_checked_when_contiguous() {
        let mut detector = ReorgDetector::default();
        detector.seed(99, B256::repeat_byte(0x99));

        let guard = |first_block_number, parent: u8| RollbackGuard {
            block_number: 120,
            timestamp: 0,
            hash: Hash::from([0u8; 32]),
            first_block_number,
            first_parent_hash: Hash::from([parent; 32]),
        };

        assert!(detector.check_guard(&guard(100, 0x99)).is_none());
        assert!(detector.check_guard(&guard(100, 0x11)).is_some());
        // Guard window starts elsewhere: nothing to compare.
        assert!(detector.check_guard(&guard(90, 0x11)).is_none());
    }
}
