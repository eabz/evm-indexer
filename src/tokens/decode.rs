//! Tolerant decoding of `name()` / `symbol()` / `decimals()` return data.
//!
//! Token contracts in the wild are messy: old tokens (MKR, SAI, ...) return
//! `bytes32` instead of `string`, some return invalid UTF-8, some return
//! megabytes of junk. Nothing in here ever fails loudly: undecodable data
//! simply yields `None` and whatever is decoded is sanitized before it can
//! reach ClickHouse or Redis.

use alloy::primitives::U256;

/// Maximum number of characters kept for a token name or symbol.
pub const MAX_STRING_CHARS: usize = 256;

const WORD: usize = 32;

/// Removes NUL bytes and other control characters, trims surrounding
/// whitespace and caps the length to [`MAX_STRING_CHARS`] characters.
pub fn sanitize(input: &str) -> String {
    input
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_STRING_CHARS)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Reads an ABI word as `usize`, `None` if it does not fit.
fn word_as_usize(word: &[u8]) -> Option<usize> {
    let value = U256::try_from_be_slice(word)?;
    usize::try_from(value).ok()
}

/// Decodes a standard ABI encoded dynamic `string` (or `bytes`) return
/// value. Invalid UTF-8 is converted lossily instead of being rejected.
fn decode_abi_string(data: &[u8]) -> Option<String> {
    if data.len() < 2 * WORD {
        return None;
    }

    let offset = word_as_usize(&data[..WORD])?;
    let length_end = offset.checked_add(WORD)?;
    let length = word_as_usize(data.get(offset..length_end)?)?;
    let bytes = data.get(length_end..length_end.checked_add(length)?)?;

    // Only look at a bounded prefix: a single UTF-8 char is at most 4 bytes
    // so this is always enough to fill MAX_STRING_CHARS.
    let bytes = &bytes[..bytes.len().min(MAX_STRING_CHARS * 4 + 8)];

    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Decodes a `bytes32` return value as a string (MKR style tokens).
///
/// Only trailing NUL padding is accepted: values with a leading NUL byte
/// (left padded number/address/bool) or interior NUL bytes are rejected as
/// garbage.
fn decode_bytes32_string(data: &[u8]) -> Option<String> {
    if data.len() != WORD {
        return None;
    }

    let end = data.iter().rposition(|b| *b != 0)? + 1;
    let bytes = &data[..end];

    if bytes.contains(&0) {
        return None;
    }

    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Decodes the return data of `name()` / `symbol()`.
///
/// Tries the standard dynamic `string` encoding first and falls back to
/// `bytes32`. Returns `None` for empty / garbage data. The returned string
/// is always sanitized.
pub fn decode_string(data: &[u8]) -> Option<String> {
    let raw =
        decode_abi_string(data).or_else(|| decode_bytes32_string(data))?;

    Some(sanitize(&raw))
}

/// Decodes the return data of `decimals()`.
///
/// Accepts any integer width (some tokens return `uint256`), values that do
/// not fit a `u8` are treated as garbage.
pub fn decode_decimals(data: &[u8]) -> Option<u8> {
    if data.len() < WORD {
        return None;
    }

    let value = U256::try_from_be_slice(&data[..WORD])?;

    u8::try_from(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::{
        primitives::{Address, B256},
        sol_types::SolValue,
    };

    fn bytes32(text: &[u8]) -> Vec<u8> {
        let mut word = [0u8; 32];
        word[..text.len()].copy_from_slice(text);
        word.to_vec()
    }

    #[test]
    fn decodes_standard_string() {
        let data = "Wrapped Ether".to_string().abi_encode();
        assert_eq!(decode_string(&data).as_deref(), Some("Wrapped Ether"));
    }

    #[test]
    fn decodes_empty_string() {
        let data = String::new().abi_encode();
        assert_eq!(decode_string(&data).as_deref(), Some(""));
    }

    #[test]
    fn decodes_long_string_and_caps_it() {
        let data = "a".repeat(10_000).abi_encode();
        let decoded = decode_string(&data).unwrap();
        assert_eq!(decoded.chars().count(), MAX_STRING_CHARS);
    }

    #[test]
    fn caps_multibyte_strings_on_char_boundaries() {
        let data = "é".repeat(1_000).abi_encode();
        let decoded = decode_string(&data).unwrap();
        assert_eq!(decoded.chars().count(), MAX_STRING_CHARS);
        assert!(decoded.chars().all(|c| c == 'é'));
    }

    #[test]
    fn decodes_bytes32_fallback() {
        // MKR returns bytes32("MKR") / bytes32("Maker").
        assert_eq!(
            decode_string(&bytes32(b"MKR")).as_deref(),
            Some("MKR")
        );
        assert_eq!(
            decode_string(&bytes32(b"Maker")).as_deref(),
            Some("Maker")
        );
        // A full 32 byte value without any padding.
        let full = [b'x'; 32];
        assert_eq!(
            decode_string(&full).as_deref(),
            Some("x".repeat(32).as_str())
        );
    }

    #[test]
    fn bytes32_is_lossy_on_invalid_utf8() {
        let decoded =
            decode_string(&bytes32(&[b'A', 0xff, b'B'])).unwrap();
        assert_eq!(decoded, "A\u{fffd}B");
    }

    #[test]
    fn abi_string_is_lossy_on_invalid_utf8() {
        // `bytes` and `string` share the same encoding.
        let data = alloy::primitives::Bytes::from(vec![b'A', 0xff, b'B'])
            .abi_encode();
        assert_eq!(decode_string(&data).as_deref(), Some("A\u{fffd}B"));
    }

    #[test]
    fn rejects_garbage() {
        // Empty return data (EOA / self-destructed contract).
        assert_eq!(decode_string(&[]), None);
        // Left padded values are numbers/addresses, not bytes32 strings.
        assert_eq!(decode_string(&U256::from(18u8).abi_encode()), None);
        assert_eq!(
            decode_string(&Address::repeat_byte(0x11).abi_encode()),
            None
        );
        assert_eq!(decode_string(&B256::ZERO.abi_encode()), None);
        // Interior NUL bytes in a bytes32.
        assert_eq!(decode_string(&bytes32(b"AB\0CD")), None);
        // Odd sized data.
        assert_eq!(decode_string(&[1, 2, 3]), None);
        assert_eq!(decode_string(&[0xffu8; 31]), None);
        // Offset pointing outside of the data.
        assert_eq!(decode_string(&[0xffu8; 64]), None);
        // Length larger than the data.
        let mut data = "abc".to_string().abi_encode();
        data[63] = 0xff;
        assert_eq!(decode_string(&data), None);
        // Huge offset/length must not overflow.
        let mut data = vec![0u8; 96];
        data[..32].copy_from_slice(&U256::MAX.to_be_bytes::<32>());
        assert_eq!(decode_string(&data), None);
        let mut data = vec![0u8; 96];
        data[31] = 32;
        data[32..64].copy_from_slice(&U256::MAX.to_be_bytes::<32>());
        assert_eq!(decode_string(&data), None);
    }

    #[test]
    fn strips_nul_and_control_characters() {
        let data = "\0Tok\0en\n\tName\u{0}  ".to_string().abi_encode();
        assert_eq!(decode_string(&data).as_deref(), Some("TokenName"));
        assert_eq!(sanitize("  spaced out  "), "spaced out");
        assert_eq!(sanitize("\0\0\0"), "");
    }

    #[test]
    fn sanitize_never_leaves_trailing_whitespace_after_cap() {
        let input =
            format!("{}{}", "a".repeat(MAX_STRING_CHARS - 1), "  b");
        let output = sanitize(&input);
        assert_eq!(output, "a".repeat(MAX_STRING_CHARS - 1));
    }

    #[test]
    fn decodes_decimals() {
        assert_eq!(
            decode_decimals(&U256::from(18u8).abi_encode()),
            Some(18)
        );
        assert_eq!(
            decode_decimals(&U256::from(6u8).abi_encode()),
            Some(6)
        );
        assert_eq!(
            decode_decimals(&U256::from(255u8).abi_encode()),
            Some(255)
        );
        assert_eq!(
            decode_decimals(&U256::from(256u16).abi_encode()),
            None
        );
        assert_eq!(decode_decimals(&U256::MAX.abi_encode()), None);
        assert_eq!(decode_decimals(&[]), None);
        assert_eq!(decode_decimals(&[18]), None);
    }
}
