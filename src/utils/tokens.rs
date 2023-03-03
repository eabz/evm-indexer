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
    let mut pairs_metadata_required: HashSet<String> = HashSet::new();

    for log in logs.iter() {
        if log.topics.len() == 0 {
            continue;
        }

        let topic_0 = log.topics[0].clone();

        if topic_0 == TRANSFER_EVENTS_SIGNATURE
            || topic_0 == ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE
            || topic_0 == ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE
        {
            tokens_metadata_required.insert(log.address.clone());
        }

        if topic_0 == SWAP_EVENT_SIGNATURE || topic_0 == SWAPV3_EVENT_SIGNATURE {
            tokens_metadata_required.insert(log.address.clone());
            pairs_metadata_required.insert(log.address.clone());
        }
    }

    // Fetch pairs first since we are going to add their underlying tokens to the full token metadata required
    let db_pairs = db.get_tokens(&pairs_metadata_required).await;

    let db_pairs_address: Vec<String> = db_pairs.iter().map(|token| token.token.clone()).collect();

    let missing_pairs: Vec<&String> = pairs_metadata_required
        .iter()
        .filter(|token| !db_pairs_address.contains(&token))
        .collect();

    let mut pairs_data = vec![];

    for missing_pairs in missing_pairs.iter() {
        pairs_data.push(rpc.get_token_metadata(missing_pairs.to_string()))
    }

    let pairs_metadata: Vec<DatabaseTokenDetails> = join_all(pairs_data)
        .await
        .iter()
        .map(|token| token.clone().unwrap())
        .collect();

    if pairs_metadata.len() != missing_pairs.len() {
        panic!("inconsistent amount of pairs fetched")
    }

    if pairs_metadata.len() > 0 {
        db.store_token_details(&pairs_metadata).await.unwrap();
    }

    let mut pairs_refetch = vec![];

    for pair in pairs_metadata.iter() {
        let token0 = match pair.token0.clone() {
            Some(token0) => token0,
            None => {
                pairs_refetch.push(pair.token.clone());
                continue;
            }
        };

        let token1 = match pair.token0.clone() {
            Some(token1) => token1,
            None => {
                pairs_refetch.push(pair.token.clone());
                continue;
            }
        };

        tokens_metadata_required.insert(token0);
        tokens_metadata_required.insert(token1);
    }

    for pair in db_pairs.iter() {
        let token0 = match pair.token0.clone() {
            Some(token0) => token0,
            None => {
                pairs_refetch.push(pair.token.clone());
                continue;
            }
        };

        let token1 = match pair.token0.clone() {
            Some(token1) => token1,
            None => {
                pairs_refetch.push(pair.token.clone());
                continue;
            }
        };

        tokens_metadata_required.insert(token0);
        tokens_metadata_required.insert(token1);
    }

    let mut pairs_refetch_data = vec![];

    for missing_pairs in pairs_refetch.iter() {
        pairs_refetch_data.push(rpc.get_token_metadata(missing_pairs.to_string()))
    }

    let pairs_refetched_data: Vec<DatabaseTokenDetails> = join_all(pairs_refetch_data)
        .await
        .iter()
        .map(|token| token.clone().unwrap())
        .collect();

    if pairs_refetched_data.len() > 0 {
        db.store_token_details(&pairs_refetched_data).await.unwrap();
    }

    for pair in pairs_refetched_data.iter() {
        tokens_metadata_required.insert(pair.token0.clone().unwrap());
        tokens_metadata_required.insert(pair.token1.clone().unwrap());
    }

    let db_tokens = db.get_tokens(&tokens_metadata_required).await;

    let db_token_address: Vec<String> = db_tokens.iter().map(|token| token.token.clone()).collect();

    let missing_tokens: Vec<&String> = tokens_metadata_required
        .iter()
        .filter(|token| !db_token_address.contains(&token))
        .collect();

    let mut tokens_data = vec![];

    for missing_token in missing_tokens.iter() {
        tokens_data.push(rpc.get_token_metadata(missing_token.to_string()))
    }

    let tokens_metadata: Vec<DatabaseTokenDetails> = join_all(tokens_data)
        .await
        .iter()
        .map(|token| token.clone().unwrap())
        .collect();

    if tokens_metadata.len() > 0 {
        db.store_token_details(&tokens_metadata).await.unwrap();
    }

    let mut tokens_data: HashMap<String, DatabaseTokenDetails> = HashMap::new();

    for token in db_tokens.iter() {
        tokens_data.insert(token.token.clone(), token.to_owned());
    }

    for token in db_pairs.iter() {
        tokens_data.insert(token.token.clone(), token.to_owned());
    }

    for token in tokens_metadata.iter() {
        tokens_data.insert(token.token.clone(), token.to_owned());
    }

    if tokens_data.len() != tokens_metadata_required.len() {
        panic!("inconsistent amount of tokens to parse the logs")
    }

    return tokens_data;
}
