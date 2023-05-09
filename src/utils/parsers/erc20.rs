use ethabi::{ethereum_types::H256, ParamType};

use crate::{
    db::models::log::{DatabaseLog, TokenTransferType},
    utils::format::{decode_bytes, format_address},
};

impl DatabaseLog {
    pub fn parse_erc20_transfer(&mut self) {
        let from_address_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(
                self.topic1.clone().unwrap(),
            )
            .unwrap();

        let to_address_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(
                self.topic2.clone().unwrap(),
            )
            .unwrap();

        let from_address_tokens = ethabi::decode(
            &[ParamType::Address],
            from_address_bytes.as_bytes(),
        )
        .unwrap();

        let from_address = from_address_tokens.first().unwrap();

        let to_address_tokens = ethabi::decode(
            &[ParamType::Address],
            to_address_bytes.as_bytes(),
        )
        .unwrap();

        let to_address = to_address_tokens.first().unwrap();

        let log_data = decode_bytes(self.data.clone());

        let value_tokens =
            ethabi::decode(&[ParamType::Uint(256)], &log_data[..])
                .unwrap();

        let value = value_tokens.first().unwrap();

        self.token_transfer_from = Some(format_address(
            from_address.to_owned().into_address().unwrap(),
        ));
        self.token_transfer_to = Some(format_address(
            to_address.to_owned().into_address().unwrap(),
        ));
        self.token_transfer_amount =
            Some(value.to_owned().into_uint().unwrap());
        self.token_transfer_token_address = Some(self.address.clone());
        self.token_transfer_type = Some(TokenTransferType::Erc20);
    }
}
