use clap::{ArgAction, Args, Parser, Subcommand};
use std::ffi::OsString;

/// Boolean flags are driven from the environment by docker-compose, which
/// passes every variable even when blank. So `DEBUG=false`, `DEBUG=0` and
/// `DEBUG=` must all mean "off" instead of failing to parse.
fn parse_flag(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "f" | "no" | "n" | "off" => Ok(false),
        "1" | "true" | "t" | "yes" | "y" | "on" => Ok(true),
        other => Err(format!("invalid boolean value '{other}'")),
    }
}

// Top level command line. (Not a doc comment: clap would print it as the
// long help.)
//
// `indexer [RUN OPTIONS]` without a subcommand is `indexer run [RUN
// OPTIONS]`, see `with_default_subcommand`: command lines and compose
// files written before subcommands existed keep working unchanged.
#[derive(Parser, Debug)]
#[command(
    name = "indexer",
    version,
    about = "Scalable SQL indexer for EVM compatible blockchains.",
    after_help = "Without a subcommand `run` is assumed: `indexer --chain 1` \
                  is `indexer run --chain 1`. See `indexer run --help` for \
                  its options. Every option can also be set through the \
                  environment."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: CliCommand,
}

#[derive(Subcommand, Debug)]
pub enum CliCommand {
    /// Index a chain (default). Applies pending schema migrations first.
    Run(Box<IndexerArgs>),
    /// Apply pending schema migrations and exit.
    Migrate(MigrateArgs),
    /// Verify the indexed data of a chain (gaps, consistency) and exit.
    /// Read only. Exit code 0 = consistent, 1 = problems found.
    Verify(VerifyArgs),
    /// Re-decode a module's rows from the STORED logs (no re-sync), e.g.
    /// after a decoder fix or a new event family. Safe while `run` is live.
    Backfill(BackfillArgs),
}

#[derive(Args, Debug)]
pub struct MigrateArgs {
    #[arg(
        long,
        env = "DATABASE_URL",
        hide_env_values = true,
        help = "Clickhouse database url with username and password. The database is created when missing."
    )]
    pub database: String,

    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "List the pending migrations without applying (or creating) anything."
    )]
    pub dry_run: bool,

    #[arg(
        long,
        env = "DEBUG",
        action = ArgAction::SetTrue,
        value_parser = parse_flag,
        help = "Start log with debug."
    )]
    pub debug: bool,
}

#[derive(Args, Debug)]
pub struct VerifyArgs {
    #[arg(
        long,
        env = "CHAIN_ID",
        help = "Number identifying the chain id to verify.",
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
        env = "START_BLOCK",
        help = "First block to verify.",
        default_value_t = 0
    )]
    pub start_block: u64,

    #[arg(
        long,
        env = "END_BLOCK",
        help = "Block to stop verifying at (exclusive). 0 verifies up to the highest indexed block.",
        default_value_t = 0
    )]
    pub end_block: u64,

    #[arg(
        long,
        env = "DEBUG",
        action = ArgAction::SetTrue,
        value_parser = parse_flag,
        help = "Start log with debug."
    )]
    pub debug: bool,
}

#[derive(Args, Debug)]
pub struct BackfillArgs {
    #[arg(
        long,
        help = "Module to re-decode from the stored logs. Available: dex, predictions.",
        value_parser = ["dex", "predictions"]
    )]
    pub module: String,

    #[arg(
        long,
        env = "CHAIN_ID",
        help = "Number identifying the chain id to backfill.",
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

    // No env fallback on purpose: START_BLOCK / END_BLOCK of a compose
    // file describe the sync, not a one-off backfill.
    #[arg(long, help = "First block to re-decode.", default_value_t = 0)]
    pub from_block: u64,

    #[arg(
        long,
        help = "Block to stop at (exclusive). 0 = up to the highest indexed block.",
        default_value_t = 0
    )]
    pub to_block: u64,

    #[arg(
        long,
        help = "Blocks re-decoded per chunk.",
        default_value_t = 2_000
    )]
    pub chunk_blocks: u64,

    #[arg(
        long,
        env = "DEBUG",
        action = ArgAction::SetTrue,
        value_parser = parse_flag,
        help = "Start log with debug."
    )]
    pub debug: bool,
}

/// Options of `indexer run` (and of a bare `indexer`).
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
        help = "JSON-RPC endpoints for token and DEX pool metadata eth_calls (never on the commit path). Comma separated list with failover. Default (unset or blank) is `auto`: public endpoints for the chain id are discovered from https://chainid.network/chains.json (best effort). `none` disables RPC features. `https://mine,auto` = own endpoint first, public fallback (recommended for production)."
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
        env = "MAX_REORG_DEPTH",
        help = "Deepest chain reorganization that is rolled back automatically. A deeper one is a fatal error.",
        default_value_t = 512
    )]
    pub max_reorg_depth: u64,

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
        env = "NO_DEX",
        action = ArgAction::SetTrue,
        value_parser = parse_flag,
        help = "Do not decode DEX pools, swaps and liquidity events (DEX analytics are ON by default)."
    )]
    pub no_dex: bool,

    #[arg(
        long,
        env = "NO_PREDICTIONS",
        action = ArgAction::SetTrue,
        value_parser = parse_flag,
        help = "Do not decode prediction market events (prediction market analytics are ON by default)."
    )]
    pub no_predictions: bool,

    #[arg(
        long,
        env = "METRICS_ADDR",
        help = "ip:port to serve Prometheus metrics, /healthz and /readyz on. Off when unset."
    )]
    pub metrics_addr: Option<String>,

    #[arg(
        long,
        env = "NO_MIGRATE",
        action = ArgAction::SetTrue,
        value_parser = parse_flag,
        help = "Do not apply pending schema migrations at startup (run `indexer migrate` yourself)."
    )]
    pub no_migrate: bool,

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
    /// The `--rpc` argument as given. `None` (unset / blank) means `auto`,
    /// `none` disables RPC features: `tokens::build_caller` interprets it.
    pub rpc_url: Option<String>,
    pub redis_url: Option<String>,
    pub start_block: u64,
    /// Exclusive. 0 = follow the chain head.
    pub end_block: u64,
    /// Blocks to stay behind the chain head.
    pub confirmations: u64,
    /// A reorg deeper than this is fatal instead of rolled back.
    pub max_reorg_depth: u64,
    /// DEX decoding (on unless `--no-dex`).
    pub dex: bool,
    /// Prediction market decoding (on unless `--no-predictions`).
    pub predictions: bool,
    /// Where to serve metrics; `None` = off.
    pub metrics_addr: Option<std::net::SocketAddr>,
    pub new_blocks_only: bool,
    pub flush_rows: usize,
    pub flush_interval_ms: u64,
    /// Skip the schema migrations at startup.
    pub no_migrate: bool,
    pub debug: bool,
}

/// Settings of `indexer migrate`.
#[derive(Debug, Clone)]
pub struct MigrateConfig {
    pub database_url: String,
    /// Only list what is pending.
    pub dry_run: bool,
    pub debug: bool,
}

/// Settings of `indexer verify`.
#[derive(Debug, Clone)]
pub struct VerifyConfig {
    pub chain_id: u64,
    pub database_url: String,
    pub start_block: u64,
    /// Exclusive. 0 = up to the highest indexed block.
    pub end_block: u64,
    pub debug: bool,
}

/// Settings of `indexer backfill`.
#[derive(Debug, Clone)]
pub struct BackfillConfig {
    pub module: String,
    pub chain_id: u64,
    pub database_url: String,
    pub from_block: u64,
    /// Exclusive. 0 = up to the highest indexed block.
    pub to_block: u64,
    pub chunk_blocks: u64,
    pub debug: bool,
}

/// What the process was asked to do.
#[derive(Debug, Clone)]
pub enum Command {
    Run(Box<Config>),
    Migrate(MigrateConfig),
    Verify(VerifyConfig),
    Backfill(BackfillConfig),
}

/// docker-compose passes `VAR=` for blank entries: empty means unset.
fn non_empty(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// `--metrics-addr`: `ip:port`; a bare `:port` listens on every interface.
fn parse_metrics_addr(
    value: Option<String>,
) -> Result<Option<std::net::SocketAddr>, clap::Error> {
    let Some(value) = non_empty(value) else { return Ok(None) };

    let candidate = if value.starts_with(':') {
        format!("0.0.0.0{value}")
    } else {
        value.clone()
    };

    candidate.parse().map(Some).map_err(|e| {
        clap::Error::raw(
            clap::error::ErrorKind::ValueValidation,
            format!(
                "invalid value '{value}' for '--metrics-addr': {e} \
                 (expected ip:port, e.g. 0.0.0.0:9090)\n"
            ),
        )
    })
}

impl TryFrom<IndexerArgs> for Config {
    type Error = clap::Error;

    fn try_from(args: IndexerArgs) -> Result<Self, clap::Error> {
        Ok(Self {
            chain_id: args.chain,
            database_url: args.database,
            hypersync_url: non_empty(args.hypersync_url),
            hypersync_token: args.hypersync_token.trim().to_string(),
            rpc_url: non_empty(args.rpc),
            redis_url: non_empty(args.redis),
            start_block: args.start_block,
            end_block: args.end_block,
            confirmations: args.confirmations,
            max_reorg_depth: args.max_reorg_depth,
            dex: !args.no_dex,
            predictions: !args.no_predictions,
            metrics_addr: parse_metrics_addr(args.metrics_addr)?,
            new_blocks_only: args.new_blocks_only,
            flush_rows: args.flush_rows.max(1),
            flush_interval_ms: args.flush_interval_ms.max(1),
            no_migrate: args.no_migrate,
            debug: args.debug,
        })
    }
}

impl From<BackfillArgs> for BackfillConfig {
    fn from(args: BackfillArgs) -> Self {
        Self {
            module: args.module,
            chain_id: args.chain,
            database_url: args.database,
            from_block: args.from_block,
            to_block: args.to_block,
            chunk_blocks: args.chunk_blocks.max(1),
            debug: args.debug,
        }
    }
}

impl From<MigrateArgs> for MigrateConfig {
    fn from(args: MigrateArgs) -> Self {
        Self {
            database_url: args.database,
            dry_run: args.dry_run,
            debug: args.debug,
        }
    }
}

impl From<VerifyArgs> for VerifyConfig {
    fn from(args: VerifyArgs) -> Self {
        Self {
            chain_id: args.chain,
            database_url: args.database,
            start_block: args.start_block,
            end_block: args.end_block,
            debug: args.debug,
        }
    }
}

impl TryFrom<Cli> for Command {
    type Error = clap::Error;

    fn try_from(cli: Cli) -> Result<Self, clap::Error> {
        Ok(match cli.command {
            CliCommand::Run(args) => {
                Self::Run(Box::new((*args).try_into()?))
            }
            CliCommand::Migrate(args) => Self::Migrate(args.into()),
            CliCommand::Verify(args) => Self::Verify(args.into()),
            CliCommand::Backfill(args) => Self::Backfill(args.into()),
        })
    }
}

/// Environment variables read by the CLI.
const ENV_VARS: [&str; 18] = [
    "CHAIN_ID",
    "DATABASE_URL",
    "HYPERSYNC_URL",
    "ENVIO_API_TOKEN",
    "RPC_URL",
    "REDIS_URL",
    "START_BLOCK",
    "END_BLOCK",
    "CONFIRMATIONS",
    "MAX_REORG_DEPTH",
    "NO_DEX",
    "NO_PREDICTIONS",
    "METRICS_ADDR",
    "NEW_BLOCKS_ONLY",
    "FLUSH_ROWS",
    "FLUSH_INTERVAL_MS",
    "NO_MIGRATE",
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

/// Inserts the default `run` subcommand when the command line names none.
///
/// `run` takes no positional argument, so a command line that names no
/// subcommand is either empty or starts with an option. Top level `--help`
/// / `--version` are left alone.
pub fn with_default_subcommand(mut argv: Vec<OsString>) -> Vec<OsString> {
    let names_no_subcommand = match argv.get(1) {
        None => !argv.is_empty(),
        Some(first) => match first.to_str() {
            Some("-h" | "--help" | "-V" | "--version") => false,
            Some(first) => first.starts_with('-'),
            None => false,
        },
    };

    if names_no_subcommand {
        argv.insert(1, OsString::from("run"));
    }

    argv
}

impl Command {
    /// Parses the command line / environment. Prints usage and exits on
    /// invalid input (standard clap behaviour).
    ///
    /// Call before the tokio runtime is built, see [`scrub_blank_env`].
    pub fn parse() -> Self {
        scrub_blank_env();
        Self::try_parse_from(std::env::args_os())
            .unwrap_or_else(|e| e.exit())
    }

    /// Like [`parse`](Self::parse) for an explicit command line (the first
    /// item is the program name). Does not scrub the environment.
    pub fn try_parse_from<I, T>(argv: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        let argv = argv.into_iter().map(Into::into).collect();

        Cli::try_parse_from(with_default_subcommand(argv))
            .and_then(Self::try_from)
    }

    pub fn debug(&self) -> bool {
        match self {
            Self::Run(config) => config.debug,
            Self::Migrate(config) => config.debug,
            Self::Verify(config) => config.debug,
            Self::Backfill(config) => config.debug,
        }
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

    /// Through the real entry point, WITHOUT naming a subcommand: every
    /// pre-subcommand test below doubles as a "bare `indexer` is `indexer
    /// run`" test.
    fn parse(
        env: &[(&str, &str)],
        args: &[&str],
        scrub: bool,
    ) -> Result<Config, clap::Error> {
        match parse_command(env, args, scrub)? {
            Command::Run(config) => Ok(*config),
            other => panic!("expected the run command, got {other:?}"),
        }
    }

    fn parse_command(
        env: &[(&str, &str)],
        args: &[&str],
        scrub: bool,
    ) -> Result<Command, clap::Error> {
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

        let result = Command::try_parse_from(argv);

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
        assert!(!config.debug);
        assert_eq!(config.flush_rows, 100_000);
        assert_eq!(config.flush_interval_ms, 2_000);
        assert_eq!(config.hypersync_url, None);
        assert_eq!(config.rpc_url, None);
        assert_eq!(config.redis_url, None);
        // The owner's defaults: DEX on, RPC auto (= unset), reorgs rolled
        // back up to 512 blocks, metrics off.
        assert!(config.dex);
        assert!(config.predictions);
        assert_eq!(config.max_reorg_depth, 512);
        assert_eq!(config.metrics_addr, None);
    }

    #[test]
    fn dex_is_on_unless_opted_out() {
        let mut args = REQUIRED.to_vec();
        args.push("--no-dex");
        assert!(!parse_with_env(&[], &args).unwrap().dex);

        for (value, dex) in
            [("true", false), ("1", false), ("false", true), ("", true)]
        {
            let config =
                parse_with_env(&[("NO_DEX", value)], &REQUIRED).unwrap();
            assert_eq!(config.dex, dex, "NO_DEX={value}");
        }

        let mut args = REQUIRED.to_vec();
        args.push("--no-predictions");
        let config = parse_with_env(&[], &args).unwrap();
        assert!(config.dex && !config.predictions);
        assert!(
            !parse_with_env(&[("NO_PREDICTIONS", "true")], &REQUIRED)
                .unwrap()
                .predictions
        );

        // The old opt-in flag is gone: asking for it is an error, not a
        // silent no-op.
        let mut args = REQUIRED.to_vec();
        args.push("--dex");
        assert!(parse_with_env(&[], &args).is_err());
    }

    #[test]
    fn rpc_is_passed_through_for_build_caller_to_interpret() {
        for (value, expected) in [
            ("none", Some("none")),
            ("auto", Some("auto")),
            (
                "https://mine.example,auto",
                Some("https://mine.example,auto"),
            ),
            ("", None),
            ("   ", None),
        ] {
            let config =
                parse_with_env(&[("RPC_URL", value)], &REQUIRED).unwrap();
            assert_eq!(config.rpc_url.as_deref(), expected, "'{value}'");
        }
    }

    #[test]
    fn max_reorg_depth_and_metrics_addr() {
        let mut args = REQUIRED.to_vec();
        args.extend([
            "--max-reorg-depth",
            "64",
            "--metrics-addr",
            "127.0.0.1:9090",
        ]);
        let config = parse_with_env(&[], &args).unwrap();
        assert_eq!(config.max_reorg_depth, 64);
        assert_eq!(
            config.metrics_addr,
            Some("127.0.0.1:9090".parse().unwrap())
        );

        let config = parse(
            &[("MAX_REORG_DEPTH", ""), ("METRICS_ADDR", " ")],
            &REQUIRED,
            true,
        )
        .unwrap();
        assert_eq!(config.max_reorg_depth, 512);
        assert_eq!(config.metrics_addr, None);

        let config =
            parse_with_env(&[("METRICS_ADDR", ":9100")], &REQUIRED)
                .unwrap();
        assert_eq!(
            config.metrics_addr,
            Some("0.0.0.0:9100".parse().unwrap())
        );

        assert!(parse_with_env(
            &[("METRICS_ADDR", "nonsense")],
            &REQUIRED
        )
        .is_err());
    }

    #[test]
    fn backfill_subcommand() {
        let command = parse_command(
            &[("START_BLOCK", "77"), ("END_BLOCK", "99")],
            &[
                "backfill",
                "--module",
                "dex",
                "--database",
                DATABASE,
                "--chain",
                "10",
                "--from-block",
                "5",
                "--to-block",
                "50",
            ],
            false,
        )
        .unwrap();

        let Command::Backfill(config) = command else {
            panic!("{command:?}");
        };
        assert_eq!(config.module, "dex");
        assert_eq!(config.chain_id, 10);
        assert_eq!((config.from_block, config.to_block), (5, 50));
        assert_eq!(config.chunk_blocks, 2_000);

        // The sync's START_BLOCK / END_BLOCK never leak into a backfill.
        let command = parse_command(
            &[("START_BLOCK", "77"), ("DATABASE_URL", DATABASE)],
            &["backfill", "--module", "dex"],
            false,
        )
        .unwrap();
        let Command::Backfill(config) = command else {
            panic!("{command:?}");
        };
        assert_eq!((config.from_block, config.to_block), (0, 0));

        // The module is required and must exist.
        assert!(parse_command(
            &[("DATABASE_URL", DATABASE)],
            &["backfill"],
            false
        )
        .is_err());
        assert!(parse_command(
            &[("DATABASE_URL", DATABASE)],
            &["backfill", "--module", "launchpads"],
            false
        )
        .is_err());
    }

    #[test]
    fn every_env_variable_of_the_cli_is_scrubbed() {
        use clap::CommandFactory;

        let mut cli = Cli::command();
        cli.build();

        for subcommand in cli.get_subcommands() {
            for arg in subcommand.get_arguments() {
                if let Some(env) = arg.get_env() {
                    let env = env.to_str().unwrap();
                    assert!(
                        ENV_VARS.contains(&env),
                        "{env} (of `{}`) is missing in ENV_VARS",
                        subcommand.get_name()
                    );
                }
            }
        }
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
        assert!(config.debug);
        assert_eq!(config.flush_rows, 5000);
        assert_eq!(config.flush_interval_ms, 250);
    }

    #[test]
    fn false_and_blank_flags_from_env_mean_false() {
        for value in ["false", "False", "0", "no", "off", ""] {
            let config = parse_with_env(
                &[("DEBUG", value), ("NEW_BLOCKS_ONLY", value)],
                &REQUIRED,
            )
            .unwrap_or_else(|e| panic!("value '{value}' failed: {e}"));

            assert!(!config.debug, "DEBUG={value}");
            assert!(!config.new_blocks_only, "NEW_BLOCKS_ONLY={value}");
        }
    }

    #[test]
    fn traces_can_not_be_requested() {
        // Traces are out of scope (docs/design.md, section 9): the flag is
        // gone, asking for it is an error instead of a silent no-op.
        let mut args = REQUIRED.to_vec();
        args.push("--traces");
        assert!(parse_with_env(&[], &args).is_err());

        // A leftover TRACES variable in an old .env is simply ignored.
        assert!(parse_with_env(&[("TRACES", "true")], &REQUIRED).is_ok());
    }

    #[test]
    fn garbage_flag_value_is_rejected() {
        assert!(parse_with_env(&[("DEBUG", "maybe")], &REQUIRED).is_err());
    }

    #[test]
    fn command_line_flag_wins_over_false_env() {
        let mut args = REQUIRED.to_vec();
        args.push("--debug");

        let config = parse_with_env(&[("DEBUG", "false")], &args).unwrap();

        assert!(config.debug);
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

        let mut help =
            IndexerArgs::command().render_long_help().to_string();

        let mut cli = Cli::command();
        cli.build();
        help.push_str(&cli.render_long_help().to_string());
        for subcommand in cli.get_subcommands_mut() {
            help.push_str(&subcommand.render_long_help().to_string());
        }

        for (name, _) in secrets {
            std::env::remove_var(name);
        }

        assert!(help.contains("--confirmations"));
        assert!(help.contains("--dry-run"));
        assert!(help.contains("--no-migrate"));
        assert!(help.contains("--no-dex"));
        assert!(help.contains("--max-reorg-depth"));
        assert!(help.contains("--metrics-addr"));
        // The default RPC behaviour reaches out to a third party: say so.
        assert!(help.contains("chainid.network"));
        assert!(!help.to_lowercase().contains("trace"), "{help}");
        assert!(!help.contains("hunter2"), "{help}");
    }

    // ---- subcommands ----

    const DATABASE: &str = "http://default:pw@localhost:8123/indexer";

    const FULL_ENV: [(&str, &str); 3] = [
        ("DATABASE_URL", "http://u:p@ch:8123/from_env"),
        ("ENVIO_API_TOKEN", "00000000-0000-0000-0000-000000000000"),
        ("CHAIN_ID", "8453"),
    ];

    #[test]
    fn clap_definition_is_consistent() {
        use clap::CommandFactory;

        Cli::command().debug_assert();
        IndexerArgs::command().debug_assert();
    }

    #[test]
    fn default_subcommand_is_inserted_only_when_none_is_named() {
        let rewrite = |args: &[&str]| -> Vec<String> {
            with_default_subcommand(
                args.iter().map(OsString::from).collect(),
            )
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
        };

        assert_eq!(rewrite(&["indexer"]), ["indexer", "run"]);
        assert_eq!(
            rewrite(&["indexer", "--chain", "1"]),
            ["indexer", "run", "--chain", "1"]
        );
        assert_eq!(
            rewrite(&["indexer", "--new-blocks-only"]),
            ["indexer", "run", "--new-blocks-only"]
        );

        for untouched in [
            &["indexer", "run", "--new-blocks-only"][..],
            &["indexer", "migrate", "--dry-run"],
            &["indexer", "verify"],
            &["indexer", "help", "run"],
            &["indexer", "--help"],
            &["indexer", "-h"],
            &["indexer", "--version"],
            &["indexer", "-V"],
            // Not an option: left for clap to reject.
            &["indexer", "bogus"],
        ] {
            assert_eq!(rewrite(untouched), untouched);
        }
    }

    #[test]
    fn no_subcommand_and_explicit_run_are_the_same() {
        let mut bare = REQUIRED.to_vec();
        bare.extend([
            "--chain",
            "10",
            "--new-blocks-only",
            "--start-block",
            "7",
        ]);

        let mut explicit = vec!["run"];
        explicit.extend(&bare);

        let bare = parse(&[], &bare, false).unwrap();
        let explicit = parse(&[], &explicit, false).unwrap();

        assert_eq!(format!("{bare:?}"), format!("{explicit:?}"));
        assert_eq!(explicit.chain_id, 10);
        assert_eq!(explicit.start_block, 7);
        assert!(explicit.new_blocks_only);
        assert!(!explicit.no_migrate);
    }

    #[test]
    fn run_configured_by_environment_only() {
        for args in [&[][..], &["run"]] {
            let config = parse(&FULL_ENV, args, false).unwrap();

            assert_eq!(config.chain_id, 8453);
            assert_eq!(config.database_url, "http://u:p@ch:8123/from_env");
            assert!(!config.no_migrate);
        }
    }

    #[test]
    fn run_requires_its_arguments_with_and_without_subcommand() {
        assert!(parse_command(&[], &["run"], false).is_err());
        assert!(parse_command(&[], &[], false).is_err());
    }

    #[test]
    fn no_migrate_flag_and_env() {
        let mut args = REQUIRED.to_vec();
        args.push("--no-migrate");
        assert!(parse(&[], &args, false).unwrap().no_migrate);

        for (value, expected) in
            [("true", true), ("1", true), ("false", false), ("", false)]
        {
            let config =
                parse(&[("NO_MIGRATE", value)], &REQUIRED, false).unwrap();
            assert_eq!(config.no_migrate, expected, "NO_MIGRATE={value}");
        }

        assert!(
            parse(&FULL_ENV, &["run", "--no-migrate"], false)
                .unwrap()
                .no_migrate
        );
    }

    #[test]
    fn migrate_subcommand() {
        let command = parse_command(
            &[],
            &["migrate", "--database", DATABASE],
            false,
        )
        .unwrap();

        let Command::Migrate(config) = &command else {
            panic!("{command:?}");
        };
        assert_eq!(config.database_url, DATABASE);
        assert!(!config.dry_run);
        assert!(!config.debug);
        assert!(!command.debug());

        let command = parse_command(
            &[],
            &["migrate", "--dry-run", "--debug", "--database", DATABASE],
            false,
        )
        .unwrap();

        let Command::Migrate(config) = &command else {
            panic!("{command:?}");
        };
        assert!(config.dry_run);
        assert!(config.debug);
        assert!(command.debug());
    }

    #[test]
    fn migrate_needs_only_the_database_url() {
        // No HyperSync token, chain, ... : just the url.
        assert!(parse_command(&[], &["migrate"], false).is_err());

        let command = parse_command(
            &[("DATABASE_URL", DATABASE)],
            &["migrate"],
            false,
        )
        .unwrap();
        assert!(matches!(
            &command,
            Command::Migrate(c) if c.database_url == DATABASE && !c.dry_run
        ));

        // The compose environment (every run variable set, some blank)
        // does not get in the way of `indexer migrate`.
        let command = parse_command(
            &[
                ("DATABASE_URL", DATABASE),
                ("ENVIO_API_TOKEN", "token"),
                ("CHAIN_ID", "8453"),
                ("NEW_BLOCKS_ONLY", "true"),
                ("START_BLOCK", ""),
                ("RPC_URL", ""),
                ("DEBUG", ""),
            ],
            &["migrate", "--dry-run"],
            true,
        )
        .unwrap();
        assert!(matches!(&command, Command::Migrate(c) if c.dry_run));
    }

    #[test]
    fn migrate_rejects_run_options() {
        assert!(parse_command(
            &[],
            &["migrate", "--database", DATABASE, "--new-blocks-only"],
            false
        )
        .is_err());
    }

    #[test]
    fn verify_subcommand() {
        let command = parse_command(
            &[],
            &[
                "verify",
                "--database",
                DATABASE,
                "--chain",
                "10",
                "--start-block",
                "5",
                "--end-block",
                "50",
            ],
            false,
        )
        .unwrap();

        let Command::Verify(config) = command else {
            panic!("{command:?}");
        };
        assert_eq!(config.chain_id, 10);
        assert_eq!(config.database_url, DATABASE);
        assert_eq!(config.start_block, 5);
        assert_eq!(config.end_block, 50);
        assert!(!config.debug);
    }

    #[test]
    fn verify_configured_by_environment_only() {
        let command =
            parse_command(&FULL_ENV, &["verify"], false).unwrap();

        let Command::Verify(config) = command else {
            panic!("{command:?}");
        };
        assert_eq!(config.chain_id, 8453);
        assert_eq!(config.database_url, "http://u:p@ch:8123/from_env");
        assert_eq!(config.start_block, 0);
        assert_eq!(config.end_block, 0);

        assert!(parse_command(&[], &["verify"], false).is_err());
    }

    #[test]
    fn unknown_subcommand_and_stray_positionals_are_rejected() {
        assert!(parse_command(&FULL_ENV, &["bogus"], false).is_err());
        assert!(
            parse_command(&FULL_ENV, &["run", "bogus"], false).is_err()
        );
        assert!(parse_command(
            &FULL_ENV,
            &["--new-blocks-only", "migrate"],
            false
        )
        .is_err());
    }

    #[test]
    fn top_level_help_and_version_are_not_rewritten_to_run() {
        use clap::error::ErrorKind;

        let kind = |args: &[&str]| {
            parse_command(&[], args, false).unwrap_err().kind()
        };

        assert_eq!(kind(&["--help"]), ErrorKind::DisplayHelp);
        assert_eq!(kind(&["--version"]), ErrorKind::DisplayVersion);
        assert_eq!(kind(&["run", "--help"]), ErrorKind::DisplayHelp);
        assert_eq!(kind(&["migrate", "--help"]), ErrorKind::DisplayHelp);
    }
}
