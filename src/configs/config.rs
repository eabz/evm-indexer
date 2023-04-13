use crate::chains::chains::{get_chain, Chain};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "EVM Indexer",
    about = "Scalable SQL indexer for EVM based blockchains."
)]
pub struct IndexerArgs {
    #[arg(long, help = "Start log with debug.", default_value_t = false)]
    pub debug: bool,

    #[arg(long, help = "Chain name to sync.", default_value_t = String::from("mainnet"))]
    pub chain: String,

    #[arg(long, help = "Block to start syncing.", default_value_t = 0)]
    pub start_block: i64,

    #[arg(
        long,
        help = "Amount of blocks to fetch at the same time.",
        default_value_t = 200
    )]
    pub batch_size: usize,

    #[arg(long, help = "Comma separated list of rpcs to use to fetch blocks.")]
    pub rpcs: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub start_block: i64,
    pub db_host: String,
    pub db_username: String,
    pub db_password: String,
    pub db_name: String,
    pub debug: bool,
    pub chain: Chain,
    pub batch_size: usize,
    pub rpcs: Vec<String>,
}

impl Config {
    pub fn new() -> Self {
        let args = IndexerArgs::parse();

        let mut chainname = args.chain;

        if chainname == "mainnet" {
            chainname = "ethereum".to_string();
        }

        let chain = get_chain(chainname.clone());

        let rpcs: Vec<String> = args.rpcs.split(",").map(|rpc| rpc.to_string()).collect();

        Self {
            start_block: args.start_block,
            db_host: std::env::var("DATABASE_HOST").expect("DATABASE_HOST must be set."),
            db_username: std::env::var("DATABASE_USERNAME")
                .expect("DATABASE_USERNAME must be set."),
            db_password: std::env::var("DATABASE_PASSWORD")
                .expect("DATABASE_PASSWORD must be set."),
            db_name: std::env::var("DATABASE_NAME").expect("DATABASE_NAME must be set."),
            debug: args.debug,
            chain,
            batch_size: args.batch_size,
            rpcs,
        }
    }
}
