use alloy::primitives::U256;
use serde::{self, Deserialize, Deserializer, Serialize, Serializer};
use serde_with::{DeserializeAs, SerializeAs};

pub struct SerU256(());

impl SerializeAs<U256> for SerU256 {
    fn serialize_as<S>(x: &U256, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let buf: [u8; 32] = x.to_le_bytes();
        buf.serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, U256> for SerU256 {
    fn deserialize_as<D>(deserializer: D) -> Result<U256, D::Error>
    where
        D: Deserializer<'de>,
    {
        let u: [u8; 32] = Deserialize::deserialize(deserializer)?;
        Ok(U256::from_le_bytes(u))
    }
}
