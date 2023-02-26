use crate::chains::chains::{get_chain, Chain};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "Indexer",
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
    pub db_url: String,
    pub redis_url: String,
    pub rabbitmq_url: String,
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
            db_url: std::env::var("DATABASE_URL").expect("DATABASE_URL must be set."),
            redis_url: std::env::var("REDIS_URL").expect("REDIS_URL must be set."),
            rabbitmq_url: std::env::var("RABBITMQ_URL").expect("RABBITMQ_URL must be set."),
            debug: args.debug,
            chain,
            batch_size: args.batch_size,
            rpcs,
        }
    }
}
