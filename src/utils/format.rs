use ethers::types::{Bytes, H160, H256, H64};
use primitive_types::U256;
use serde::{self, Deserialize, Deserializer, Serialize, Serializer};
use serde_with::{DeserializeAs, SerializeAs};

pub fn format_nonce(h: H64) -> String {
    format!("{:?}", h)
}

pub fn format_hash(h: H256) -> String {
    format!("{:?}", h)
}

pub fn format_address(h: H160) -> String {
    format!("{:?}", h)
}

pub fn format_bytes(b: &Bytes) -> String {
    serde_json::to_string(b).unwrap().replace('\"', "")
}

pub fn decode_bytes(s: String) -> Vec<u8> {
    let without_prefix = &s[2..];
    hex::decode(without_prefix).unwrap()
}

pub fn format_bytes_slice(b: &[u8]) -> String {
    format!("0x{}", hex::encode(b))
}

pub fn byte4_from_input(input: &str) -> [u8; 4] {
    let input_sanitized = input.strip_prefix("0x").unwrap();

    if input_sanitized.is_empty() {
        return [0x00, 0x00, 0x00, 0x00];
    }

    let input_bytes = hex::decode(input_sanitized).unwrap();

    if input_bytes.len() < 4 {
        return [0x00, 0x00, 0x00, 0x00];
    }

    let byte4: [u8; 4] =
        [input_bytes[0], input_bytes[1], input_bytes[2], input_bytes[3]];

    byte4
}

pub struct SerU256(U256);

impl SerializeAs<U256> for SerU256 {
    fn serialize_as<S>(x: &U256, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut buf: [u8; 32] = [0; 32];
        x.to_little_endian(&mut buf);
        buf.serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, U256> for SerU256 {
    fn deserialize_as<D>(deserializer: D) -> Result<U256, D::Error>
    where
        D: Deserializer<'de>,
    {
        let u: [u8; 32] = Deserialize::deserialize(deserializer)?;
        Ok(U256::from_little_endian(&u))
    }
}
