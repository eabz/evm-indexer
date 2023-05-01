use std::collections::HashMap;

use super::BalanceAllocation;

static GENESIS_ALLOCATION: &str = r#"{
  "0x446aa6e0dc65690403df3f127750da1322941f3e": {
    "balance": "0x1b1ae4d6e2ef500000"
  },
  "0xb005741528b86f5952469d80a8614591e3c5b632": {
    "balance": "0x1b1ae4d6e2ef500000"
  },
  "0x0000000000000000000000000000000000001004": {
    "balance": "0x91eb549e49e7a157ba0000"
  }
}"#;

pub fn get_genesis_allocation() -> HashMap<String, BalanceAllocation> {
    let allocations: HashMap<String, BalanceAllocation> =
        serde_json::from_str(GENESIS_ALLOCATION).unwrap();

    allocations
}
