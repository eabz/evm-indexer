//! What events can not tell: the token an exchange / AMM pool is paid in
//! (and the registry it trades positions of). Asked with `eth_call`, in
//! the background, never on the commit path.
//!
//! | family | collateral | registry |
//! |---|---|---|
//! | `ctf_exchange`, `ctf_exchange_v2` | `getCollateral()` | `getCtf()` |
//! | `fpmm` | `collateralToken()` | `conditionalTokens()` |
//!
//! Both getters were checked against the live contracts (Polygon CTF
//! Exchange V1 / V2, Gnosis and Base FixedProductMarketMakers). Trades are
//! decoded and priced without any of this: the collateral token only turns
//! raw amounts of the leaderboard into decimal ones.
//!
//! **Never a bare `eth_call`.** Every getter goes through
//! [`call_confirmed`], which only returns an answer two independent
//! endpoints agree on - the same rule `src/dex/resolve.rs` and the token
//! worker follow. The endpoints are discovered and public, so one lying or
//! compromised node would otherwise set `prediction_venues.collateral_token`
//! to any ERC-20 it likes; the row is written with `source = 'rpc'` and
//! `MISSING_VENUES_SQL` never asks about an exchange that already has one,
//! so the wrong decimals would scale that exchange's whole leaderboard by
//! `10^(wrong - right)` for ever. Without agreement the answer is
//! [`Resolution::Retry`] and NOTHING is cached.

use alloy::primitives::{keccak256, Address, Bytes};

use crate::tokens::multicall::{call_confirmed, CallError, EthCaller};

use super::{
    models::{
        PredictionVenue, Protocol, RowSource, VERSION_RPC,
        VERSION_UNRESOLVED,
    },
    VenueCandidate,
};

/// A getter without arguments returning one address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Getter {
    pub signature: &'static str,
}

impl Getter {
    pub fn calldata(&self) -> Bytes {
        Bytes::copy_from_slice(&keccak256(self.signature.as_bytes())[..4])
    }
}

pub const GET_COLLATERAL: Getter = Getter { signature: "getCollateral()" };
pub const GET_CTF: Getter = Getter { signature: "getCtf()" };
pub const COLLATERAL_TOKEN: Getter =
    Getter { signature: "collateralToken()" };
pub const CONDITIONAL_TOKENS: Getter =
    Getter { signature: "conditionalTokens()" };

/// (collateral getter, registry getter) to try for a family, in order.
fn getters(protocol: Protocol) -> [(Getter, Getter); 2] {
    let exchange = (GET_COLLATERAL, GET_CTF);
    let pool = (COLLATERAL_TOKEN, CONDITIONAL_TOKENS);

    if protocol == Protocol::Fpmm {
        [pool, exchange]
    } else {
        [exchange, pool]
    }
}

/// Outcome of asking one contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Resolved(PredictionVenue),
    /// The contract answered and is not a venue of a known family: a
    /// `source = 'unresolved'` row keeps it from being asked again.
    NotAVenue(PredictionVenue),
    /// Nothing can be concluded (node unreachable, empty answer that
    /// could not be confirmed): ask again later, cache nothing.
    Retry,
}

enum Answer {
    Address(Address),
    Nothing,
    Retry,
}

async fn ask(
    caller: &dyn EthCaller,
    to: Address,
    getter: Getter,
) -> Answer {
    // Two independent endpoints must return the same bytes (or both
    // refuse) before anything here is believed - see the module docs.
    // `call_confirmed` maps "no agreement" onto `Transient`, which is
    // `Retry`: nothing is stored, the exchange is asked again later.
    match call_confirmed(caller, to, getter.calldata()).await {
        Ok(bytes)
            if bytes.len() == 32
                && bytes[..12].iter().all(|b| *b == 0) =>
        {
            Answer::Address(Address::from_slice(&bytes[12..]))
        }
        // A confirmed empty answer: the contract has no such getter.
        Ok(bytes) if bytes.is_empty() => Answer::Nothing,
        Ok(_) | Err(CallError::Execution(_)) => Answer::Nothing,
        Err(CallError::Transient(_)) => Answer::Retry,
    }
}

fn row(
    chain: u64,
    candidate: &VenueCandidate,
    collateral_token: Address,
    registry: Address,
    source: RowSource,
) -> PredictionVenue {
    PredictionVenue {
        chain,
        exchange: candidate.exchange,
        protocol: candidate.protocol,
        collateral_token,
        registry,
        source,
        _version: if source == RowSource::Rpc {
            VERSION_RPC
        } else {
            VERSION_UNRESOLVED
        },
    }
}

/// The row that says "asked, not a venue".
pub fn unresolved_venue(
    chain: u64,
    candidate: &VenueCandidate,
) -> PredictionVenue {
    row(
        chain,
        candidate,
        Address::ZERO,
        Address::ZERO,
        RowSource::Unresolved,
    )
}

pub async fn resolve_venue(
    caller: &dyn EthCaller,
    chain: u64,
    candidate: &VenueCandidate,
) -> Resolution {
    for (collateral_getter, registry_getter) in getters(candidate.protocol)
    {
        let collateral =
            match ask(caller, candidate.exchange, collateral_getter).await
            {
                Answer::Address(address) if !address.is_zero() => address,
                Answer::Retry => return Resolution::Retry,
                _ => continue,
            };

        // The registry is a bonus: a venue without the getter still has a
        // collateral.
        let registry =
            match ask(caller, candidate.exchange, registry_getter).await {
                Answer::Address(address) => address,
                Answer::Retry => return Resolution::Retry,
                Answer::Nothing => Address::ZERO,
            };

        return Resolution::Resolved(row(
            chain,
            candidate,
            collateral,
            registry,
            RowSource::Rpc,
        ));
    }

    Resolution::NotAVenue(unresolved_venue(chain, candidate))
}

#[cfg(test)]
pub mod test_support {
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Mutex,
        },
    };

    use futures::future::BoxFuture;

    use super::*;

    /// A node that knows a few contracts.
    #[derive(Default)]
    pub struct FakeNode {
        answers: Mutex<HashMap<(Address, Bytes), Bytes>>,
        pub down: AtomicBool,
        calls: AtomicUsize,
    }

    impl FakeNode {
        pub fn set(&self, to: Address, getter: Getter, answer: Address) {
            self.answers.lock().unwrap().insert(
                (to, getter.calldata()),
                Bytes::copy_from_slice(answer.into_word().as_slice()),
            );
        }

        pub fn exchange(
            &self,
            to: Address,
            collateral: Address,
            ctf: Address,
        ) {
            self.set(to, GET_COLLATERAL, collateral);
            self.set(to, GET_CTF, ctf);
        }

        pub fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl EthCaller for FakeNode {
        fn call(
            &self,
            to: Address,
            data: Bytes,
        ) -> BoxFuture<'_, Result<Bytes, CallError>> {
            self.calls.fetch_add(1, Ordering::Relaxed);

            let answer = if self.down.load(Ordering::Relaxed) {
                Err(CallError::Transient("down".into()))
            } else {
                match self.answers.lock().unwrap().get(&(to, data)) {
                    Some(bytes) => Ok(bytes.clone()),
                    None => Err(CallError::Execution("revert".into())),
                }
            };

            Box::pin(async move { answer })
        }

        fn chain_id(&self) -> BoxFuture<'_, Result<u64, CallError>> {
            Box::pin(async { Ok(137) })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::{test_support::FakeNode, *};

    fn candidate(byte: u8, protocol: Protocol) -> VenueCandidate {
        VenueCandidate { exchange: Address::repeat_byte(byte), protocol }
    }

    /// Selectors of the deployed contracts (4byte directory / verified
    /// sources), so a typo in a signature can not go unnoticed.
    #[test]
    fn selectors() {
        assert_eq!(hex::encode(GET_COLLATERAL.calldata()), "5c1548fb");
        assert_eq!(hex::encode(GET_CTF.calldata()), "3b521d78");
        assert_eq!(hex::encode(COLLATERAL_TOKEN.calldata()), "b2016bd4");
        assert_eq!(hex::encode(CONDITIONAL_TOKENS.calldata()), "5bd9e299");
    }

    #[tokio::test]
    async fn exchanges_and_pools_are_asked_their_own_getters() {
        let node = FakeNode::default();
        let usdc = Address::repeat_byte(0xc0);
        let ctf = Address::repeat_byte(0xcf);

        node.exchange(Address::repeat_byte(1), usdc, ctf);
        node.set(Address::repeat_byte(2), COLLATERAL_TOKEN, usdc);

        let exchange = candidate(1, Protocol::CtfExchangeV2);
        let Resolution::Resolved(venue) =
            resolve_venue(&node, 137, &exchange).await
        else {
            panic!("not resolved");
        };
        assert_eq!(venue.collateral_token, usdc);
        assert_eq!(venue.registry, ctf);
        assert_eq!(venue.source, RowSource::Rpc);
        assert_eq!(venue._version, VERSION_RPC);

        // A pool without conditionalTokens(): collateral is enough.
        let pool = candidate(2, Protocol::Fpmm);
        let Resolution::Resolved(venue) =
            resolve_venue(&node, 137, &pool).await
        else {
            panic!("not resolved");
        };
        assert_eq!(venue.collateral_token, usdc);
        assert!(venue.registry.is_zero());
    }

    #[tokio::test]
    async fn reverts_are_negative_outages_are_not() {
        let node = FakeNode::default();
        let stranger = candidate(9, Protocol::CtfExchange);

        let Resolution::NotAVenue(venue) =
            resolve_venue(&node, 137, &stranger).await
        else {
            panic!("expected a negative answer");
        };
        assert_eq!(venue.source, RowSource::Unresolved);
        assert_eq!(venue._version, VERSION_UNRESOLVED);

        node.down.store(true, Ordering::Relaxed);
        assert_eq!(
            resolve_venue(&node, 137, &stranger).await,
            Resolution::Retry
        );
    }
}
