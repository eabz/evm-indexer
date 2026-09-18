use clap::{ArgAction, Parser};

/// Boolean flags are driven from the environment by docker-compose, which
/// passes every variable even when blank. So `TRACES=false`, `TRACES=0` and
/// `TRACES=` must all mean "off" instead of failing to parse.
fn parse_flag(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "f" | "no" | "n" | "off" => Ok(false),
        "1" | "true" | "t" | "yes" | "y" | "on" => Ok(true),
        other => Err(format!("invalid boolean value '{other}'")),
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "EVM Indexer",
    about = "Scalable SQL indexer for EVM compatible blockchains."
)]
pub struct IndexerArgs {
    #[arg(
        long,
        env = "CHAIN_ID",
        help = "Number identifying the chain id to sync.",
        default_value_t = 1
    )]
    pub chain: u64,

    #[arg(
        long,
        env = "DATABASE_URL",
        hide_env_values = true,
        help = "Clickhouse database url with username and password."
    )]
    pub database: String,

    #[arg(
        long,
        env = "HYPERSYNC_URL",
        help = "HyperSync endpoint. Defaults to the public endpoint of the chain id."
    )]
    pub hypersync_url: Option<String>,

    #[arg(
        long,
        env = "ENVIO_API_TOKEN",
        hide_env_values = true,
        help = "HyperSync (Envio) API token."
    )]
    pub hypersync_token: String,

    #[arg(
        long,
        env = "RPC_URL",
        hide_env_values = true,
        help = "JSON-RPC endpoint, only used for token metadata eth_calls."
    )]
    pub rpc: Option<String>,

    #[arg(
        long,
        env = "REDIS_URL",
        hide_env_values = true,
        help = "Redis (or Dragonfly) url for the token metadata cache."
    )]
    pub redis: Option<String>,

    #[arg(
        long,
        env = "START_BLOCK",
        help = "Block to start syncing.",
        default_value_t = 0
    )]
    pub start_block: u64,

    #[arg(
        long,
        env = "END_BLOCK",
        help = "Block to stop syncing at (exclusive). 0 follows the chain head.",
        default_value_t = 0
    )]
    pub end_block: u64,

    #[arg(
        long,
        env = "CONFIRMATIONS",
        help = "Stay this many blocks behind the chain head (reorg safety). 0 indexes up to the head.",
        default_value_t = 0
    )]
    pub confirmations: u64,

    #[arg(
        long,
        env = "NEW_BLOCKS_ONLY",
        action = ArgAction::SetTrue,
        value_parser = parse_flag,
        help = "Start from the current chain height instead of --start-block."
    )]
    pub new_blocks_only: bool,

    #[arg(
        long,
        env = "TRACES",
        action = ArgAction::SetTrue,
        value_parser = parse_flag,
        help = "Index traces (and contracts created through traces)."
    )]
    pub traces: bool,

    #[arg(
        long,
        env = "FLUSH_ROWS",
        help = "Flush to the database once this many rows are buffered.",
        default_value_t = 100_000
    )]
    pub flush_rows: usize,

    #[arg(
        long,
        env = "FLUSH_INTERVAL_MS",
        help = "Flush to the database at least this often (milliseconds).",
        default_value_t = 2_000
    )]
    pub flush_interval_ms: u64,

    #[arg(
        long,
        env = "DEBUG",
        action = ArgAction::SetTrue,
        value_parser = parse_flag,
        help = "Start log with debug."
    )]
    pub debug: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub chain_id: u64,
    pub database_url: String,
    pub hypersync_url: Option<String>,
    pub hypersync_token: String,
    pub rpc_url: Option<String>,
    pub redis_url: Option<String>,
    pub start_block: u64,
    /// Exclusive. 0 = follow the chain head.
    pub end_block: u64,
    /// Blocks to stay behind the chain head.
    pub confirmations: u64,
    pub new_blocks_only: bool,
    pub traces: bool,
    pub flush_rows: usize,
    pub flush_interval_ms: u64,
    pub debug: bool,
}

/// docker-compose passes `VAR=` for blank entries: empty means unset.
fn non_empty(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

impl From<IndexerArgs> for Config {
    fn from(args: IndexerArgs) -> Self {
        Self {
            chain_id: args.chain,
            database_url: args.database,
            hypersync_url: non_empty(args.hypersync_url),
            hypersync_token: args.hypersync_token.trim().to_string(),
            rpc_url: non_empty(args.rpc),
            redis_url: non_empty(args.redis),
            start_block: args.start_block,
            end_block: args.end_block,
            confirmations: args.confirmations,
            new_blocks_only: args.new_blocks_only,
            traces: args.traces,
            flush_rows: args.flush_rows.max(1),
            flush_interval_ms: args.flush_interval_ms.max(1),
            debug: args.debug,
        }
    }
}

/// Environment variables read by the CLI.
const ENV_VARS: [&str; 14] = [
    "CHAIN_ID",
    "DATABASE_URL",
    "HYPERSYNC_URL",
    "ENVIO_API_TOKEN",
    "RPC_URL",
    "REDIS_URL",
    "START_BLOCK",
    "END_BLOCK",
    "CONFIRMATIONS",
    "NEW_BLOCKS_ONLY",
    "TRACES",
    "FLUSH_ROWS",
    "FLUSH_INTERVAL_MS",
    "DEBUG",
];

/// Removes blank (`VAR=`) entries so they behave exactly like an unset
/// variable: defaults apply to numbers, required values are reported as
/// missing instead of as unparsable.
///
/// Mutates the process environment: call it before any thread is spawned
/// (i.e. before the tokio runtime is built).
pub fn scrub_blank_env() {
    for name in ENV_VARS {
        if std::env::var(name).is_ok_and(|v| v.trim().is_empty()) {
            std::env::remove_var(name);
        }
    }
}

impl Config {
    /// Parses the command line / environment. Prints usage and exits on
    /// invalid input (standard clap behaviour).
    ///
    /// Call before the tokio runtime is built, see [`scrub_blank_env`].
    pub fn new() -> Self {
        scrub_blank_env();
        IndexerArgs::parse().into()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The environment is process global; tests touching it take this lock.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn parse_with_env(
        env: &[(&str, &str)],
        args: &[&str],
    ) -> Result<Config, clap::Error> {
        parse(env, args, false)
    }

    fn parse(
        env: &[(&str, &str)],
        args: &[&str],
        scrub: bool,
    ) -> Result<Config, clap::Error> {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        for name in ENV_VARS {
            std::env::remove_var(name);
        }
        for (name, value) in env {
            std::env::set_var(name, value);
        }

        let mut argv = vec!["indexer"];
        argv.extend_from_slice(args);

        if scrub {
            scrub_blank_env();
        }

        let result = IndexerArgs::try_parse_from(argv).map(Config::from);

        for name in ENV_VARS {
            std::env::remove_var(name);
        }

        result
    }

    const REQUIRED: [&str; 4] = [
        "--database",
        "http://default:pw@localhost:8123/indexer",
        "--hypersync-token",
        "00000000-0000-0000-0000-000000000000",
    ];

    #[test]
    fn defaults_match_the_cli_contract() {
        let config = parse_with_env(&[], &REQUIRED).unwrap();

        assert_eq!(config.chain_id, 1);
        assert_eq!(config.start_block, 0);
        assert_eq!(config.end_block, 0);
        assert_eq!(config.confirmations, 0);
        assert!(!config.new_blocks_only);
        assert!(!config.traces);
        assert!(!config.debug);
        assert_eq!(config.flush_rows, 100_000);
        assert_eq!(config.flush_interval_ms, 2_000);
        assert_eq!(config.hypersync_url, None);
        assert_eq!(config.rpc_url, None);
        assert_eq!(config.redis_url, None);
    }

    #[test]
    fn required_arguments_are_enforced() {
        assert!(parse_with_env(&[], &[]).is_err());
        assert!(parse_with_env(&[], &REQUIRED[..2]).is_err());
    }

    #[test]
    fn everything_can_come_from_the_environment() {
        let config = parse_with_env(
            &[
                ("CHAIN_ID", "8453"),
                ("DATABASE_URL", "http://u:p@ch:8123/indexer"),
                ("HYPERSYNC_URL", "https://base.hypersync.xyz"),
                (
                    "ENVIO_API_TOKEN",
                    "00000000-0000-0000-0000-000000000000",
                ),
                ("RPC_URL", "https://rpc.example"),
                ("REDIS_URL", "redis://cache:6379"),
                ("START_BLOCK", "100"),
                ("END_BLOCK", "200"),
                ("CONFIRMATIONS", "12"),
                ("NEW_BLOCKS_ONLY", "true"),
                ("TRACES", "1"),
                ("FLUSH_ROWS", "5000"),
                ("FLUSH_INTERVAL_MS", "250"),
                ("DEBUG", "yes"),
            ],
            &[],
        )
        .unwrap();

        assert_eq!(config.chain_id, 8453);
        assert_eq!(
            config.hypersync_url.as_deref(),
            Some("https://base.hypersync.xyz")
        );
        assert_eq!(config.rpc_url.as_deref(), Some("https://rpc.example"));
        assert_eq!(
            config.redis_url.as_deref(),
            Some("redis://cache:6379")
        );
        assert_eq!(config.start_block, 100);
        assert_eq!(config.end_block, 200);
        assert_eq!(config.confirmations, 12);
        assert!(config.new_blocks_only);
        assert!(config.traces);
        assert!(config.debug);
        assert_eq!(config.flush_rows, 5000);
        assert_eq!(config.flush_interval_ms, 250);
    }

    #[test]
    fn false_and_blank_flags_from_env_mean_false() {
        for value in ["false", "False", "0", "no", "off", ""] {
            let config = parse_with_env(
                &[
                    ("TRACES", value),
                    ("DEBUG", value),
                    ("NEW_BLOCKS_ONLY", value),
                ],
                &REQUIRED,
            )
            .unwrap_or_else(|e| panic!("value '{value}' failed: {e}"));

            assert!(!config.traces, "TRACES={value}");
            assert!(!config.debug, "DEBUG={value}");
            assert!(!config.new_blocks_only, "NEW_BLOCKS_ONLY={value}");
        }
    }

    #[test]
    fn garbage_flag_value_is_rejected() {
        assert!(parse_with_env(&[("TRACES", "maybe")], &REQUIRED).is_err());
    }

    #[test]
    fn command_line_flag_wins_over_false_env() {
        let mut args = REQUIRED.to_vec();
        args.push("--traces");

        let config =
            parse_with_env(&[("TRACES", "false")], &args).unwrap();

        assert!(config.traces);
    }

    #[test]
    fn empty_optional_env_values_are_unset() {
        let config = parse_with_env(
            &[("HYPERSYNC_URL", ""), ("RPC_URL", ""), ("REDIS_URL", "  ")],
            &REQUIRED,
        )
        .unwrap();

        assert_eq!(config.hypersync_url, None);
        assert_eq!(config.rpc_url, None);
        assert_eq!(config.redis_url, None);
    }

    #[test]
    fn blank_numeric_env_falls_back_to_defaults_after_scrub() {
        let config = parse(
            &[
                ("START_BLOCK", ""),
                ("END_BLOCK", " "),
                ("FLUSH_ROWS", ""),
                ("CONFIRMATIONS", ""),
                ("CHAIN_ID", ""),
            ],
            &REQUIRED,
            true,
        )
        .unwrap();

        assert_eq!(config.chain_id, 1);
        assert_eq!(config.start_block, 0);
        assert_eq!(config.end_block, 0);
        assert_eq!(config.flush_rows, 100_000);
    }

    #[test]
    fn help_never_prints_secret_env_values() {
        use clap::CommandFactory;

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let secrets = [
            ("DATABASE_URL", "http://u:hunter2-db@ch/indexer"),
            ("ENVIO_API_TOKEN", "hunter2-token"),
            ("RPC_URL", "https://rpc.example/hunter2-rpc-key"),
            ("REDIS_URL", "redis://:hunter2-redis@cache:6379"),
        ];

        for (name, value) in secrets {
            std::env::set_var(name, value);
        }

        let help = IndexerArgs::command().render_long_help().to_string();

        for (name, _) in secrets {
            std::env::remove_var(name);
        }

        assert!(help.contains("--confirmations"));
        assert!(!help.contains("hunter2"), "{help}");
    }
}
