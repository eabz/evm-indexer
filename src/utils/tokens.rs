use std::collections::{HashMap, HashSet};

use crate::{
    db::{models::token::DatabaseToken, Database},
    rpc::Rpc,
};

async fn get_tokens_metadata(
    db: &Database,
    rpc: &Rpc,
    tokens: &HashSet<String>,
) -> Vec<DatabaseToken> {
    let mut db_tokens = db.get_tokens(tokens).await;

    let db_token_address: Vec<String> =
        db_tokens.iter().map(|token| token.token.clone()).collect();

    let missing_tokens: Vec<&String> = tokens
        .iter()
        .filter(|token| !db_token_address.contains(token))
        .collect();

    let mut missing_tokens_metadata: Vec<DatabaseToken> = Vec::new();

    for missing_token in missing_tokens.iter() {
        let data = rpc
            .get_token_metadata(missing_token.to_string())
            .await
            .unwrap();

        missing_tokens_metadata.push(data);
    }

    if !missing_tokens_metadata.is_empty() {
        db.store_token_details(&missing_tokens_metadata).await.unwrap();
    }

    db_tokens.append(&mut missing_tokens_metadata);

    db_tokens
}

pub async fn get_tokens(
    db: &Database,
    rpc: &Rpc,
    tokens: &HashSet<String>,
) -> HashMap<String, DatabaseToken> {
    let db_tokens = get_tokens_metadata(db, rpc, tokens).await;

    let mut tokens_data: HashMap<String, DatabaseToken> = HashMap::new();

    for token in db_tokens.iter() {
        tokens_data.insert(token.token.clone(), token.to_owned());
    }

    if tokens_data.len() != tokens.len() {
        panic!("inconsistent amount of tokens to parse the logs")
    }

    let mut underlying_tokens: HashSet<String> = HashSet::new();

    for token in db_tokens.iter() {
        if token.token0.is_some() {
            underlying_tokens.insert(token.token0.clone().unwrap());
        }

        if token.token1.is_some() {
            underlying_tokens.insert(token.token1.clone().unwrap());
        }
    }

    let db_underlying_tokens =
        get_tokens_metadata(db, rpc, &underlying_tokens).await;

    for token in db_underlying_tokens.iter() {
        tokens_data.insert(token.token.clone(), token.to_owned());
    }

    tokens_data
}
