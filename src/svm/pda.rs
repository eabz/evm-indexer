//! Telling a program owned account apart from a user's wallet, with no RPC
//! call and no registry.
//!
//! # Why the decoder needs this
//!
//! When a venue moves ONE mint as a token and the other leg is native SOL
//! (a bonding curve just decrements its own lamports, with no instruction at
//! all), the two sides of the trade look completely symmetric: the user
//! sends a token and gains lamports, the pool gains the token and loses
//! exactly the same lamports. Shape alone cannot say which is which, and
//! `is_signer` does not help either - a live pump.fun sell in
//! `svm::fixtures` has a BOT paying the fee, so the user is not a signer
//! any more than the curve is.
//!
//! The asymmetry that always holds is this: **a pool authority is a Program
//! Derived Address, and a PDA is by construction a 32-byte value that is NOT
//! a valid ed25519 public key** - `find_program_address` searches bump seeds
//! precisely until it lands off the curve, so that no private key can ever
//! exist for it. A user's wallet, being an actual ed25519 public key, is on
//! the curve.
//!
//! So: on the curve = someone can sign for it = a wallet. Off the curve =
//! nobody can = a program derived account.
//!
//! # What this module does
//!
//! [`is_on_curve`] performs the ed25519 point decompression of RFC 8032
//! section 5.1.3 and reports whether the compressed y-coordinate yields a
//! valid curve point. It is pure arithmetic modulo `2^255 - 19` on
//! [`U256`], with no dependencies beyond what the crate already has.
//!
//! It is a CLASSIFIER, not a proof of intent: a wallet can hold a pool and a
//! PDA can belong to a user. It is used only to break the symmetry above,
//! and only when exactly one of the two candidates is off the curve;
//! otherwise the decoder reports the swap as ambiguous rather than guessing.

use std::cell::RefCell;

use alloy::primitives::U256;

/// The curve constants, parsed once.
///
/// They used to be rebuilt on every call, and two of them by parsing a
/// DECIMAL STRING - which a profile of the decoder showed costing more than
/// some whole transactions. See `svm::profile`.
struct Constants {
    p: U256,
    d: U256,
    sqrt_m1: U256,
    /// `(p - 5) / 8`, the square-root exponent.
    exponent: U256,
}

fn constants() -> &'static Constants {
    static CONSTANTS: std::sync::OnceLock<Constants> =
        std::sync::OnceLock::new();
    CONSTANTS.get_or_init(|| {
        let p = (U256::from(1u8) << 255) - U256::from(19u8);
        Constants {
            p,
            d: U256::from_str_radix(
                "37095705934669439343138083508754565189542113879843219016388785533085940283555",
                10,
            )
            .expect("curve constant d"),
            sqrt_m1: U256::from_str_radix(
                "19681161376707505956807079304988542015446066515923890162744021073123829784752",
                10,
            )
            .expect("curve constant sqrt(-1)"),
            exponent: (p - U256::from(5u8)) >> 3,
        }
    })
}

/// The field prime, `2^255 - 19`.
fn p() -> U256 {
    constants().p
}

/// The Edwards curve constant `d = -121665 / 121666 (mod p)`.
fn d() -> U256 {
    constants().d
}

/// `sqrt(-1) (mod p)`, i.e. `2^((p-1)/4)`.
fn sqrt_m1() -> U256 {
    constants().sqrt_m1
}

// --- the memo ------------------------------------------------------------

/// Slots in the per-thread memo. A power of two so the index is a mask.
///
/// 8,192 entries is 264 KB per decoding thread, which is nothing next to what
/// it saves: the answer is a pure function of the 32 bytes, and in a real
/// stream the SAME pool authorities come back in transaction after
/// transaction - a busy PumpSwap pool appears hundreds of times in one slot.
const MEMO_SLOTS: usize = 1 << 13;

/// Direct mapped, so an entry is simply overwritten on a collision. There is
/// no correctness question either way: a miss just recomputes.
#[derive(Clone, Copy)]
struct Memo {
    key: [u8; 32],
    /// 0 = empty, 1 = off the curve, 2 = on the curve.
    state: u8,
}

thread_local! {
    static MEMO: RefCell<Box<[Memo]>> = RefCell::new(
        vec![Memo { key: [0u8; 32], state: 0 }; MEMO_SLOTS]
            .into_boxed_slice(),
    );
}

/// Index of `key` in the memo. Pubkeys are uniformly distributed, so the low
/// bytes are as good a hash as anything.
fn memo_slot(key: &[u8; 32]) -> usize {
    let mut head = [0u8; 8];
    head.copy_from_slice(&key[..8]);
    (u64::from_le_bytes(head) as usize) & (MEMO_SLOTS - 1)
}

/// Is `key` a valid ed25519 point, i.e. an address somebody could hold the
/// private key for?
///
/// `false` means the value is off the curve, which for a Solana account
/// means it is a program derived address: a pool, a vault authority, a
/// bonding curve, a config account.
/// It is also MEMOISED, per thread. The curve test is a modular
/// exponentiation with a 252-bit exponent - measured at ~56 microseconds a
/// call, which made it by far the most expensive thing the decoder does -
/// and the answer depends on nothing but the 32 bytes, so the same pool
/// authority never needs computing twice.
pub fn is_on_curve(key: &[u8; 32]) -> bool {
    let slot = memo_slot(key);
    let cached = MEMO.with(|memo| {
        let memo = memo.borrow();
        let entry = &memo[slot];
        if entry.state != 0 && entry.key == *key {
            Some(entry.state == 2)
        } else {
            None
        }
    });
    if let Some(answer) = cached {
        return answer;
    }

    let answer = compute_on_curve(key);
    MEMO.with(|memo| {
        memo.borrow_mut()[slot] =
            Memo { key: *key, state: if answer { 2 } else { 1 } };
    });
    answer
}

/// The curve test itself, with no memo in front of it.
fn compute_on_curve(key: &[u8; 32]) -> bool {
    let p = p();

    // The compressed encoding is little endian, with the top bit carrying
    // the sign of x.
    let mut bytes = *key;
    let sign = bytes[31] >> 7;
    bytes[31] &= 0x7f;
    let y = U256::from_le_bytes(bytes);

    // A non-canonical y is not a point.
    if y >= p {
        return false;
    }

    let one = U256::from(1u8);
    let y2 = y.mul_mod(y, p);
    // u = y^2 - 1, v = d * y^2 + 1
    let u = sub_mod(y2, one, p);
    let v = d().mul_mod(y2, p).add_mod(one, p);

    // x = u * v^3 * (u * v^7)^((p - 5) / 8)
    let v2 = v.mul_mod(v, p);
    let v3 = v2.mul_mod(v, p);
    let v7 = v3.mul_mod(v3, p).mul_mod(v, p);
    let exponent = constants().exponent;
    let mut x =
        u.mul_mod(v3, p).mul_mod(u.mul_mod(v7, p).pow_mod(exponent, p), p);

    // Check v * x^2 == u, or == -u (then multiply by sqrt(-1)).
    let vxx = v.mul_mod(x.mul_mod(x, p), p);
    if vxx != u {
        if vxx == sub_mod(U256::ZERO, u, p) {
            x = x.mul_mod(sqrt_m1(), p);
        } else {
            return false;
        }
    }

    // x = 0 has only one root, so a sign bit of 1 is not a valid encoding.
    if x == U256::ZERO && sign == 1 {
        return false;
    }

    true
}

fn sub_mod(a: U256, b: U256, modulus: U256) -> U256 {
    if a >= b {
        a - b
    } else {
        modulus - (b - a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::svm::programs::pubkey;

    /// The two accounts the pump.fun fixture cannot tell apart by shape.
    ///
    /// `HaJwBJ...` is the bonding curve, a PDA of the pump.fun program.
    ///
    /// The `BwWK17...` side of that same trade turns out to be program
    /// derived TOO - a trading bot's vault, not a plain wallet - which is
    /// why the curve test alone does NOT separate the two sides there and
    /// `svm::decode` has to let the venue's own event adjudicate. The case
    /// is pinned here so the limitation stays visible.
    #[test]
    fn the_pumpfun_curve_is_program_derived_and_so_is_that_bots_vault() {
        let bonding_curve =
            pubkey("HaJwBJYmFyBRxuVQe4Yr53wkkDQbWsDH8uC7S362y1Ew");
        let bot_vault =
            pubkey("BwWK17cbHxwWBKZkUYvzxLcNQ1YVyaFezduWbtm2de6s");

        assert!(
            !is_on_curve(&bonding_curve),
            "a bonding curve PDA must be off the curve"
        );
        assert!(
            !is_on_curve(&bot_vault),
            "this 'user' is itself a PDA, so the curve test cannot break \
             the tie on its own"
        );
    }

    /// Pool vaults and pool state accounts are PDAs too.
    #[test]
    fn pumpswap_pool_and_vaults_are_program_derived() {
        for pda in [
            // The PumpSwap pool of the recorded buy.
            "EJTBQyiF4GMwXSjucW1qnBMVa7iJD21yyvCvMDFrkSrR",
            // Its base and quote vaults.
            "CLm11GrqZHTVrPKrrWTVsUHtyNYRmBK568YyaD16G3dx",
            "7YHYVZXbHH9AFn9YLgc2j1udnpUCPZxyFL5JLybJ9KY3",
        ] {
            assert!(
                !is_on_curve(&pubkey(pda)),
                "{pda} should be program derived"
            );
        }
    }

    /// A wallet that signs transactions is on the curve.
    #[test]
    fn fee_payers_are_on_the_curve() {
        for wallet in [
            // Fee payer of the recorded PumpSwap buy.
            "5e2SDXr1HCNyu47txjXnVd8wraceGhgVTzSh24jwrQHy",
            // Fee payer of the recorded pump.fun sell.
            "Gygj9QQby4j2jryqyqBHvLP7ctv2SaANgh4sCb69BUpA",
        ] {
            assert!(is_on_curve(&pubkey(wallet)), "{wallet} is a wallet");
        }
    }

    /// The all-zero key is the canonical y = 0 point and IS on the curve;
    /// the test exists so a future refactor cannot make the function
    /// trivially return false.
    #[test]
    fn the_function_actually_discriminates() {
        let mut on = 0;
        let mut off = 0;
        // Deterministic pseudo random 32 byte values: roughly half of all
        // 32-byte strings are valid points.
        let mut state = 0x243f_6a88_85a3_08d3u64;
        for _ in 0..200 {
            let mut key = [0u8; 32];
            for byte in key.iter_mut() {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                *byte = (state >> 33) as u8;
            }
            if is_on_curve(&key) {
                on += 1;
            } else {
                off += 1;
            }
        }
        assert!(on > 40, "suspiciously few points on the curve: {on}");
        assert!(off > 40, "suspiciously few off the curve: {off}");
    }
}
