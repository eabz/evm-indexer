use alloy::primitives::{Address, Bloom, Bytes, B256, B64, U256};
use serde::{self, Deserialize, Deserializer, Serialize, Serializer};
use serde_with::{DeserializeAs, SerializeAs};

pub struct SerU256(());

/// Serializer for B256 (32-byte hash) as hex string with 0x prefix
pub struct SerB256(());

impl SerializeAs<B256> for SerB256 {
    fn serialize_as<S>(x: &B256, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        format!("{:?}", x).serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, B256> for SerB256 {
    fn deserialize_as<D>(deserializer: D) -> Result<B256, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Serializer for Address (20-byte address) as hex string with 0x prefix
pub struct SerAddress(());

impl SerializeAs<Address> for SerAddress {
    fn serialize_as<S>(
        x: &Address,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        format!("{:?}", x).serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, Address> for SerAddress {
    fn deserialize_as<D>(deserializer: D) -> Result<Address, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Serializer for Bloom (256-byte bloom filter) as hex string with 0x prefix
pub struct SerBloom(());

impl SerializeAs<Bloom> for SerBloom {
    fn serialize_as<S>(x: &Bloom, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        format!("{:?}", x).serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, Bloom> for SerBloom {
    fn deserialize_as<D>(deserializer: D) -> Result<Bloom, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Serializer for Bytes (variable-length bytes) as hex string with 0x prefix
pub struct SerBytes(());

impl SerializeAs<Bytes> for SerBytes {
    fn serialize_as<S>(x: &Bytes, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        format!("{:?}", x).serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, Bytes> for SerBytes {
    fn deserialize_as<D>(deserializer: D) -> Result<Bytes, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Serializer for B64 (8-byte nonce) as hex string with 0x prefix
pub struct SerB64(());

impl SerializeAs<B64> for SerB64 {
    fn serialize_as<S>(x: &B64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        format!("{:?}", x).serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, B64> for SerB64 {
    fn deserialize_as<D>(deserializer: D) -> Result<B64, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Serializer for Vec<B256> as array of hex strings
pub struct SerVecB256(());

impl SerializeAs<Vec<B256>> for SerVecB256 {
    fn serialize_as<S>(
        x: &Vec<B256>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let hex_strings: Vec<String> =
            x.iter().map(|h| format!("{:?}", h)).collect();
        hex_strings.serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, Vec<B256>> for SerVecB256 {
    fn deserialize_as<D>(deserializer: D) -> Result<Vec<B256>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let strings: Vec<String> = Deserialize::deserialize(deserializer)?;
        strings
            .into_iter()
            .map(|s| s.parse().map_err(serde::de::Error::custom))
            .collect()
    }
}

impl SerializeAs<U256> for SerU256 {
    fn serialize_as<S>(x: &U256, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Serialize as hex string for ClickHouse compatibility
        let hex_string = format!("{:x}", x);
        hex_string.serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, U256> for SerU256 {
    fn deserialize_as<D>(deserializer: D) -> Result<U256, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        U256::from_str_radix(&s, 16).map_err(serde::de::Error::custom)
    }
}

/// Serializer for access list Vec<(Address, Vec<B256>)> as array of tuples with hex strings
pub struct SerAccessList(());

impl SerializeAs<Vec<(Address, Vec<B256>)>> for SerAccessList {
    fn serialize_as<S>(
        x: &Vec<(Address, Vec<B256>)>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let hex_list: Vec<(String, Vec<String>)> = x
            .iter()
            .map(|(addr, keys)| {
                (
                    format!("{:?}", addr),
                    keys.iter().map(|k| format!("{:?}", k)).collect(),
                )
            })
            .collect();
        hex_list.serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, Vec<(Address, Vec<B256>)>> for SerAccessList {
    fn deserialize_as<D>(
        deserializer: D,
    ) -> Result<Vec<(Address, Vec<B256>)>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let hex_list: Vec<(String, Vec<String>)> =
            Deserialize::deserialize(deserializer)?;
        hex_list
            .into_iter()
            .map(|(addr_str, keys_str)| {
                let addr: Address =
                    addr_str.parse().map_err(serde::de::Error::custom)?;
                let keys: Result<Vec<B256>, _> = keys_str
                    .into_iter()
                    .map(|k| k.parse().map_err(serde::de::Error::custom))
                    .collect();
                Ok((addr, keys?))
            })
            .collect()
    }
}
