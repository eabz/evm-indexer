use crate::utils::format::SerU256;
use alloy::primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_trace::parity::{
    Action, CallType as AlloyCallType, LocalizedTransactionTrace as Trace,
    RewardType as AlloyRewardType, TraceOutput as Res,
};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use serde_with::serde_as;

#[derive(Debug, Clone, Serialize_repr, Deserialize_repr, PartialEq)]
#[repr(u8)]
pub enum ActionType {
    Call = 1,
    Create = 2,
    Suicide = 3,
    Reward = 4,
}

#[derive(Debug, Clone, Serialize_repr, Deserialize_repr, PartialEq)]
#[repr(u8)]
pub enum CallType {
    None = 0,
    Call = 1,
    CallCode = 2,
    DelegateCall = 3,
    StaticCall = 4,
}

#[derive(Debug, Clone, Serialize_repr, Deserialize_repr, PartialEq)]
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
    pub action_type: String,
    pub address: Option<Address>,
    pub author: Option<Address>,
    #[serde_as(as = "Option<SerU256>")]
    pub balance: Option<U256>,
    pub block_hash: B256,
    pub block_number: u32,
    pub call_type: Option<String>,
    pub chain: u64,
    pub code: Option<Bytes>,
    pub error: Option<String>,
    pub from: Option<Address>,
    pub gas: Option<u32>,
    pub gas_used: Option<u32>,
    pub init: Option<Bytes>,
    pub input: Option<Bytes>,
    pub output: Option<Bytes>,
    pub refund_address: Option<Address>,
    pub reward_type: Option<String>,
    pub subtraces: u16,
    pub to: Option<Address>,
    pub trace_address: Vec<u16>,
    pub transaction_hash: Option<B256>,
    pub transaction_position: Option<u16>,
    #[serde_as(as = "Option<SerU256>")]
    pub value: Option<U256>,
}

impl DatabaseTrace {
    pub fn from_rpc(trace: &Trace, chain: u64) -> Self {
        let mut call_type: Option<String> = None;
        let mut reward_type: Option<String> = None;
        let mut from: Option<Address> = None;
        let mut to: Option<Address> = None;
        let mut gas: Option<u32> = None;
        let mut input: Option<Bytes> = None;
        let mut value: Option<U256> = None;
        let mut init: Option<Bytes> = None;
        let mut address: Option<Address> = None;
        let mut refund_address: Option<Address> = None;
        let mut author: Option<Address> = None;
        let mut balance: Option<U256> = None;

        let action_type = match &trace.trace.action {
            Action::Call(call) => {
                from = Some(call.from);
                to = Some(call.to);
                gas = Some(call.gas.to::<u32>());
                input = Some(call.input.clone());
                value = Some(call.value);
                call_type = match call.call_type {
                    AlloyCallType::None => Some("none".to_string()),
                    AlloyCallType::Call => Some("call".to_string()),
                    AlloyCallType::CallCode => {
                        Some("callcode".to_string())
                    }
                    AlloyCallType::DelegateCall => {
                        Some("delegatecall".to_string())
                    }
                    AlloyCallType::StaticCall => {
                        Some("staticcall".to_string())
                    }
                    AlloyCallType::AuthCall => Some("none".to_string()),
                };
                "call".to_string()
            }
            Action::Create(create) => {
                from = Some(create.from);
                value = Some(create.value);
                gas = Some(create.gas.to::<u32>());
                init = Some(create.init.clone());
                "create".to_string()
            }
            Action::Selfdestruct(suicide) => {
                from = Some(suicide.address);
                refund_address = Some(suicide.refund_address);
                balance = Some(suicide.balance);
                "suicide".to_string()
            }
            Action::Reward(reward) => {
                author = Some(reward.author);
                value = Some(reward.value);
                reward_type = match reward.reward_type {
                    AlloyRewardType::Block => Some("block".to_string()),
                    AlloyRewardType::Uncle => Some("uncle".to_string()),
                };
                "reward".to_string()
            }
        };

        let mut gas_used: Option<u32> = None;
        let mut output: Option<Bytes> = None;
        let mut code: Option<Bytes> = None;
        let mut address_output: Option<Address> = None;

        if let Some(result) = &trace.trace.result {
            match result {
                Res::Call(call) => {
                    gas_used = Some(call.gas_used.to::<u32>());
                    output = Some(call.output.clone());
                }
                Res::Create(create) => {
                    gas_used = Some(create.gas_used.to::<u32>());
                    code = Some(create.code.clone());
                    address_output = Some(create.address);
                }
            }
        }

        if address.is_none() && address_output.is_some() {
            address = address_output;
        }

        Self {
            action_type,
            address,
            author,
            balance,
            block_hash: trace.block_hash.unwrap(),
            block_number: trace.block_number.unwrap() as u32,
            call_type,
            chain,
            code,
            error: trace.trace.error.clone(),
            from,
            gas,
            gas_used,
            init,
            input,
            output,
            refund_address,
            reward_type,
            subtraces: trace.trace.subtraces as u16,
            to,
            trace_address: trace
                .trace
                .trace_address
                .iter()
                .map(|v| *v as u16)
                .collect(),
            transaction_hash: trace.transaction_hash,
            transaction_position: trace
                .transaction_position
                .map(|v| v as u16),
            value,
        }
    }
}
