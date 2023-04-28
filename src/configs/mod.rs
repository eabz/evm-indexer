use crate::chains::{get_chain, Chain};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "EVM Indexer",
    about = "Scalable SQL indexer for EVM compatible blockchains."
)]
pub struct IndexerArgs {
    #[arg(long, help = "Start log with debug.", default_value_t = false)]
    pub debug: bool,

    #[arg(
        long,
        help = "Number identifying the chain id to sync.",
        default_value_t = 1
    )]
    pub chain: usize,

    #[arg(long, help = "Block to start syncing.", default_value_t = 0)]
    pub start_block: u64,

    #[arg(
        long,
        help = " Amount of blocks to fetch in parallel.",
        default_value_t = 200
    )]
    pub batch_size: usize,

    #[arg(
        long,
        help = "Comma separated list of rpcs to use to fetch blocks."
    )]
    pub rpcs: String,

    #[arg(
        long,
        help = "Url of the websocket endpoint to fetch new blocks.",
        default_value_t = String::from("")
    )]
    pub ws: String,

    #[arg(
        long,
        help = "Clickhouse database string with username and password."
    )]
    pub database: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub start_block: u64,
    pub db_host: String,
    pub db_username: String,
    pub db_password: String,
    pub db_name: String,
    pub debug: bool,
    pub chain: Chain,
    pub batch_size: usize,
    pub rpcs: Vec<String>,
    pub ws_url: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

impl Config {
    pub fn new() -> Self {
        let args = IndexerArgs::parse();

        let chain = get_chain(args.chain as u64);

        let rpcs: Vec<String> =
            args.rpcs.split(',').map(|rpc| rpc.to_string()).collect();

        let ws_url: Option<String> =
            if args.ws.is_empty() { None } else { Some(args.ws) };

        Self {
            start_block: args.start_block,
            db_host: "".to_string(),
            db_username: "".to_string(),
            db_password: "".to_string(),
            db_name: "".to_string(),
            debug: args.debug,
            chain,
            batch_size: args.batch_size,
            rpcs,
            ws_url,
        }
    }
}
