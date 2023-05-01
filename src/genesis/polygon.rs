use std::collections::HashMap;

use super::BalanceAllocation;

static GENESIS_ALLOCATION: &str = r#"{
  "0x42eefcda06ead475cde3731b8eb138e88cd0bac3": {
    "balance": "0x3635c9adc5dea00000"
  },
  "0x0375b2fc7140977c9c76d45421564e354ed42277": {
    "balance": "0x3635c9adc5dea00000"
  },
  "0x7fcd58c2d53d980b247f1612fdba93e9a76193e6": {
    "balance": "0x3635c9adc5dea00000"
  },
  "0xb702f1c9154ac9c08da247a8e30ee6f2f3373f41": {
    "balance": "0x3635c9adc5dea00000"
  },
  "0xf84c74dea96df0ec22e11e7c33996c73fcc2d822": {
    "balance": "0x3635c9adc5dea00000"
  },
  "0xb8bb158b93c94ed35c1970d610d1e2b34e26652c": {
    "balance": "0x3635c9adc5dea00000"
  },
  "0x5973918275c01f50555d44e92c9d9b353cadad54": {
    "balance": "0x3635c9adc5dea00000"
  },
  "0x0000000000000000000000000000000000001010": {
    "balance": "0x204fcce2c5a141f7f9a00000"
  }
}"#;

pub fn get_genesis_allocation() -> HashMap<String, BalanceAllocation> {
    let allocations: HashMap<String, BalanceAllocation> =
        serde_json::from_str(GENESIS_ALLOCATION).unwrap();

    allocations
}
