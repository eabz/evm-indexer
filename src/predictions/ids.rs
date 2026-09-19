//! The id arithmetic of the Gnosis Conditional Tokens Framework, offline.
//!
//! ```text
//! conditionId  = keccak256(oracle ++ questionId ++ uint256(outcomeSlotCount))
//! collectionId = point(keccak256(conditionId ++ uint256(indexSet)))   (alt_bn128, see below)
//! positionId   = uint256(keccak256(collateralToken ++ collectionId))  = the ERC-1155 token id
//! ```
//!
//! So the ERC-1155 id of "outcome `i` of condition `c` backed by collateral
//! `t`" is a pure function of `(t, c, 1 << i)`: the indexer computes the
//! outcome <-> token id mapping from a `PositionSplit` without any RPC, and
//! can VERIFY a `ConditionPreparation` instead of trusting its emitter.
//!
//! `collectionId` (CTHelpers.getCollectionId of the CTF): the hash is turned
//! into the x coordinate of a point of alt_bn128 (`y^2 = x^3 + 3`): x is
//! incremented until `x^3 + 3` is a square, the parity of `y` is taken
//! from the top bit of the hash and stored in bit 254 of the result. With a
//! parent collection the two points are ADDED - nested (deep) positions are
//! not supported here ([`collection_id`] returns `None`), no venue with
//! volume uses them.
//!
//! Checked against real `PositionSplit` / `TransferBatch` pairs of Polygon
//! (see the tests).

use alloy::primitives::{keccak256, uint, Address, B256, U256};

/// Field modulus of alt_bn128.
const P: U256 = uint!(
    21888242871839275222246405745257275088696311157297823662689037894645226208583_U256
);

/// `(P + 1) / 4`: `P = 3 mod 4`, so `a^((P+1)/4)` is a square root of `a`
/// whenever `a` is a square.
const SQRT_EXPONENT: U256 = uint!(
    5472060717959818805561601436314318772174077789324455915672259473661306552146_U256
);

/// Bound of the "increment x until it is on the curve" loop. Half of all x
/// are, so the chain itself needs ~2 rounds; 256 misses in a row do not
/// happen (p = 2^-256).
const MAX_ROUNDS: usize = 256;

/// `keccak256(abi.encodePacked(oracle, questionId, outcomeSlotCount))`.
pub fn condition_id(
    oracle: Address,
    question_id: B256,
    outcome_slot_count: U256,
) -> B256 {
    let mut packed = [0u8; 20 + 32 + 32];
    packed[..20].copy_from_slice(oracle.as_slice());
    packed[20..52].copy_from_slice(question_id.as_slice());
    packed[52..].copy_from_slice(&outcome_slot_count.to_be_bytes::<32>());
    keccak256(packed)
}

/// Collection id of `index_set` of `condition_id` directly under the
/// collateral. `None` for a nested position (`parent != 0`).
pub fn collection_id(
    parent_collection_id: B256,
    condition_id: B256,
    index_set: U256,
) -> Option<B256> {
    if !parent_collection_id.is_zero() {
        return None;
    }

    let mut packed = [0u8; 64];
    packed[..32].copy_from_slice(condition_id.as_slice());
    packed[32..].copy_from_slice(&index_set.to_be_bytes::<32>());

    let mut x = U256::from_be_bytes(keccak256(packed).0);
    let odd = x.bit(255);

    let mut y = U256::ZERO;
    let mut found = false;

    for _ in 0..MAX_ROUNDS {
        x = x.add_mod(U256::from(1u8), P);
        let yy = x.mul_mod(x.mul_mod(x, P), P).add_mod(U256::from(3u8), P);
        y = yy.pow_mod(SQRT_EXPONENT, P);

        if y.mul_mod(y, P) == yy {
            found = true;
            break;
        }
    }

    if !found {
        return None;
    }

    if odd != y.bit(0) {
        y = P - y;
    }

    if y.bit(0) {
        x ^= U256::from(1u8) << 254;
    }

    Some(B256::from(x.to_be_bytes::<32>()))
}

/// `uint256(keccak256(abi.encodePacked(collateralToken, collectionId)))`.
pub fn position_id(collateral_token: Address, collection_id: B256) -> U256 {
    let mut packed = [0u8; 52];
    packed[..20].copy_from_slice(collateral_token.as_slice());
    packed[20..].copy_from_slice(collection_id.as_slice());
    U256::from_be_bytes(keccak256(packed).0)
}

/// ERC-1155 id of the position `index_set` of `condition_id` held directly
/// against `collateral_token`.
pub fn outcome_token_id(
    collateral_token: Address,
    condition_id: B256,
    index_set: U256,
) -> Option<U256> {
    collection_id(B256::ZERO, condition_id, index_set)
        .map(|collection| position_id(collateral_token, collection))
}

/// The outcome an index set stands for when it names exactly ONE outcome
/// slot (`1 << i`); `None` for combined positions (`0b110`...).
pub fn single_outcome(index_set: U256) -> Option<u16> {
    (index_set.count_ones() == 1)
        .then(|| index_set.trailing_zeros())
        .and_then(|index| u16::try_from(index).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predictions::{events, fixtures};

    #[test]
    fn the_square_root_exponent_is_p_plus_one_over_four() {
        assert_eq!(
            (P + U256::from(1u8)) / U256::from(4u8),
            SQRT_EXPONENT
        );
        assert_eq!(P % U256::from(4u8), U256::from(3u8));
    }

    fn word(data: &[u8], index: usize) -> U256 {
        U256::from_be_slice(&data[index * 32..(index + 1) * 32])
    }

    /// Real preparations (Polygon: UmaCtfAdapter and NegRiskAdapter as
    /// oracle): the id the CTF emitted is the hash of its three inputs.
    #[test]
    fn condition_ids_match_real_preparations() {
        let mut checked = 0;

        for tx in fixtures::ALL {
            for log in tx.logs() {
                if log.topic0
                    != Some(events::CTF_CONDITION_PREPARATION.topic0)
                {
                    continue;
                }

                let oracle = Address::from_word(log.topic2.unwrap());
                let question = log.topic3.unwrap();
                let slots = word(&log.data, 0);

                assert_eq!(
                    condition_id(oracle, question, slots),
                    log.topic1.unwrap(),
                    "{}",
                    tx.hash
                );
                assert_ne!(
                    condition_id(oracle, question, slots + U256::from(1u8)),
                    log.topic1.unwrap()
                );
                checked += 1;
            }
        }

        assert!(checked >= 3, "{checked}");
    }

    /// Real splits on Polygon (USDC.e and the wrapped collateral of the
    /// NegRiskAdapter), Gnosis (WXDAI), Base and BNB Chain: the ids the
    /// registry minted right before its PositionSplit are the ids computed
    /// from (collateral, condition, partition).
    #[test]
    fn position_ids_match_real_mints() {
        let mut checked = 0;

        for tx in fixtures::ALL {
            let logs = tx.logs();

            for (index, log) in logs.iter().enumerate().skip(1) {
                let mint = &logs[index - 1];
                if log.topic0 != Some(events::CTF_POSITION_SPLIT.topic0)
                    || mint.topic0
                        != Some(events::ERC1155_TRANSFER_BATCH.topic0)
                    || mint.address != log.address
                {
                    continue;
                }

                let collateral =
                    Address::from_word(B256::from(word(&log.data, 0)));
                let condition = log.topic3.unwrap();
                let outcomes = word(&log.data, 3).to::<usize>();
                assert_eq!(word(&mint.data, 2).to::<usize>(), outcomes);

                for outcome in 0..outcomes {
                    let index_set = word(&log.data, 4 + outcome);
                    assert_eq!(
                        outcome_token_id(collateral, condition, index_set),
                        Some(word(&mint.data, 3 + outcome)),
                        "{} outcome {outcome}",
                        tx.hash
                    );
                }
                checked += 1;
            }
        }

        assert!(checked >= 6, "{checked}");
    }

    #[test]
    fn nested_positions_are_not_supported() {
        assert_eq!(
            collection_id(
                B256::repeat_byte(1),
                B256::repeat_byte(2),
                U256::from(1u8)
            ),
            None
        );
    }

    #[test]
    fn index_sets_name_one_outcome_or_none() {
        assert_eq!(single_outcome(U256::from(1u8)), Some(0));
        assert_eq!(single_outcome(U256::from(2u8)), Some(1));
        assert_eq!(single_outcome(U256::from(1u8) << 255), Some(255));
        assert_eq!(single_outcome(U256::from(3u8)), None);
        assert_eq!(single_outcome(U256::ZERO), None);
    }
}
