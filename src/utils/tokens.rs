use std::collections::{HashMap, HashSet};

use futures::future::join_all;

use crate::{
    db::{
        db::Database,
        models::{log::DatabaseLog, token_detail::DatabaseTokenDetails},
    },
    rpc::rpc::Rpc,
};

use super::events::{
    ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE, ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE,
    SWAPV3_EVENT_SIGNATURE, SWAP_EVENT_SIGNATURE, TRANSFER_EVENTS_SIGNATURE,
};

pub async fn get_tokens_metadata(
    db: &Database,
    rpc: &Rpc,
    logs: &Vec<DatabaseLog>,
) -> HashMap<String, DatabaseTokenDetails> {
    let mut tokens_metadata_required: HashSet<String> = HashSet::new();

    // filter only logs with topic
    let logs_scan: Vec<&DatabaseLog> = logs.iter().filter(|log| log.topics.len() > 0).collect();

    // get the logos that match a swap topic
    let logs_swaps = logs_scan.iter().filter(|log| {
        let topic_0 = log.topics.first().unwrap();
        topic_0 == SWAPV3_EVENT_SIGNATURE || topic_0 == SWAP_EVENT_SIGNATURE
    });

    // insert all the tokens from the logs to metadata check
    for log in logs_scan.iter() {
        let topic_0 = log.topics.first().unwrap();

        if topic_0 == TRANSFER_EVENTS_SIGNATURE
            || topic_0 == ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE
            || topic_0 == ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE
        {
            tokens_metadata_required.insert(log.address.clone());
        }
    }

    // insert at the end the pairs from swap events (pairs at the end on purpose to fetch first the underlying tokens data)
    for log in logs_swaps {
        tokens_metadata_required.insert(log.address.clone());
    }

    let mut db_tokens = db.get_tokens(&tokens_metadata_required).await;

    let db_token_address: Vec<String> = db_tokens.iter().map(|token| token.token.clone()).collect();

    let missing_tokens: Vec<&String> = tokens_metadata_required
        .iter()
        .filter(|token| !db_token_address.contains(&token))
        .collect();

    let mut tokens_data = vec![];

    for missing_token in missing_tokens.iter() {
        tokens_data.push(rpc.get_token_metadata(missing_token.to_string()))
    }

    let mut tokens_metadata: Vec<DatabaseTokenDetails> = join_all(tokens_data)
        .await
        .iter()
        .map(|token| token.clone().unwrap())
        .collect();

    if tokens_metadata.len() > 0 {
        db.store_token_details(&tokens_metadata).await.unwrap();
    }

    db_tokens.append(&mut tokens_metadata);

    let mut tokens_data: HashMap<String, DatabaseTokenDetails> = HashMap::new();

    for token in db_tokens.iter() {
        tokens_data.insert(token.token.clone(), token.to_owned());
    }

    if tokens_data.len() != tokens_metadata_required.len() {
        panic!("inconsistent amount of tokens to parse the logs")
    }

    return tokens_data;
}
