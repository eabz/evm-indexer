//! `serde_with` adapters between alloy primitives and the binary ClickHouse
//! column types of the schema (docs/design.md, section 1). Nothing is stored as hex.
//!
//! | Rust               | ClickHouse        | RowBinary                     |
//! |--------------------|-------------------|-------------------------------|
//! | `B256`             | `FixedString(32)` | 32 raw bytes, no length       |
//! | `Address`          | `FixedString(20)` | 20 raw bytes, no length       |
//! | `B64`              | `FixedString(8)`  | 8 raw bytes, no length        |
//! | `Selector`         | `FixedString(4)`  | 4 raw bytes, no length        |
//! | `U256`             | `UInt256`         | 32 bytes little endian        |
//! | `I256`             | `Int256`          | 32 bytes LE, two's complement |
//! | `Bytes`            | `String`          | LEB128 length + raw bytes     |
//!
//! The chain-neutral analytics modules (docs/design.md §13) add two more:
//!
//! | Rust               | ClickHouse        | RowBinary                     |
//! |--------------------|-------------------|-------------------------------|
//! | `Address` (`SerId32`) | `FixedString(32)` | 12 zero bytes + 20 address bytes |
//! | `Vec<Address>` (`SerVecId32`) | `Array(FixedString(32))` | the same, per element |
//! | `Bytes` (`SerTxId`) | `String` (`tx_id`) | LEB128 length + raw bytes |
//!
//! How this maps onto the `clickhouse` crate (checked against 0.14.0,
//! `src/rowbinary/{ser,validation}.rs`):
//!
//! - `FixedString(N)` must be written WITHOUT a length prefix, which is what
//!   a serde tuple of `N` `u8` produces (`[u8; N]`). `serialize_bytes` would
//!   add a LEB128 length and corrupt the row. The crate's schema validation
//!   accepts exactly this shape (`SerdeType::Tuple(N)` for
//!   `FixedString(N)`).
//! - `UInt256` / `Int256` have no native support. On the wire they are 32
//!   little endian bytes, which is exactly the four little endian `u64`
//!   limbs of a ruint `U256` written in order: a serde tuple of four `u64`
//!   (4 writes instead of 32). The crate's schema validation has NO mapping
//!   for `(U)Int256` and panics on any serde shape, so rows with such
//!   columns are inserted with validation disabled (plain `RowBinary` with
//!   an explicit column list, see `Database::insert_once`).
//!
//! Every adapter also deserializes, so rows can be read back with
//! `fetch::<Row>()` on a client with validation disabled.

use alloy::primitives::{Address, Bytes, Selector, B256, B64, I256, U256};
use serde::{
    de::{self, SeqAccess, Visitor},
    Deserialize, Deserializer, Serialize, Serializer,
};
use serde_with::{DeserializeAs, SerializeAs};
use std::fmt;

/// Implements a `FixedString(N)` adapter for a `FixedBytes<N>` like type
/// whose `.0` is a `[u8; N]`.
macro_rules! fixed_string_adapter {
    ($(#[$doc:meta])* $adapter:ident, $target:ty, $len:literal) => {
        $(#[$doc])*
        pub struct $adapter(());

        impl SerializeAs<$target> for $adapter {
            #[inline]
            fn serialize_as<S>(
                value: &$target,
                serializer: S,
            ) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                let bytes: &[u8; $len] = &value.0;
                bytes.serialize(serializer)
            }
        }

        impl<'de> DeserializeAs<'de, $target> for $adapter {
            fn deserialize_as<D>(
                deserializer: D,
            ) -> Result<$target, D::Error>
            where
                D: Deserializer<'de>,
            {
                <[u8; $len]>::deserialize(deserializer).map(<$target>::from)
            }
        }
    };
}

fixed_string_adapter!(
    /// `B256` (hash, topic) <-> `FixedString(32)`.
    SerB256,
    B256,
    32
);

fixed_string_adapter!(
    /// `B64` (block nonce) <-> `FixedString(8)`.
    SerB64,
    B64,
    8
);

fixed_string_adapter!(
    /// 4 byte function selector <-> `FixedString(4)`.
    SerSelector,
    Selector,
    4
);

/// `Address` <-> `FixedString(20)`.
pub struct SerAddress(());

impl SerializeAs<Address> for SerAddress {
    #[inline]
    fn serialize_as<S>(
        value: &Address,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let bytes: &[u8; 20] = &value.0 .0;
        bytes.serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, Address> for SerAddress {
    fn deserialize_as<D>(deserializer: D) -> Result<Address, D::Error>
    where
        D: Deserializer<'de>,
    {
        <[u8; 20]>::deserialize(deserializer).map(Address::from)
    }
}

// STUB - replaced by dex-neutral at merge
// Signatures taken verbatim from dex-neutral over tirith (message
// 4234934c, 2026-09-19 05:11:53Z): it owns the implementation, this is a
// byte-for-byte compatible placeholder so the predictions module can build
// before its branch lands.
/// 32 byte form of an EVM address: 12 zero bytes + the 20 address bytes
/// (docs/design.md §13). The convention `dex_pools.pool_id` already uses.
#[inline]
pub fn id32(address: Address) -> B256 {
    address.into_word()
}

// STUB - replaced by dex-neutral at merge
/// The EVM address inside a 32 byte id, `None` when the 12 leading bytes
/// are not zero (a non-EVM id: it is NOT an address and is never truncated
/// into one).
#[inline]
pub fn address_of_id32(id: B256) -> Option<Address> {
    id[..12]
        .iter()
        .all(|byte| *byte == 0)
        .then(|| Address::from_slice(&id[12..]))
}

// STUB - replaced by dex-neutral at merge
/// `tx_id` of an EVM transaction: the raw 32 bytes of its hash. The column
/// is a `String` because a Solana signature is 64 bytes (docs/design.md
/// §13).
#[inline]
pub fn tx_id(hash: B256) -> Bytes {
    Bytes::copy_from_slice(hash.as_slice())
}

// STUB - replaced by dex-neutral at merge
/// The EVM transaction hash inside a `tx_id`, `None` unless it is exactly
/// 32 bytes (a Solana signature is not a `B256`).
#[inline]
pub fn tx_hash_of(id: &[u8]) -> Option<B256> {
    <[u8; 32]>::try_from(id).ok().map(B256::from)
}

// STUB - replaced by dex-neutral at merge
/// alloy `Address` <-> `FixedString(32)`, left padded with 12 zero bytes
/// (docs/design.md §13: one identity type for every chain family).
/// Deserializing ERRORS on a non-zero padding by design - rows written by a
/// non-EVM front end are read with raw SQL, never through this adapter.
pub struct SerId32(());

impl SerializeAs<Address> for SerId32 {
    #[inline]
    fn serialize_as<S>(
        value: &Address,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        id32(*value).0.serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, Address> for SerId32 {
    fn deserialize_as<D>(deserializer: D) -> Result<Address, D::Error>
    where
        D: Deserializer<'de>,
    {
        let id = B256::from(<[u8; 32]>::deserialize(deserializer)?);
        address_of_id32(id).ok_or_else(|| {
            de::Error::custom(format!(
                "id {id} is not a left padded EVM address"
            ))
        })
    }
}

// STUB - replaced by dex-neutral at merge
/// `Vec<Address>` <-> `Array(FixedString(32))`.
pub struct SerVecId32(());

impl SerializeAs<Vec<Address>> for SerVecId32 {
    fn serialize_as<S>(
        value: &Vec<Address>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let ids: Vec<[u8; 32]> =
            value.iter().map(|address| id32(*address).0).collect();
        ids.serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, Vec<Address>> for SerVecId32 {
    fn deserialize_as<D>(deserializer: D) -> Result<Vec<Address>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<[u8; 32]>::deserialize(deserializer)?
            .into_iter()
            .map(|bytes| {
                let id = B256::from(bytes);
                address_of_id32(id).ok_or_else(|| {
                    de::Error::custom(format!(
                        "id {id} is not a left padded EVM address"
                    ))
                })
            })
            .collect()
    }
}

// STUB - replaced by dex-neutral at merge
/// `tx_id` <-> `String` holding the RAW transaction id (32 bytes on EVM,
/// 64 on Solana). Never hex, never a sorting key column.
pub struct SerTxId(());

impl SerializeAs<Bytes> for SerTxId {
    #[inline]
    fn serialize_as<S>(
        value: &Bytes,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(value.as_ref())
    }
}

impl<'de> DeserializeAs<'de, Bytes> for SerTxId {
    fn deserialize_as<D>(deserializer: D) -> Result<Bytes, D::Error>
    where
        D: Deserializer<'de>,
    {
        SerBytes::deserialize_as(deserializer)
    }
}

impl SerializeAs<Vec<u8>> for SerTxId {
    #[inline]
    fn serialize_as<S>(
        value: &Vec<u8>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(value)
    }
}

impl<'de> DeserializeAs<'de, Vec<u8>> for SerTxId {
    fn deserialize_as<D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        SerBytes::deserialize_as(deserializer).map(|bytes| bytes.to_vec())
    }
}

/// `Option<B256>` <-> NON nullable `FixedString(32)`: `None` is stored as
/// 32 zero bytes (log topics). Reading maps zero bytes back to `None`, so a
/// genuine all-zero topic does not survive a round trip through this
/// adapter alone: `DatabaseLog` reads back through `logs.topic_count`
/// instead, which does keep the distinction.
pub struct SerTopic(());

impl SerializeAs<Option<B256>> for SerTopic {
    #[inline]
    fn serialize_as<S>(
        value: &Option<B256>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let bytes: &[u8; 32] = match value {
            Some(topic) => &topic.0,
            None => &B256::ZERO.0,
        };
        bytes.serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, Option<B256>> for SerTopic {
    fn deserialize_as<D>(deserializer: D) -> Result<Option<B256>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let topic =
            <[u8; 32]>::deserialize(deserializer).map(B256::from)?;
        Ok((!topic.is_zero()).then_some(topic))
    }
}

/// `U256` <-> `UInt256`: four little endian `u64` limbs, least significant
/// first, i.e. 32 little endian bytes on the wire.
pub struct SerU256(());

impl SerializeAs<U256> for SerU256 {
    #[inline]
    fn serialize_as<S>(
        value: &U256,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value.as_limbs().serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, U256> for SerU256 {
    fn deserialize_as<D>(deserializer: D) -> Result<U256, D::Error>
    where
        D: Deserializer<'de>,
    {
        <[u64; 4]>::deserialize(deserializer).map(U256::from_limbs)
    }
}

/// `I256` <-> `Int256`: the two's complement bit pattern, written exactly
/// like a `U256`.
pub struct SerI256(());

impl SerializeAs<I256> for SerI256 {
    #[inline]
    fn serialize_as<S>(
        value: &I256,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value.into_raw().as_limbs().serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, I256> for SerI256 {
    fn deserialize_as<D>(deserializer: D) -> Result<I256, D::Error>
    where
        D: Deserializer<'de>,
    {
        <[u64; 4]>::deserialize(deserializer)
            .map(|limbs| I256::from_raw(U256::from_limbs(limbs)))
    }
}

/// `Bytes` <-> `String` holding the raw bytes (NOT hex, NOT utf-8).
pub struct SerBytes(());

impl SerializeAs<Bytes> for SerBytes {
    #[inline]
    fn serialize_as<S>(
        value: &Bytes,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(value.as_ref())
    }
}

impl<'de> DeserializeAs<'de, Bytes> for SerBytes {
    fn deserialize_as<D>(deserializer: D) -> Result<Bytes, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BytesVisitor;

        impl<'de> Visitor<'de> for BytesVisitor {
            type Value = Bytes;

            fn expecting(
                &self,
                f: &mut fmt::Formatter<'_>,
            ) -> fmt::Result {
                f.write_str("a byte string")
            }

            fn visit_bytes<E: de::Error>(
                self,
                value: &[u8],
            ) -> Result<Bytes, E> {
                Ok(Bytes::copy_from_slice(value))
            }

            fn visit_byte_buf<E: de::Error>(
                self,
                value: Vec<u8>,
            ) -> Result<Bytes, E> {
                Ok(Bytes::from(value))
            }

            // Self describing formats without a bytes type (JSON).
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Bytes, A::Error> {
                let mut bytes =
                    Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(byte) = seq.next_element::<u8>()? {
                    bytes.push(byte);
                }
                Ok(Bytes::from(bytes))
            }
        }

        deserializer.deserialize_byte_buf(BytesVisitor)
    }
}

/// First four bytes of the calldata, zeros when it is shorter (plain
/// transfers, malformed calls).
pub fn method_selector(input: &[u8]) -> Selector {
    input
        .first_chunk::<4>()
        .map(|selector| Selector::from(*selector))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_with::serde_as;

    /// Minimal RowBinary-like serializer: fixed width integers little
    /// endian, tuples without a prefix, bytes / sequences with a one byte
    /// length (enough for the tests), `Option` with a one byte tag.
    /// Mirrors what `clickhouse::rowbinary::ser` does for these shapes.
    mod wire {
        use serde::{ser, Serialize};
        use std::fmt;

        #[derive(Debug)]
        pub struct Error(String);

        impl fmt::Display for Error {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl std::error::Error for Error {}

        impl ser::Error for Error {
            fn custom<T: fmt::Display>(msg: T) -> Self {
                Error(msg.to_string())
            }
        }

        #[derive(Default)]
        pub struct Wire(pub Vec<u8>);

        pub fn to_bytes<T: Serialize>(value: &T) -> Vec<u8> {
            let mut wire = Wire::default();
            value.serialize(&mut wire).unwrap();
            wire.0
        }

        macro_rules! unsupported {
            ($($method:ident: $ty:ty),*) => {$(
                fn $method(self, _: $ty) -> Result<(), Error> {
                    Err(ser::Error::custom(stringify!($method)))
                }
            )*};
        }

        impl ser::Serializer for &mut Wire {
            type Ok = ();
            type Error = Error;
            type SerializeSeq = Self;
            type SerializeTuple = Self;
            type SerializeTupleStruct = ser::Impossible<(), Error>;
            type SerializeTupleVariant = ser::Impossible<(), Error>;
            type SerializeMap = ser::Impossible<(), Error>;
            type SerializeStruct = Self;
            type SerializeStructVariant = ser::Impossible<(), Error>;

            unsupported!(
                serialize_bool: bool, serialize_i8: i8, serialize_i16: i16,
                serialize_i32: i32, serialize_i64: i64, serialize_u16: u16,
                serialize_f32: f32, serialize_f64: f64, serialize_char: char
            );

            fn serialize_u8(self, v: u8) -> Result<(), Error> {
                self.0.push(v);
                Ok(())
            }

            fn serialize_u32(self, v: u32) -> Result<(), Error> {
                self.0.extend(v.to_le_bytes());
                Ok(())
            }

            fn serialize_u64(self, v: u64) -> Result<(), Error> {
                self.0.extend(v.to_le_bytes());
                Ok(())
            }

            fn serialize_str(self, v: &str) -> Result<(), Error> {
                self.serialize_bytes(v.as_bytes())
            }

            fn serialize_bytes(self, v: &[u8]) -> Result<(), Error> {
                self.0.push(u8::try_from(v.len()).unwrap());
                self.0.extend(v);
                Ok(())
            }

            fn serialize_none(self) -> Result<(), Error> {
                self.0.push(1);
                Ok(())
            }

            fn serialize_some<T: ?Sized + Serialize>(
                self,
                value: &T,
            ) -> Result<(), Error> {
                self.0.push(0);
                value.serialize(self)
            }

            fn serialize_unit(self) -> Result<(), Error> {
                Err(ser::Error::custom("unit"))
            }

            fn serialize_unit_struct(
                self,
                _: &'static str,
            ) -> Result<(), Error> {
                Err(ser::Error::custom("unit struct"))
            }

            fn serialize_unit_variant(
                self,
                _: &'static str,
                _: u32,
                _: &'static str,
            ) -> Result<(), Error> {
                Err(ser::Error::custom("unit variant"))
            }

            fn serialize_newtype_struct<T: ?Sized + Serialize>(
                self,
                _: &'static str,
                value: &T,
            ) -> Result<(), Error> {
                value.serialize(self)
            }

            fn serialize_newtype_variant<T: ?Sized + Serialize>(
                self,
                _: &'static str,
                _: u32,
                _: &'static str,
                _: &T,
            ) -> Result<(), Error> {
                Err(ser::Error::custom("newtype variant"))
            }

            fn serialize_seq(
                self,
                len: Option<usize>,
            ) -> Result<Self, Error> {
                self.0.push(u8::try_from(len.unwrap()).unwrap());
                Ok(self)
            }

            fn serialize_tuple(self, _: usize) -> Result<Self, Error> {
                Ok(self)
            }

            fn serialize_tuple_struct(
                self,
                _: &'static str,
                _: usize,
            ) -> Result<Self::SerializeTupleStruct, Error> {
                Err(ser::Error::custom("tuple struct"))
            }

            fn serialize_tuple_variant(
                self,
                _: &'static str,
                _: u32,
                _: &'static str,
                _: usize,
            ) -> Result<Self::SerializeTupleVariant, Error> {
                Err(ser::Error::custom("tuple variant"))
            }

            fn serialize_map(
                self,
                _: Option<usize>,
            ) -> Result<Self::SerializeMap, Error> {
                Err(ser::Error::custom("map"))
            }

            fn serialize_struct(
                self,
                _: &'static str,
                _: usize,
            ) -> Result<Self, Error> {
                Ok(self)
            }

            fn serialize_struct_variant(
                self,
                _: &'static str,
                _: u32,
                _: &'static str,
                _: usize,
            ) -> Result<Self::SerializeStructVariant, Error> {
                Err(ser::Error::custom("struct variant"))
            }
        }

        impl ser::SerializeSeq for &mut Wire {
            type Ok = ();
            type Error = Error;

            fn serialize_element<T: ?Sized + Serialize>(
                &mut self,
                value: &T,
            ) -> Result<(), Error> {
                value.serialize(&mut **self)
            }

            fn end(self) -> Result<(), Error> {
                Ok(())
            }
        }

        impl ser::SerializeTuple for &mut Wire {
            type Ok = ();
            type Error = Error;

            fn serialize_element<T: ?Sized + Serialize>(
                &mut self,
                value: &T,
            ) -> Result<(), Error> {
                value.serialize(&mut **self)
            }

            fn end(self) -> Result<(), Error> {
                Ok(())
            }
        }

        impl ser::SerializeStruct for &mut Wire {
            type Ok = ();
            type Error = Error;

            fn serialize_field<T: ?Sized + Serialize>(
                &mut self,
                _: &'static str,
                value: &T,
            ) -> Result<(), Error> {
                value.serialize(&mut **self)
            }

            fn end(self) -> Result<(), Error> {
                Ok(())
            }
        }
    }

    #[serde_as]
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Everything {
        #[serde_as(as = "SerB256")]
        hash: B256,
        #[serde_as(as = "SerAddress")]
        address: Address,
        #[serde_as(as = "Option<SerAddress>")]
        to: Option<Address>,
        #[serde_as(as = "SerB64")]
        nonce: B64,
        #[serde_as(as = "SerSelector")]
        method: Selector,
        #[serde_as(as = "SerTopic")]
        topic: Option<B256>,
        #[serde_as(as = "SerU256")]
        value: U256,
        #[serde_as(as = "Option<SerU256>")]
        fee: Option<U256>,
        #[serde_as(as = "SerI256")]
        delta: I256,
        #[serde_as(as = "SerBytes")]
        data: Bytes,
        #[serde_as(as = "Vec<SerU256>")]
        ids: Vec<U256>,
        #[serde_as(as = "Vec<(SerAddress, Vec<SerB256>)>")]
        access_list: Vec<(Address, Vec<B256>)>,
    }

    fn sample() -> Everything {
        Everything {
            hash: B256::repeat_byte(0xab),
            address: Address::repeat_byte(0x11),
            to: None,
            nonce: B64::repeat_byte(0x42),
            method: Selector::from([0xa9, 0x05, 0x9c, 0xbb]),
            topic: None,
            value: (U256::from(1u8) << 200) + U256::from(7u8),
            fee: Some(U256::from(9u8)),
            delta: I256::MINUS_ONE,
            data: Bytes::from(vec![0x00, 0xff, 0x80]),
            ids: vec![U256::from(1u8), U256::MAX],
            access_list: vec![(
                Address::repeat_byte(2),
                vec![B256::repeat_byte(3)],
            )],
        }
    }

    #[test]
    fn fixed_strings_are_raw_bytes_without_a_length_prefix() {
        #[serde_as]
        #[derive(Serialize)]
        struct Row {
            #[serde_as(as = "SerB256")]
            hash: B256,
            #[serde_as(as = "SerAddress")]
            address: Address,
            #[serde_as(as = "SerSelector")]
            method: Selector,
            #[serde_as(as = "SerB64")]
            nonce: B64,
        }

        let bytes = wire::to_bytes(&Row {
            hash: B256::repeat_byte(0xab),
            address: Address::repeat_byte(0x11),
            method: Selector::from([1, 2, 3, 4]),
            nonce: B64::repeat_byte(0x42),
        });

        let mut expected = vec![0xab; 32];
        expected.extend([0x11; 20]);
        expected.extend([1, 2, 3, 4]);
        expected.extend([0x42; 8]);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn u256_is_32_little_endian_bytes() {
        #[serde_as]
        #[derive(Serialize)]
        struct Row {
            #[serde_as(as = "SerU256")]
            value: U256,
        }

        for value in [
            U256::ZERO,
            U256::from(1u8),
            U256::from(u64::MAX),
            (U256::from(1u8) << 128) + U256::from(0x0102u16),
            U256::MAX,
        ] {
            assert_eq!(
                wire::to_bytes(&Row { value }),
                value.to_le_bytes::<32>().to_vec(),
                "{value}"
            );
        }
    }

    #[test]
    fn i256_is_little_endian_twos_complement() {
        #[serde_as]
        #[derive(Serialize)]
        struct Row {
            #[serde_as(as = "SerI256")]
            value: I256,
        }

        assert_eq!(
            wire::to_bytes(&Row { value: I256::MINUS_ONE }),
            vec![0xff; 32]
        );

        let minus_two = I256::try_from(-2i64).unwrap();
        let mut expected = vec![0xff; 32];
        expected[0] = 0xfe;
        assert_eq!(wire::to_bytes(&Row { value: minus_two }), expected);

        for value in [I256::ZERO, I256::MIN, I256::MAX, I256::ONE] {
            assert_eq!(
                wire::to_bytes(&Row { value }),
                value.to_le_bytes::<32>().to_vec(),
                "{value}"
            );
        }
    }

    #[test]
    fn bytes_are_length_prefixed_raw_bytes_not_hex() {
        #[serde_as]
        #[derive(Serialize)]
        struct Row {
            #[serde_as(as = "SerBytes")]
            data: Bytes,
        }

        assert_eq!(
            wire::to_bytes(&Row {
                data: Bytes::from(vec![0, 0xff, 0x80])
            }),
            vec![3, 0, 0xff, 0x80]
        );
        assert_eq!(wire::to_bytes(&Row { data: Bytes::new() }), vec![0]);
    }

    #[test]
    fn missing_topics_are_zero_bytes_and_nullable_columns_are_tagged() {
        #[serde_as]
        #[derive(Serialize)]
        struct Row {
            #[serde_as(as = "SerTopic")]
            topic: Option<B256>,
            #[serde_as(as = "Option<SerAddress>")]
            to: Option<Address>,
        }

        // Non nullable column: no tag, 32 zero bytes. Nullable: tag only.
        let mut expected = vec![0u8; 32];
        expected.push(1);
        assert_eq!(
            wire::to_bytes(&Row { topic: None, to: None }),
            expected
        );

        let mut expected = vec![9u8; 32];
        expected.push(0);
        expected.extend([7u8; 20]);
        assert_eq!(
            wire::to_bytes(&Row {
                topic: Some(B256::repeat_byte(9)),
                to: Some(Address::repeat_byte(7)),
            }),
            expected
        );
    }

    #[test]
    fn every_adapter_round_trips() {
        // JSON exercises the Deserialize side (tuples / sequences), the
        // real RowBinary round trip is covered by the integration tests.
        let json = serde_json::to_string(&sample()).unwrap();
        let back: Everything = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sample());

        let mut with_values = sample();
        with_values.to = Some(Address::repeat_byte(5));
        with_values.topic = Some(B256::repeat_byte(6));
        with_values.delta = I256::MIN;
        let json = serde_json::to_string(&with_values).unwrap();
        let back: Everything = serde_json::from_str(&json).unwrap();
        assert_eq!(back, with_values);
    }

    #[test]
    fn method_selector_handles_short_input() {
        assert_eq!(method_selector(&[]), Selector::ZERO);
        assert_eq!(method_selector(&[1, 2, 3]), Selector::ZERO);
        assert_eq!(
            method_selector(&[0xa9, 0x05, 0x9c, 0xbb, 0xff]),
            Selector::from([0xa9, 0x05, 0x9c, 0xbb])
        );
    }
}
