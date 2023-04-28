use ethers::types::{Bytes, H160, H256, H64};

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

pub fn sanitize_string(str: String) -> String {
    let trim = str.trim_matches(char::from(0)).to_string();

    let remove_single_quotes: String = trim.replace('\'', "");

    let to_bytes = remove_single_quotes.as_bytes();

    let remove_non_utf8_chars = String::from_utf8_lossy(to_bytes);

    format!("'{}'", remove_non_utf8_chars)
}

pub mod serialize_u256 {
    use ethabi::ethereum_types::U256;
    use serde::{
        de::{Deserialize, Deserializer},
        ser::{Serialize, Serializer},
    };

    pub fn serialize<S: Serializer>(
        u: &U256,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut buf: [u8; 32] = [0; 32];
        u.to_little_endian(&mut buf);
        buf.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<U256, D::Error>
    where
        D: Deserializer<'de>,
    {
        let u: [u8; 32] = Deserialize::deserialize(deserializer)?;
        Ok(U256::from_little_endian(&u))
    }
}

pub mod opt_serialize_u256 {
    use ethabi::ethereum_types::U256;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::utils::format::serialize_u256;

    pub fn serialize<S>(
        value: &Option<U256>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct Helper<'a>(#[serde(with = "serialize_u256")] &'a U256);
        value.as_ref().map(Helper).serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<Option<U256>, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Helper(#[serde(with = "serialize_u256")] U256);
        let helper = Option::deserialize(deserializer)?;
        Ok(helper.map(|Helper(external)| external))
    }
}
