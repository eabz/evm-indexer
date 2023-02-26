use ethers::types::{Bytes, H160, H256, H64, U256, U64};

pub fn format_nonce(h: H64) -> String {
    return format!("{:?}", h);
}

pub fn format_bool(h: U64) -> bool {
    let data = format!("{:?}", h);
    return data == "1";
}

pub fn format_hash(h: H256) -> String {
    return format!("{:?}", h);
}

pub fn format_address(h: H160) -> String {
    return format!("{:?}", h);
}

pub fn format_number(n: U256) -> String {
    return format!("{}", n.to_string());
}

pub fn format_small_number(n: U64) -> String {
    return format!("{}", n.to_string());
}

pub fn format_bytes(b: &Bytes) -> String {
    return format!("{}", serde_json::to_string(b).unwrap().replace("\"", ""));
}

pub fn byte4_from_input(input: &String) -> [u8; 4] {
    let input_sanitized = input.strip_prefix("0x").unwrap();

    if input_sanitized == "" {
        return [0x00, 0x00, 0x00, 0x00];
    }

    let input_bytes = hex::decode(input_sanitized).unwrap();

    if input_bytes.len() < 4 {
        return [0x00, 0x00, 0x00, 0x00];
    }

    let byte4: [u8; 4] = [
        input_bytes[0],
        input_bytes[1],
        input_bytes[2],
        input_bytes[3],
    ];

    return byte4;
}
