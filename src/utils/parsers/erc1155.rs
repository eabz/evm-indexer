use ethabi::{
    ethereum_types::{H256, U256},
    ParamType,
};

use crate::{
    db::models::log::{DatabaseLog, TokenTransferType},
    utils::format::format_address,
};

impl DatabaseLog {
    pub fn parse_single_erc1155_transfer(
        &mut self,
        id: U256,
        amount: U256,
    ) {
        let operator_bytes = array_bytes::hex_n_into::<String, H256, 32>(
            self.topic1.clone().unwrap(),
        )
        .unwrap();

        let from_address_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(
                self.topic2.clone().unwrap(),
            )
            .unwrap();

        let to_address_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(
                self.topic3.clone().unwrap(),
            )
            .unwrap();

        let operator_tokens = ethabi::decode(
            &[ParamType::Address],
            operator_bytes.as_bytes(),
        )
        .unwrap();

        let operator = operator_tokens.first().unwrap();

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

        self.token_transfer_from = Some(format_address(
            from_address.to_owned().into_address().unwrap(),
        ));
        self.token_transfer_to = Some(format_address(
            to_address.to_owned().into_address().unwrap(),
        ));
        self.token_transfer_id = Some(id);
        self.token_transfer_operator = Some(format_address(
            operator.to_owned().into_address().unwrap(),
        ));
        self.token_transfer_amount = Some(amount);
        self.token_transfer_token_address = Some(self.address.clone());
        self.token_transfer_type = Some(TokenTransferType::Erc1155);
    }

    pub fn parse_batch_erc1155_transfer(
        &mut self,
        ids: Vec<U256>,
        amounts: Vec<U256>,
    ) {
        let operator_bytes = array_bytes::hex_n_into::<String, H256, 32>(
            self.topic1.clone().unwrap(),
        )
        .unwrap();

        let from_address_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(
                self.topic2.clone().unwrap(),
            )
            .unwrap();

        let to_address_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(
                self.topic3.clone().unwrap(),
            )
            .unwrap();

        let operator_tokens = ethabi::decode(
            &[ParamType::Address],
            operator_bytes.as_bytes(),
        )
        .unwrap();

        let operator = operator_tokens.first().unwrap();

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

        self.token_transfer_from = Some(format_address(
            from_address.to_owned().into_address().unwrap(),
        ));
        self.token_transfer_to = Some(format_address(
            to_address.to_owned().into_address().unwrap(),
        ));
        self.token_transfer_ids = ids;
        self.token_transfer_operator = Some(format_address(
            operator.to_owned().into_address().unwrap(),
        ));
        self.token_transfer_amounts = amounts;
        self.token_transfer_token_address = Some(self.address.clone());
        self.token_transfer_type = Some(TokenTransferType::Erc1155);
    }
}
