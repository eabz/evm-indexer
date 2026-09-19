//! Corroboration of swap events against ERC-20 `Transfer`s of the same
//! transaction. Pure: it only looks at the logs `decode` was given (a batch
//! always holds whole blocks, hence whole transactions).
//!
//! A swap event is a CLAIM of whoever emitted it, and anyone can deploy a
//! contract that emits swap shaped events. A `Transfer` emitted by a token
//! contract is a claim of THAT token. So a leg of a swap is **verified**
//! when, in the same transaction, the token itself reports a
//! transfer of exactly the leg's amount to the emitter (in leg) or from the
//! emitter (out leg). The verified leg carries the token's address: token
//! identity and USD valuation of a verified swap need no pool metadata, no
//! RPC and no registry.
//!
//! The rule, exactly:
//!
//! * candidates = ERC-20 `Transfer` logs (3 topics, 32 data bytes) of the
//!   swap's transaction, amount `==` the leg amount, `to == emitter` (in) /
//!   `from == emitter` (out), emitted by a contract other than the emitter,
//!   not yet used by an earlier swap leg, and - when the event names the
//!   token (Balancer) - emitted by it. Pools that are their own contract
//!   move the tokens BEFORE they emit the swap, so only transfers with a
//!   smaller log index count; the singletons (Balancer Vault, Uniswap V4
//!   PoolManager) settle AFTER their swap events, so any position counts;
//! * candidates of more than one token => the leg stays unverified
//!   (ambiguity is never resolved by guessing);
//! * otherwise the latest candidate is consumed and its emitter is the
//!   verified token. One transfer verifies one leg: a contract can not
//!   emit a thousand swaps over a single real transfer.
//!
//! What this proves: the token moved, in that amount, to / from the
//! emitter. What it does NOT prove: that the movement was a trade at a
//! market price. The owner of a fake pool can move real tokens through it
//! (flash loans make that free) - wash volume and wash prices stay
//! possible, as they are on real pools. See README, "What is proven".
//!
//! Not verifiable by construction: native coin legs (no `Transfer`),
//! fee-on-transfer legs whose `Transfer` amount differs from the pool's
//! accounting (exact match only, no tolerance), V2 legs with both an in and
//! an out amount on the same token, and singleton legs that are netted
//! across hops (Balancer batch swaps: only the first in and the last out
//! reach the Vault; Uniswap V4: one settlement per currency and
//! transaction). Unverified means unpriced, never a guess.

use std::collections::HashMap;

use alloy::primitives::{Address, B256, U256};

use crate::{
    db::format::{address_of_id32, tx_hash_of},
    db::models::log::DatabaseLog,
    utils::events::TRANSFER_EVENT_SIGNATURE,
};

use super::{
    decode::topics_of,
    events,
    models::{DexSwap, Protocol},
};

struct TransferSeen {
    token: Address,
    from: Address,
    to: Address,
    amount: U256,
    log_index: u64,
    used: bool,
}

struct SyncSeen {
    emitter: Address,
    log_index: u64,
    reserve0: U256,
    reserve1: U256,
}

#[derive(Default)]
struct TxEvidence {
    transfers: Vec<TransferSeen>,
    syncs: Vec<SyncSeen>,
}

/// Transfers and reserve snapshots of a batch, by transaction.
#[derive(Default)]
pub struct Evidence {
    by_transaction: HashMap<B256, TxEvidence>,
}

/// An indexed `address` topic: the 12 leading bytes must be zero. Same rule
/// as the chain neutral id encoding, so it shares its helper.
fn clean_address(word: &B256) -> Option<Address> {
    address_of_id32(*word)
}

impl Evidence {
    pub fn collect(logs: &[DatabaseLog]) -> Self {
        let mut evidence = Evidence::default();

        for log in logs {
            let topics = topics_of(log);
            let data: &[u8] = log.data.as_ref();
            let log_index: u64 = log.log_index.into();

            if topics.first() == TRANSFER_EVENT_SIGNATURE
                && topics.len() == 3
                && data.len() == 32
            {
                let (Some(from), Some(to)) = (
                    clean_address(&topics.at(1)),
                    clean_address(&topics.at(2)),
                ) else {
                    continue;
                };

                evidence
                    .by_transaction
                    .entry(log.transaction_hash)
                    .or_default()
                    .transfers
                    .push(TransferSeen {
                        token: log.address,
                        from,
                        to,
                        amount: U256::from_be_slice(data),
                        log_index,
                        used: false,
                    });
            } else if (topics.first() == events::V2_SYNC.topic0
                || topics.first() == events::SOLIDLY_SYNC.topic0)
                && topics.len() == 1
                && data.len() == 64
            {
                evidence
                    .by_transaction
                    .entry(log.transaction_hash)
                    .or_default()
                    .syncs
                    .push(SyncSeen {
                        emitter: log.address,
                        log_index,
                        reserve0: U256::from_be_slice(&data[..32]),
                        reserve1: U256::from_be_slice(&data[32..]),
                    });
            }
        }

        evidence
    }

    /// Fills `verified_in` / `verified_out` and `reserve0` / `reserve1`.
    pub fn apply(&mut self, swaps: &mut [DexSwap]) {
        // Transfers are consumed in chain order whatever the input order.
        let mut order: Vec<usize> = (0..swaps.len()).collect();
        order.sort_by_key(|&index| {
            (
                swaps[index].block_number,
                swaps[index].tx_index,
                swaps[index].ordinal,
            )
        });

        for index in order {
            let swap = &mut swaps[index];

            // Corroboration is an EVM ERC-20 rule, so the evidence is keyed
            // by the 32 byte transaction hash the logs carry. A tx_id that
            // is not 32 bytes can not have come from this decoder.
            let Some(hash) = tx_hash_of(&swap.tx_id) else { continue };
            let Some(transaction) = self.by_transaction.get_mut(&hash)
            else {
                continue;
            };

            let position = swap.ordinal;
            // Contract pools transfer first and emit last; singletons
            // emit first and settle later.
            let before = if swap.protocol.is_singleton() {
                u64::MAX
            } else {
                position
            };
            let named =
                |token: Address| (!token.is_zero()).then_some(token);

            swap.verified_in = transaction.claim(
                swap.emitter,
                swap.amount_in,
                true,
                before,
                named(swap.token_in),
            );
            swap.verified_out = transaction.claim(
                swap.emitter,
                swap.amount_out,
                false,
                before,
                named(swap.token_out),
            );

            // A token can not be swapped for itself.
            if !swap.verified_in.is_zero()
                && swap.verified_in == swap.verified_out
            {
                swap.verified_in = Address::ZERO;
                swap.verified_out = Address::ZERO;
            }

            if matches!(
                swap.protocol,
                Protocol::UniswapV2 | Protocol::Solidly
            ) {
                let sync = transaction
                    .syncs
                    .iter()
                    .filter(|sync| {
                        sync.emitter == swap.emitter
                            && sync.log_index < position
                    })
                    .max_by_key(|sync| sync.log_index);

                if let Some(sync) = sync {
                    swap.reserve0 = sync.reserve0;
                    swap.reserve1 = sync.reserve1;
                }
            }
        }
    }
}

impl TxEvidence {
    fn claim(
        &mut self,
        emitter: Address,
        amount: U256,
        incoming: bool,
        before: u64,
        named: Option<Address>,
    ) -> Address {
        if amount.is_zero() {
            return Address::ZERO;
        }

        let mut chosen: Option<usize> = None;

        for (index, transfer) in self.transfers.iter().enumerate() {
            let counterparty =
                if incoming { transfer.to } else { transfer.from };

            if transfer.used
                || transfer.log_index >= before
                || transfer.amount != amount
                || counterparty != emitter
                || transfer.from == transfer.to
                || transfer.token == emitter
                || named.is_some_and(|token| token != transfer.token)
            {
                continue;
            }

            match chosen {
                Some(current)
                    if self.transfers[current].token != transfer.token =>
                {
                    // Two tokens claim the same leg: prove nothing.
                    return Address::ZERO;
                }
                Some(current)
                    if self.transfers[current].log_index
                        > transfer.log_index => {}
                _ => chosen = Some(index),
            }
        }

        match chosen {
            Some(index) => {
                self.transfers[index].used = true;
                self.transfers[index].token
            }
            None => Address::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, B256, I256, U256};

    use crate::{
        db::models::log::DatabaseLog,
        dex::{
            decode, events,
            fixtures::{self, address, build, same_transaction, transfer},
            models::Protocol,
        },
    };

    fn word(value: u128) -> Vec<u8> {
        U256::from(value).to_be_bytes::<32>().to_vec()
    }

    /// A V2 shaped swap of `PAIR`: side 0 pays in, side 1 pays out.
    fn v2_swap(
        amount_in: u128,
        amount_out: u128,
        index: u16,
    ) -> DatabaseLog {
        build(
            PAIR,
            &[
                events::V2_SWAP.topic0,
                Address::repeat_byte(0x70).into_word(),
                USER.into_word(),
            ],
            [word(amount_in), word(0), word(0), word(amount_out)].concat(),
            100,
            index,
            1_700_000_000,
        )
    }

    const PAIR: Address = Address::repeat_byte(0xaa);
    const USER: Address = Address::repeat_byte(0x71);

    fn usdc() -> Address {
        address(fixtures::USDC)
    }

    fn weth() -> Address {
        address(fixtures::WETH)
    }

    fn pay(token: Address, amount: u128, index: u16) -> DatabaseLog {
        transfer(token, USER, PAIR, U256::from(amount), 100, index, 0)
    }

    fn receive(token: Address, amount: u128, index: u16) -> DatabaseLog {
        transfer(token, PAIR, USER, U256::from(amount), 100, index, 0)
    }

    #[test]
    fn real_v2_swap_is_proven_and_carries_its_reserves() {
        let rows = decode(
            1,
            &[
                fixtures::V2_SWAP_WETH_IN.log(),
                fixtures::V2_SWAP_USDC_OUT.log(),
                fixtures::V2_SYNC.log(),
                fixtures::V2_SWAP.log(),
            ],
        );
        let swap = &rows.swaps[0];

        assert_eq!(swap.verified_in, weth());
        assert_eq!(swap.verified_out, usdc());
        assert_eq!(swap.reserve0, fixtures::unsigned("10391705448638"));
        assert_eq!(
            swap.reserve1,
            fixtures::unsigned("3946924532103308992521")
        );
    }

    #[test]
    fn real_v3_and_curve_swaps_are_proven() {
        let rows = decode(
            1,
            &[
                fixtures::V3_SWAP_USDC_OUT.log(),
                fixtures::V3_SWAP_WETH_IN.log(),
                fixtures::V3_SWAP.log(),
                fixtures::CURVE_3POOL_USDT_IN.log(),
                fixtures::CURVE_3POOL_USDC_OUT.log(),
                fixtures::CURVE_3POOL_EXCHANGE.log(),
            ],
        );

        assert_eq!(rows.swaps[0].protocol, Protocol::UniswapV3);
        assert_eq!(
            (rows.swaps[0].verified_in, rows.swaps[0].verified_out),
            (weth(), usdc())
        );
        // Curve: the transfers reveal the coins without any RPC.
        assert_eq!(rows.swaps[1].protocol, Protocol::Curve);
        assert_eq!(
            (rows.swaps[1].verified_in, rows.swaps[1].verified_out),
            (address(fixtures::USDT), usdc())
        );
    }

    #[test]
    fn real_balancer_swap_settles_after_its_event() {
        let rows = decode(
            1,
            &[
                fixtures::BALANCER_SWAP.log(),
                fixtures::BALANCER_SWAP_TOKEN_IN.log(),
                fixtures::BALANCER_SWAP_WETH_OUT.log(),
            ],
        );
        let swap = &rows.swaps[0];

        assert_eq!(swap.verified_in, swap.token_in);
        assert_eq!(swap.verified_out, weth());
    }

    /// The real two swap V4 transaction: the PoolManager settles ONCE per
    /// currency. Only the USDT input of the second swap has a transfer of
    /// its own; the USDC input is netted with an earlier take (off by
    /// 29,558) and both WETH outputs leave as one transfer of their sum.
    #[test]
    fn real_v4_transaction_proves_only_what_has_its_own_transfer() {
        let rows = decode(
            1,
            &[
                fixtures::V4_TX_USDC_TAKEN.log(),
                fixtures::V4_TX_WETH_TAKEN.log(),
                fixtures::V4_SWAP_USDC_IN.log(),
                fixtures::V4_SWAP_USDT_IN.log(),
                fixtures::V4_TX_USDC_SETTLED.log(),
                fixtures::V4_TX_USDT_SETTLED.log(),
            ],
        );

        let first = &rows.swaps[0];
        assert_eq!(
            (first.verified_in, first.verified_out),
            (Address::ZERO, Address::ZERO)
        );

        let second = &rows.swaps[1];
        assert_eq!(second.verified_in, address(fixtures::USDT));
        assert_eq!(second.verified_out, Address::ZERO);
    }

    #[test]
    fn a_swap_without_transfers_proves_nothing() {
        // Path A of the review: a Balancer shaped swap naming USDC / WETH
        // with absurd amounts, from a contract that moved nothing.
        let forger = Address::repeat_byte(0xbd);
        let forged = build(
            forger,
            &[
                events::BALANCER_SWAP.topic0,
                B256::repeat_byte(0x01),
                usdc().into_word(),
                weth().into_word(),
            ],
            [word(10u128.pow(30)), word(1)].concat(),
            100,
            5,
            0,
        );
        // Not even with a "Transfer" of that amount emitted by a token
        // the forger controls: the event names USDC, USDC did not speak.
        let fake_transfer = transfer(
            Address::repeat_byte(0xfa),
            USER,
            forger,
            U256::from(10u128.pow(30)),
            100,
            6,
            0,
        );

        let rows =
            decode(1, &same_transaction(vec![forged, fake_transfer], 1));
        assert_eq!(rows.swaps.len(), 1);
        assert_eq!(rows.swaps[0].verified_in, Address::ZERO);
        assert_eq!(rows.swaps[0].verified_out, Address::ZERO);
    }

    #[test]
    fn one_transfer_proves_one_leg() {
        // One real payment, a thousand claimed swaps over it.
        let mut logs =
            vec![pay(usdc(), 5_000_000, 0), receive(weth(), 2_000, 1)];
        logs.extend(
            (0..1_000u16).map(|n| v2_swap(5_000_000, 2_000, 10 + n)),
        );

        let rows = decode(1, &same_transaction(logs, 1));
        let proven = rows
            .swaps
            .iter()
            .filter(|swap| !swap.verified_in.is_zero())
            .count();

        assert_eq!(rows.swaps.len(), 1_000);
        assert_eq!(proven, 1);
        assert_eq!(rows.swaps[0].verified_in, usdc());
        assert_eq!(rows.swaps[0].verified_out, weth());
    }

    #[test]
    fn transfers_elsewhere_do_not_count() {
        let swap = v2_swap(5_000_000, 2_000, 10);

        // Another transaction.
        let mut logs =
            same_transaction(vec![pay(usdc(), 5_000_000, 0)], 2);
        logs.extend(same_transaction(vec![swap.clone()], 1));
        assert_eq!(decode(1, &logs).swaps[0].verified_in, Address::ZERO);

        // To somebody else.
        let elsewhere = transfer(
            usdc(),
            USER,
            USER,
            U256::from(5_000_000u64),
            100,
            0,
            0,
        );
        let logs = same_transaction(vec![elsewhere, swap.clone()], 1);
        assert_eq!(decode(1, &logs).swaps[0].verified_in, Address::ZERO);

        // AFTER the swap event: a contract pool has the tokens before it
        // emits.
        let logs = same_transaction(
            vec![swap.clone(), pay(usdc(), 5_000_000, 11)],
            1,
        );
        assert_eq!(decode(1, &logs).swaps[0].verified_in, Address::ZERO);

        // "Transfer" emitted by the pool itself (LP token movements).
        let own = transfer(
            PAIR,
            USER,
            PAIR,
            U256::from(5_000_000u64),
            100,
            0,
            0,
        );
        let logs = same_transaction(vec![own, swap], 1);
        assert_eq!(decode(1, &logs).swaps[0].verified_in, Address::ZERO);
    }

    #[test]
    fn ambiguity_is_never_resolved_by_guessing() {
        // Two tokens report the same amount to the pool.
        let logs = same_transaction(
            vec![
                pay(usdc(), 5_000_000, 0),
                pay(Address::repeat_byte(0xfa), 5_000_000, 1),
                receive(weth(), 2_000, 2),
                v2_swap(5_000_000, 2_000, 10),
            ],
            1,
        );
        let swap = &decode(1, &logs).swaps[0];

        assert_eq!(swap.verified_in, Address::ZERO);
        assert_eq!(swap.verified_out, weth());
    }

    #[test]
    fn fee_on_transfer_legs_stay_unverified_the_other_leg_does_not() {
        // The pair accounts 4,950,000 received; the token's Transfer says
        // 5,000,000 (1% burnt on the way). Exact match only.
        let logs = same_transaction(
            vec![
                pay(Address::repeat_byte(0xf0), 5_000_000, 0),
                receive(weth(), 2_000, 1),
                v2_swap(4_950_000, 2_000, 10),
            ],
            1,
        );
        let swap = &decode(1, &logs).swaps[0];

        assert_eq!(swap.verified_in, Address::ZERO);
        assert_eq!(swap.verified_out, weth());
    }

    #[test]
    fn same_sign_swaps_have_no_direction_and_no_proof() {
        // Both sides IN (a flash swap repaid in both tokens).
        let both_in = build(
            PAIR,
            &[events::V2_SWAP.topic0, USER.into_word(), USER.into_word()],
            [word(5), word(7), word(0), word(0)].concat(),
            100,
            10,
            0,
        );
        let logs = same_transaction(vec![pay(usdc(), 5, 0), both_in], 1);
        let swap = &decode(1, &logs).swaps[0];

        assert_eq!(swap.amount0, I256::try_from(5).unwrap());
        assert_eq!(swap.amount1, I256::try_from(7).unwrap());
        assert_eq!(
            (swap.amount_in, swap.amount_out),
            (U256::ZERO, U256::ZERO)
        );
        assert_eq!(swap.verified_in, Address::ZERO);
    }

    #[test]
    fn input_order_does_not_change_the_result() {
        let mut logs = same_transaction(
            vec![
                pay(usdc(), 5_000_000, 0),
                receive(weth(), 2_000, 1),
                v2_swap(5_000_000, 2_000, 10),
                v2_swap(5_000_000, 2_000, 20),
            ],
            1,
        );
        logs.reverse();

        let rows = decode(1, &logs);
        let at = |index: u64| {
            rows.swaps.iter().find(|swap| swap.ordinal == index).unwrap()
        };

        assert_eq!(at(10).verified_in, usdc());
        assert_eq!(at(20).verified_in, Address::ZERO);
    }
}
