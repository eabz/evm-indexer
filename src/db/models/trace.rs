use clickhouse::Row;
use ethabi::ethereum_types::U256;
use ethers::types::Trace;
use serde::{Deserialize, Serialize};

use crate::utils::format::{
    format_address, format_bytes, format_hash, opt_serialize_u256,
};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseTrace {
    pub action_type: String,
    pub address: Option<String>,
    pub author: Option<String>,
    #[serde(with = "opt_serialize_u256")]
    pub balance: Option<U256>,
    pub block_hash: String,
    pub block_number: u64,
    pub call_type: Option<String>,
    pub chain: u64,
    pub code: Option<String>,
    pub error: Option<String>,
    pub from: Option<String>,
    #[serde(with = "opt_serialize_u256")]
    pub gas: Option<U256>,
    #[serde(with = "opt_serialize_u256")]
    pub gas_used: Option<U256>,
    pub init: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub refund_address: Option<String>,
    pub reward_type: Option<String>,
    pub subtraces: u64,
    pub to: Option<String>,
    pub trace_address: Vec<u64>,
    pub transaction_hash: Option<String>,
    pub transaction_position: Option<u64>,
    #[serde(with = "opt_serialize_u256")]
    pub value: Option<U256>,
}

impl DatabaseTrace {
    pub fn from_rpc(trace: &Trace, chain: u64) -> Self {
        let trace_address = trace
            .trace_address
            .clone()
            .into_iter()
            .map(|n| n as u64)
            .collect();

        let transaction_position: Option<u64> = trace
            .transaction_position
            .map(|transaction_position| transaction_position as u64);

        let transaction_hash: Option<String> =
            trace.transaction_hash.map(format_hash);

        let action_type = match trace.action_type {
            ethers::types::ActionType::Call => String::from("call"),
            ethers::types::ActionType::Create => String::from("create"),
            ethers::types::ActionType::Suicide => String::from("suicide"),
            ethers::types::ActionType::Reward => String::from("reward"),
        };

        let mut from: Option<String> = None;
        let mut to: Option<String> = None;
        let mut gas: Option<U256> = None;
        let mut value: Option<U256> = None;
        let mut input: Option<String> = None;
        let mut call_type: Option<String> = None;
        let mut init: Option<String> = None;
        let mut address: Option<String> = None;
        let mut refund_address: Option<String> = None;
        let mut balance: Option<U256> = None;

        let mut author: Option<String> = None;
        let mut reward_type: Option<String> = None;

        match trace.action.clone() {
            ethers::types::Action::Call(call) => {
                from = Some(format_address(call.from));
                to = Some(format_address(call.to));
                value = Some(call.value);
                gas = Some(call.gas);
                input = Some(format_bytes(&call.input));
                call_type = match call.call_type {
                    ethers::types::CallType::None => {
                        Some(String::from("none"))
                    }
                    ethers::types::CallType::Call => {
                        Some(String::from("call"))
                    }
                    ethers::types::CallType::CallCode => {
                        Some(String::from("callcode"))
                    }
                    ethers::types::CallType::DelegateCall => {
                        Some(String::from("delegatecall"))
                    }
                    ethers::types::CallType::StaticCall => {
                        Some(String::from("delegatecall"))
                    }
                }
            }
            ethers::types::Action::Create(create) => {
                from = Some(format_address(create.from));
                value = Some(create.value);
                gas = Some(create.gas);
                init = Some(format_bytes(&create.init));
            }
            ethers::types::Action::Suicide(suicide) => {
                address = Some(format_address(suicide.address));
                refund_address =
                    Some(format_address(suicide.refund_address));
                balance = Some(suicide.balance)
            }
            ethers::types::Action::Reward(reward) => {
                author = Some(format_address(reward.author));
                value = Some(reward.value);
                reward_type = match reward.reward_type {
                    ethers::types::RewardType::Block => {
                        Some(String::from("block"))
                    }
                    ethers::types::RewardType::Uncle => {
                        Some(String::from("uncle"))
                    }

                    ethers::types::RewardType::EmptyStep => {
                        Some(String::from("emptyStep"))
                    }

                    ethers::types::RewardType::External => {
                        Some(String::from("external"))
                    }
                }
            }
        }

        let mut gas_used: Option<U256> = None;
        let mut output: Option<String> = None;
        let mut code: Option<String> = None;

        if trace.result.is_some() {
            let result = trace.result.clone().unwrap();
            match result {
                ethers::types::Res::Call(call) => {
                    gas_used = Some(call.gas_used);
                    output = Some(format_bytes(&call.output))
                }
                ethers::types::Res::Create(create) => {
                    address = Some(format_address(create.address));
                    gas_used = Some(create.gas_used);
                    code = Some(format_bytes(&create.code))
                }
                ethers::types::Res::None => (),
            }
        }

        Self {
            action_type,
            address,
            author,
            balance,
            block_hash: format_hash(trace.block_hash),
            block_number: trace.block_number,
            call_type,
            chain,
            code,
            error: trace.error.clone(),
            from,
            gas,
            gas_used,
            init,
            input,
            output,
            refund_address,
            reward_type,
            subtraces: trace.subtraces as u64,
            to,
            trace_address,
            transaction_hash,
            transaction_position,
            value,
        }
    }
}
