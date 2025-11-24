use clap::Parser;
use url::Url;

#[derive(Parser, Debug)]
#[command(
    name = "EVM Indexer",
    about = "Scalable SQL indexer for EVM compatible blockchains."
)]
pub struct IndexerArgs {
    #[arg(
        long,
        help = " Amount of blocks to fetch in parallel.",
        default_value_t = 200
    )]
    pub batch_size: usize,
    #[arg(
        long,
        help = "Number identifying the chain id to sync.",
        default_value_t = 1
    )]
    pub chain: usize,
    #[arg(
        long,
        help = "Clickhouse database string with username and password."
    )]
    pub database: String,
    #[arg(long, help = "Start log with debug.", default_value_t = false)]
    pub debug: bool,
    #[arg(long, help = "Last block to sync.", default_value_t = 0)]
    pub end_block: i64,
    #[arg(
        long,
        help = "Boolean to listen to new blocks only.",
        default_value_t = false
    )]
    pub new_blocks_only: bool,
    #[arg(
        long,
        help = "Comma separated list of rpcs to use to fetch blocks."
    )]
    pub rpcs: String,
    #[arg(long, help = "Block to start syncing.", default_value_t = 0)]
    pub start_block: u32,
    #[arg(
        long,
        help = "Url of the websocket endpoint to fetch new blocks.",
        default_value_t = String::from("")
    )]
    pub ws: String,
    #[arg(
        long,
        help = "Fetch blockchain traces.",
        default_value_t = true
    )]
    pub traces: bool,
    #[arg(long, help = "Fetch uncle blocks.", default_value_t = false)]
    pub fetch_uncles: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub batch_size: usize,
    pub chain_id: u64,
    pub db_host: String,
    pub db_name: String,
    pub db_password: String,
    pub db_username: String,
    pub debug: bool,
    pub end_block: i64,
    pub new_blocks_only: bool,
    pub rpcs: Vec<String>,
    pub start_block: u32,
    pub ws_url: Option<String>,
    pub traces: bool,
    pub fetch_uncles: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

impl Config {
    pub fn new() -> Self {
        let args = IndexerArgs::parse();

        let rpcs: Vec<String> =
            args.rpcs.split(',').map(|rpc| rpc.to_string()).collect();

        let ws_url: Option<String> =
            if args.ws.is_empty() { None } else { Some(args.ws) };

        let url = Url::parse(&args.database).expect("unable to parse database url expected: scheme://username:password@host/database");

        let username = url.username();

        let password =
            url.password().expect("no password provided for database");

        let db_host =
            url.host().expect("no host provided for database").to_string();

        let url_paths =
            url.path_segments().map(|c| c.collect::<Vec<_>>()).unwrap();

        let db_name =
            url_paths.first().expect("no database name provided on path");

        Self {
            batch_size: args.batch_size,
            chain_id: args.chain as u64,
            db_host: format!("{}://{}", url.scheme(), db_host),
            db_name: db_name.to_string(),
            db_password: password.to_string(),
            db_username: username.to_string(),
            debug: args.debug,
            end_block: args.end_block,
            new_blocks_only: args.new_blocks_only,
            rpcs,
            start_block: args.start_block,
            ws_url,
            traces: args.traces,
            fetch_uncles: args.fetch_uncles,
        }
    }
}
