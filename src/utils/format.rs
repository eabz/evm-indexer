use alloy::primitives::U256;
use serde::{self, Deserialize, Deserializer, Serialize, Serializer};
use serde_with::{DeserializeAs, SerializeAs};

pub struct SerU256(());

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
