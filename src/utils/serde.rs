use core::fmt;

use primitive_types::U256;
use serde::de::{SeqAccess, Visitor};

pub mod u256 {
    use primitive_types::U256;
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

pub mod opt_u256 {

    use primitive_types::U256;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::utils::serde::u256;

    pub fn serialize<S>(
        value: &Option<U256>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct Helper<'a>(#[serde(with = "u256")] &'a U256);
        value.as_ref().map(Helper).serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<Option<U256>, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Helper(#[serde(with = "u256")] U256);
        let helper = Option::deserialize(deserializer)?;
        Ok(helper.map(|Helper(external)| external))
    }
}

struct Uint256VectorDeserializer;

impl<'de> Visitor<'de> for Uint256VectorDeserializer {
    type Value = Vec<U256>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("Vec<U256>")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut vec = Vec::new();

        let length: usize = seq
            .next_element()?
            .ok_or_else(|| serde::de::Error::invalid_length(1, &self))?;

        for _ in 0..length {
            vec.push(seq.next_element().unwrap().ok_or_else(|| {
                serde::de::Error::invalid_length(1, &self)
            })?);
        }

        Ok(vec)
    }
}
pub mod vec_u256 {

    use primitive_types::U256;
    use serde::{ser::SerializeSeq, Deserializer, Serializer};

    use super::Uint256VectorDeserializer;

    pub fn serialize<S>(
        value: &Vec<U256>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(value.len())).unwrap();
        for e in value {
            seq.serialize_element(e).unwrap();
        }
        seq.end()
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<Vec<U256>, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(Uint256VectorDeserializer)
    }
}
