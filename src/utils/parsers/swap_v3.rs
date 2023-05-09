use ethabi::{ethereum_types::H256, ParamType};

use crate::{
    db::models::log::DatabaseLog,
    utils::format::{decode_bytes, format_address},
};

impl DatabaseLog {
    pub fn parse_swap_v3(&mut self) {
        let maker_bytes = array_bytes::hex_n_into::<String, H256, 32>(
            self.topic1.clone().unwrap(),
        )
        .unwrap();

        let receiver_bytes = array_bytes::hex_n_into::<String, H256, 32>(
            self.topic2.clone().unwrap(),
        )
        .unwrap();

        let maker_tokens =
            ethabi::decode(&[ParamType::Address], maker_bytes.as_bytes())
                .unwrap();

        let maker = maker_tokens.first().unwrap();

        let receiver_tokens = ethabi::decode(
            &[ParamType::Address],
            receiver_bytes.as_bytes(),
        )
        .unwrap();

        let receiver = receiver_tokens.first().unwrap();

        let log_data = decode_bytes(self.data.clone());

        let values_tokens = ethabi::decode(
            &[
                ParamType::Int(256),
                ParamType::Int(256),
                ParamType::Uint(160),
                ParamType::Uint(128),
                ParamType::Int(24),
            ],
            &log_data[..],
        )
        .unwrap();

        let token0_amount =
            values_tokens[0].to_owned().into_int().unwrap();

        let token1_amount =
            values_tokens[1].to_owned().into_int().unwrap();

        self.dex_trade_maker =
            Some(format_address(maker.to_owned().into_address().unwrap()));
        self.dex_trade_pair = Some(self.address.clone());
        self.dex_trade_receiver = Some(format_address(
            receiver.to_owned().into_address().unwrap(),
        ));
        self.dex_trade_token0_amount = Some(token0_amount);
        self.dex_trade_token1_amount = Some(token1_amount);
    }
}
