//! Conversions from HyperSync wire types into the alloy primitives and the
//! integer widths used by the ClickHouse schema.
//!
//! The columns are as wide as the protocol (`UInt64` block numbers, gas
//! and nonces, `UInt256` amounts and prices), so almost nothing narrows any
//! more. What is left:
//!
//! - HyperSync `Quantity` is an arbitrary length big endian integer. A
//!   value that does not fit the `UInt64` / `UInt256` column it belongs to
//!   is not valid chain data, but it must not panic or wrap either.
//! - Positions and counts inside a block (`transaction_index`, `log_index`,
//!   trace positions, `subtraces`...) are `UInt32` columns fed from `u64`.
//! - `timestamp` is a `DateTime` (`u32` seconds).
//!
//! Those narrowings SATURATE, and the first time it happens for each
//! target width a warning is logged (once per process, not per row): a
//! saturated value is still wrong data.

use alloy::primitives::{Address, Bytes, B256, B64, U256};
use hypersync_client::format::{
    Address as HsAddress, Data, Hash, Nonce, Quantity,
};
use log::warn;
use std::sync::atomic::{AtomicBool, Ordering};

/// Target widths a value can saturate at, one warning each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    U32,
    U64,
    U256,
}

static SATURATED_U32: AtomicBool = AtomicBool::new(false);
static SATURATED_U64: AtomicBool = AtomicBool::new(false);
static SATURATED_U256: AtomicBool = AtomicBool::new(false);

impl Width {
    fn flag(self) -> &'static AtomicBool {
        match self {
            Width::U32 => &SATURATED_U32,
            Width::U64 => &SATURATED_U64,
            Width::U256 => &SATURATED_U256,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Width::U32 => {
                "UInt32 / DateTime column (transaction_index, log_index, \
                 trace positions, counts, timestamp)"
            }
            Width::U64 => {
                "UInt64 column (gas, gas_limit, gas_used, size, nonce, \
                 withdrawal indexes)"
            }
            Width::U256 => "UInt256 column",
        }
    }

    /// True once a value saturated at this width in this process.
    pub fn has_saturated(self) -> bool {
        self.flag().load(Ordering::Relaxed)
    }
}

/// Logs the first saturation per width. Cheap on the hot path: one atomic
/// swap, and only when a value actually did not fit.
#[cold]
fn report_saturation(width: Width, value: &dyn std::fmt::Display) {
    if !width.flag().swap(true, Ordering::Relaxed) {
        warn!(
            "Value {value} does not fit a {} and was stored as the column \
             maximum. The stored value is WRONG. This warning is logged \
             once per process, more rows may be affected.",
            width.describe()
        );
    }
}

/// Position / count inside a block -> `UInt32` column.
pub fn sat_u32(value: u64) -> u32 {
    u32::try_from(value).unwrap_or_else(|_| {
        report_saturation(Width::U32, &value);
        u32::MAX
    })
}

/// Big-endian bytes -> U256, saturating when longer than 32 bytes.
pub fn be_bytes_to_u256(bytes: &[u8]) -> U256 {
    U256::try_from_be_slice(bytes).unwrap_or_else(|| {
        report_saturation(
            Width::U256,
            &format_args!("of {} bytes", bytes.len()),
        );
        U256::MAX
    })
}

pub fn quantity_to_u256(quantity: &Quantity) -> U256 {
    be_bytes_to_u256(quantity.as_ref())
}

pub fn quantity_to_u64(quantity: &Quantity) -> u64 {
    let value = quantity_to_u256(quantity);

    u64::try_from(value).unwrap_or_else(|_| {
        report_saturation(Width::U64, &value);
        u64::MAX
    })
}

/// Only for `timestamp` (`DateTime` is `u32` seconds).
pub fn quantity_to_u32(quantity: &Quantity) -> u32 {
    let value = quantity_to_u256(quantity);

    u32::try_from(value).unwrap_or_else(|_| {
        report_saturation(Width::U32, &value);
        u32::MAX
    })
}

pub fn hash_to_b256(hash: &Hash) -> B256 {
    B256::new(***hash)
}

pub fn address_to_alloy(address: &HsAddress) -> Address {
    Address::new(***address)
}

pub fn nonce_to_b64(nonce: &Nonce) -> B64 {
    B64::new(***nonce)
}

pub fn data_to_bytes(data: &Data) -> Bytes {
    Bytes::copy_from_slice(data.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrowing_saturates_instead_of_truncating() {
        assert_eq!(sat_u32(7), 7);
        assert_eq!(sat_u32(u32::MAX as u64), u32::MAX);
        // `(u32::MAX as u64 + 1) as u32` would be 0.
        assert_eq!(sat_u32(u32::MAX as u64 + 1), u32::MAX);
        assert_eq!(sat_u32(u64::MAX), u32::MAX);
    }

    #[test]
    fn quantity_conversions() {
        let small = Quantity::from(30_000_000u64);
        assert_eq!(quantity_to_u64(&small), 30_000_000);
        assert_eq!(quantity_to_u32(&small), 30_000_000);
        assert_eq!(quantity_to_u256(&small), U256::from(30_000_000u64));

        // Gas limit above u32::MAX (seen on some L2s / test chains) is
        // kept as is: the columns are UInt64.
        let big = Quantity::from(u32::MAX as u64 + 10);
        assert_eq!(quantity_to_u64(&big), u32::MAX as u64 + 10);
        assert_eq!(quantity_to_u64(&Quantity::from(u64::MAX)), u64::MAX);

        // 9 significant bytes do not fit u64.
        let huge = Quantity::from(vec![1u8, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(quantity_to_u64(&huge), u64::MAX);
        assert_eq!(quantity_to_u256(&huge), U256::from(1u8) << 64);

        assert_eq!(quantity_to_u64(&Quantity::default()), 0);
    }

    #[test]
    fn saturation_is_flagged_per_width_and_only_when_it_happens() {
        // Values that fit never report anything. (The flags are process
        // wide and other tests saturate on purpose, so only the "set"
        // direction can be asserted after a saturating call.)
        assert_eq!(sat_u32(1), 1);
        assert_eq!(quantity_to_u64(&Quantity::from(u64::MAX)), u64::MAX);

        assert_eq!(sat_u32(u64::MAX), u32::MAX);
        assert!(Width::U32.has_saturated());

        assert_eq!(
            quantity_to_u64(&Quantity::from(vec![1u8; 9])),
            u64::MAX
        );
        assert!(Width::U64.has_saturated());

        assert_eq!(be_bytes_to_u256(&[1u8; 33]), U256::MAX);
        assert!(Width::U256.has_saturated());

        // Reporting again is a no-op (no panic, flag stays set).
        report_saturation(Width::U32, &7);
        assert!(Width::U32.has_saturated());
    }

    #[test]
    fn oversized_big_endian_input_saturates() {
        let bytes = [0xffu8; 40];
        assert_eq!(be_bytes_to_u256(&bytes), U256::MAX);
        assert_eq!(be_bytes_to_u256(&[]), U256::ZERO);
    }

    #[test]
    fn fixed_size_conversions_keep_bytes() {
        let hash = Hash::from([0xabu8; 32]);
        assert_eq!(hash_to_b256(&hash), B256::repeat_byte(0xab));

        let address = HsAddress::from([0x11u8; 20]);
        assert_eq!(address_to_alloy(&address), Address::repeat_byte(0x11));

        let nonce = Nonce::from([0x42u8; 8]);
        assert_eq!(nonce_to_b64(&nonce), B64::repeat_byte(0x42));

        let data = Data::from(vec![1u8, 2, 3]);
        assert_eq!(data_to_bytes(&data), Bytes::from(vec![1u8, 2, 3]));
    }
}
