use clickhouse::Row;
use ethers::types::Trace;
use primitive_types::U256;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use serde_with::serde_as;

use crate::utils::format::{
    format_address, format_bytes, format_hash, SerU256,
};

#[derive(Debug, Clone, Serialize_repr, Deserialize_repr, PartialEq)]
#[repr(u8)]
pub enum TraceType {
    Call = 1,
    Create = 2,
    Suicide = 3,
    Reward = 4,
}

#[derive(Debug, Clone, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum CallType {
    None = 0,
    Call = 1,
    Callcode = 2,
    DelegateCall = 3,
    StaticCall = 4,
}

#[derive(Debug, Clone, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum RewardType {
    Block = 1,
    Uncle = 2,
    EmptyStep = 3,
    External = 4,
}

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseTrace {
    pub action_type: TraceType,
    pub address: Option<String>,
    pub author: Option<String>,
    #[serde_as(as = "Option<SerU256>")]
    pub balance: Option<U256>,
    pub block_hash: String,
    pub block_number: u32,
    pub call_type: Option<CallType>,
    pub chain: u64,
    pub code: Option<String>,
    pub error: Option<String>,
    pub from: Option<String>,
    pub gas: Option<u32>,
    pub gas_used: Option<u32>,
    pub init: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub refund_address: Option<String>,
    pub reward_type: Option<RewardType>,
    pub subtraces: u16,
    pub to: Option<String>,
    pub trace_address: Vec<u16>,
    pub transaction_hash: Option<String>,
    pub transaction_position: Option<u16>,
    #[serde_as(as = "Option<SerU256>")]
    pub value: Option<U256>,
}

impl DatabaseTrace {
    pub fn from_rpc(trace: &Trace, chain: u64) -> Self {
        let trace_address = trace
            .trace_address
            .clone()
            .into_iter()
            .map(|n| n as u16)
            .collect();

        let transaction_position: Option<u16> = trace
            .transaction_position
            .map(|transaction_position| transaction_position as u16);

        let action_type = match trace.action_type {
            ethers::types::ActionType::Call => TraceType::Call,
            ethers::types::ActionType::Create => TraceType::Create,
            ethers::types::ActionType::Suicide => TraceType::Suicide,
            ethers::types::ActionType::Reward => TraceType::Reward,
        };

        let transaction_hash: Option<String> =
            trace.transaction_hash.map(format_hash);

        let mut from: Option<String> = None;
        let mut to: Option<String> = None;
        let mut gas: Option<u32> = None;
        let mut value: Option<U256> = None;
        let mut input: Option<String> = None;
        let mut call_type: Option<CallType> = None;
        let mut init: Option<String> = None;
        let mut address: Option<String> = None;
        let mut refund_address: Option<String> = None;
        let mut balance: Option<U256> = None;

        let mut author: Option<String> = None;
        let mut reward_type: Option<RewardType> = None;

        match trace.action.clone() {
            ethers::types::Action::Call(call) => {
                from = Some(format_address(call.from));
                to = Some(format_address(call.to));
                value = Some(call.value);
                gas = Some(call.gas.as_usize() as u32);
                input = Some(format_bytes(&call.input));
                call_type = match call.call_type {
                    ethers::types::CallType::None => Some(CallType::None),
                    ethers::types::CallType::Call => Some(CallType::Call),
                    ethers::types::CallType::CallCode => {
                        Some(CallType::Callcode)
                    }
                    ethers::types::CallType::DelegateCall => {
                        Some(CallType::DelegateCall)
                    }
                    ethers::types::CallType::StaticCall => {
                        Some(CallType::StaticCall)
                    }
                }
            }
            ethers::types::Action::Create(create) => {
                from = Some(format_address(create.from));
                value = Some(create.value);
                gas = Some(create.gas.as_usize() as u32);
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
                        Some(RewardType::Block)
                    }
                    ethers::types::RewardType::Uncle => {
                        Some(RewardType::Uncle)
                    }

                    ethers::types::RewardType::EmptyStep => {
                        Some(RewardType::EmptyStep)
                    }

                    ethers::types::RewardType::External => {
                        Some(RewardType::External)
                    }
                }
            }
        }

        let mut gas_used: Option<u32> = None;
        let mut output: Option<String> = None;
        let mut code: Option<String> = None;

        if trace.result.is_some() {
            let result = trace.result.clone().unwrap();
            match result {
                ethers::types::Res::Call(call) => {
                    gas_used = Some(call.gas_used.as_usize() as u32);
                    output = Some(format_bytes(&call.output))
                }
                ethers::types::Res::Create(create) => {
                    address = Some(format_address(create.address));
                    gas_used = Some(create.gas_used.as_usize() as u32);
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
            block_number: trace.block_number as u32,
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
            subtraces: trace.subtraces as u16,
            to,
            trace_address,
            transaction_hash,
            transaction_position,
            value,
        }
    }
}
