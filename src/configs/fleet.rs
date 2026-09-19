//! `indexer fleet`: one process, many chains (docs/design.md section 15).
//!
//! Two things live here and nowhere else:
//!
//! * [`FleetConfig`], the settings of the supervisor process itself;
//! * the **one** validator for the per-chain settings that reach the
//!   process from the control panel or from the `fleet_chains` table.
//!
//! The second one matters more than it looks. A setting typed into a web
//! page is untrusted input, and the temptation is to write a small parser
//! next to the HTTP handler that accepts "about the same" values as the
//! command line. Then the two drift, and the panel starts accepting a
//! configuration the CLI would refuse (or the other way round).
//!
//! So there is no second parser: [`apply_chain_settings`] turns the
//! settings into the command line `indexer run` would have been given and
//! hands it to **clap**, the same `IndexerArgs` definition with the same
//! `value_parser`s. Every number range, every boolean spelling and every
//! unknown option is judged by the code that judges the command line.

use super::{parse_flag, Config, IndexerArgs};
use clap::{CommandFactory, FromArgMatches};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, net::SocketAddr};

/// The panel's password. Environment only, never a flag: a flag is visible
/// in `ps`, in a shell history and in a container's inspect output.
pub const ADMIN_PASSWORD_ENV: &str = "ADMIN_PASSWORD";

/// Default address of the control panel: loopback, so a fleet started
/// without thinking about it is not on the network.
pub const DEFAULT_ADMIN_ADDR: &str = "127.0.0.1:8090";

/// What the owner wants a chain to be doing. Stored in `fleet_chains`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Desired {
    Running,
    Stopped,
}

impl Desired {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
        }
    }

    /// Anything that is not exactly `running` is read as `stopped`: a word
    /// nobody recognises must never start a chain by accident.
    pub fn parse(value: &str) -> Self {
        if value.trim().eq_ignore_ascii_case("running") {
            Self::Running
        } else {
            Self::Stopped
        }
    }
}

/// Per-chain overrides of the `run` defaults: long flag name (no dashes)
/// -> the value as it would be typed on a command line. An absent key means
/// "the `run` default applies", which is why adding a chain needs nothing
/// but its id.
pub type ChainSettings = BTreeMap<String, String>;

/// How a setting is written on the command line, which is also how the
/// panel has to render it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    /// `--start-block 100`
    Number,
    /// `--rpc https://...`
    Text,
    /// `--no-dex`, present or absent. The stored value is still a boolean
    /// spelling (`true` / `false`), read by the CLI's own [`parse_flag`].
    Flag,
}

/// One setting the control panel may change, with the plain-language label
/// the page shows. The `name` is the long flag of `indexer run` without its
/// dashes; a unit test proves every one of them really is one.
#[derive(Debug, Clone, Copy)]
pub struct ChainSetting {
    pub name: &'static str,
    pub kind: SettingKind,
    pub label: &'static str,
    pub help: &'static str,
    /// The value may contain an API key, so it is never sent to the
    /// browser as it is (`tokens::redact`).
    pub secret: bool,
}

/// Everything the panel may change about a chain, and nothing else.
///
/// Deliberately NOT the whole of `indexer run`: the database url, the
/// HyperSync token, the Redis url, `--metrics-addr`, `--no-migrate` and
/// `--debug` belong to the PROCESS, are the same for every chain and are
/// not editable from a web page.
pub const CHAIN_SETTINGS: &[ChainSetting] = &[
    ChainSetting {
        name: "start-block",
        kind: SettingKind::Number,
        label: "Start block",
        help:
            "First block (on Solana: first slot) to index. Takes effect \
               the next time the chain starts.",
        secret: false,
    },
    ChainSetting {
        name: "end-block",
        kind: SettingKind::Number,
        label: "Stop at block",
        help: "Stop when this block is reached (it is not indexed). 0 \
               follows the chain head for ever.",
        secret: false,
    },
    ChainSetting {
        name: "confirmations",
        kind: SettingKind::Number,
        label: "Stay behind the head",
        help: "How many blocks to stay behind the chain head, so a small \
               reorganization never touches stored data. 0 indexes up to \
               the head. Refused on Solana.",
        secret: false,
    },
    ChainSetting {
        name: "max-reorg-depth",
        kind: SettingKind::Number,
        label: "Deepest automatic rollback",
        help: "A chain reorganization deeper than this stops the chain \
               instead of being rolled back automatically.",
        secret: false,
    },
    ChainSetting {
        name: "new-blocks-only",
        kind: SettingKind::Flag,
        label: "Only new blocks",
        help: "Start at the current chain height instead of the start \
               block: no history is filled in.",
        secret: false,
    },
    ChainSetting {
        name: "flush-rows",
        kind: SettingKind::Number,
        label: "Rows per write",
        help: "Write to the database once this many rows are buffered.",
        secret: false,
    },
    ChainSetting {
        name: "flush-interval-ms",
        kind: SettingKind::Number,
        label: "Time between writes (ms)",
        help: "Write to the database at least this often, even when few \
               rows arrived.",
        secret: false,
    },
    ChainSetting {
        name: "no-dex",
        kind: SettingKind::Flag,
        label: "Skip DEX analytics",
        help: "Do not decode swaps, pools and liquidity. Refused on \
               Solana, where there is nothing else to index.",
        secret: false,
    },
    ChainSetting {
        name: "no-predictions",
        kind: SettingKind::Flag,
        label: "Skip prediction markets",
        help: "Do not decode prediction market events.",
        secret: false,
    },
    ChainSetting {
        name: "no-launchpads",
        kind: SettingKind::Flag,
        label: "Skip token launchpads",
        help: "Do not decode token launchpad events.",
        secret: false,
    },
    ChainSetting {
        name: "hypersync-url",
        kind: SettingKind::Text,
        label: "HyperSync endpoint",
        help: "Leave empty for the public endpoint of this chain id.",
        secret: true,
    },
    ChainSetting {
        name: "rpc",
        kind: SettingKind::Text,
        label: "RPC endpoints",
        help: "Comma separated JSON-RPC endpoints for token and pool \
               metadata. Empty or `auto` discovers public ones, `none` \
               switches the feature off.",
        secret: true,
    },
];

/// The setting names, in the order the panel shows them.
pub fn chain_setting_keys() -> Vec<&'static str> {
    CHAIN_SETTINGS.iter().map(|setting| setting.name).collect()
}

fn setting(name: &str) -> Option<&'static ChainSetting> {
    CHAIN_SETTINGS.iter().find(|setting| setting.name == name)
}

/// A setting the panel sent that the command line would not accept. The
/// message is safe to show a browser: it never repeats a secret value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingError {
    /// The setting at fault, when one can be named.
    pub key: Option<String>,
    pub message: String,
}

impl std::fmt::Display for SettingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.key {
            Some(key) => write!(f, "{key}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for SettingError {}

impl SettingError {
    fn new(key: Option<&str>, message: impl Into<String>) -> Self {
        Self { key: key.map(str::to_string), message: message.into() }
    }
}

/// Settings of the supervisor process. Everything here is the same for
/// every chain; what differs per chain is a [`ChainSettings`] map.
#[derive(Debug, Clone)]
pub struct FleetConfig {
    pub database_url: String,
    pub hypersync_token: String,
    /// Fleet-wide `--rpc` default; a chain may override it.
    pub rpc_url: Option<String>,
    pub redis_url: Option<String>,
    /// ONE `/metrics` for the whole fleet; every series carries `chain`.
    pub metrics_addr: Option<SocketAddr>,
    /// Where the control panel listens. It only serves anything when
    /// `ADMIN_PASSWORD` is set.
    pub admin_addr: SocketAddr,
    /// Required to bind the panel to anything but a loopback address.
    pub admin_allow_remote: bool,
    /// Mark the session cookie `Secure` (the panel itself never speaks
    /// TLS; this is for a TLS reverse proxy in front of it).
    pub admin_secure_cookie: bool,
    /// Trust `X-Forwarded-Proto: https` from the proxy to decide `Secure`.
    /// Off by default: a header a client can set must not be believed.
    pub admin_trust_forwarded_proto: bool,
    /// Chains to index even when `fleet_chains` does not list them yet
    /// (`--chain`, repeatable). A brand new database needs this once.
    pub chains: Vec<u64>,
    /// Upper bound on the rows every chain may hold before its writer
    /// flushes, expressed as memory over the WHOLE fleet.
    pub max_inflight_mb: u64,
    /// Metered Solana HyperSync queries a minute, shared by every Solana
    /// chain in the process (one token, one budget).
    pub solana_queries_per_minute: u32,
    pub no_migrate: bool,
    pub debug: bool,
}

impl FleetConfig {
    /// The configuration one chain starts with: the `run` defaults, the
    /// fleet-wide values, then the chain's own settings - each of them
    /// judged by the command line parser and nothing else.
    ///
    /// `--metrics-addr` is never passed on: the fleet serves one endpoint
    /// for every chain, so a per-chain metrics server would fight it for
    /// the port.
    pub fn chain_config(
        &self,
        chain: u64,
        settings: &ChainSettings,
    ) -> Result<Config, SettingError> {
        let chain = chain.to_string();

        let mut argv: Vec<String> = vec![
            "indexer".to_string(),
            "--chain".to_string(),
            chain,
            "--database".to_string(),
            self.database_url.clone(),
            "--hypersync-token".to_string(),
            self.hypersync_token.clone(),
        ];

        if let Some(rpc) = &self.rpc_url {
            argv.push("--rpc".to_string());
            argv.push(rpc.clone());
        }
        if let Some(redis) = &self.redis_url {
            argv.push("--redis".to_string());
            argv.push(redis.clone());
        }

        apply_chain_settings(&mut argv, settings)?;

        let args = parse_run_arguments(&argv)?;

        let mut config: Config =
            args.try_into().map_err(|e: clap::Error| {
                SettingError::new(None, clean(&e.to_string()))
            })?;

        // One endpoint for the whole fleet (src/fleet/metrics.rs).
        config.metrics_addr = None;
        // The supervisor migrates once, before any chain starts.
        config.no_migrate = true;
        config.debug = self.debug;

        Ok(config)
    }
}

/// The `indexer run` parser with every ENVIRONMENT fallback removed.
///
/// A fleet process is configured once, at start. A chain's options then
/// come from exactly two places - the fleet-wide values and that chain's
/// own settings - and from nowhere else. Left as it is, clap would fill
/// anything not named on this command line from the process environment, so
/// a stray `START_BLOCK=77` in a compose file would silently apply to every
/// chain in the fleet and to every chain added later in the panel. Resetting
/// the fallbacks makes `chain_config` a pure function of its arguments.
///
/// `indexer run` itself is untouched: it keeps every environment fallback
/// it has always had.
fn parse_run_arguments(
    argv: &[String],
) -> Result<IndexerArgs, SettingError> {
    let command = IndexerArgs::command()
        .mut_args(|arg| arg.env(None::<&str>))
        .no_binary_name(false);

    let matches = command
        .try_get_matches_from(argv)
        .map_err(|e| SettingError::new(None, clean(&e.to_string())))?;

    IndexerArgs::from_arg_matches(&matches)
        .map_err(|e| SettingError::new(None, clean(&e.to_string())))
}

/// Appends the settings to a command line as `indexer run` flags.
///
/// This is the whole validator: an unknown key is refused here, and every
/// VALUE is refused (or accepted) by clap a moment later, using the very
/// `value_parser` the command line uses. Nothing in this file decides what
/// a valid block number or a valid boolean is.
pub fn apply_chain_settings(
    argv: &mut Vec<String>,
    settings: &ChainSettings,
) -> Result<(), SettingError> {
    for (key, value) in settings {
        let Some(setting) = setting(key) else {
            return Err(SettingError::new(
                Some(key),
                format!(
                    "unknown setting. Known settings: {}.",
                    chain_setting_keys().join(", ")
                ),
            ));
        };

        match setting.kind {
            // A flag is present or absent on a command line, so the stored
            // word is read by the CLI's OWN boolean parser (the one that
            // makes `NO_DEX=` from a compose file mean "off") and the flag
            // is appended only when it says yes.
            SettingKind::Flag => {
                if parse_flag(value)
                    .map_err(|e| SettingError::new(Some(key), e))?
                {
                    argv.push(format!("--{key}"));
                }
            }
            SettingKind::Number | SettingKind::Text => {
                let value = value.trim();
                // An empty box in the panel means "use the default", which
                // is exactly what leaving the flag off does.
                if value.is_empty() {
                    continue;
                }
                if value.starts_with('-') {
                    return Err(SettingError::new(
                        Some(key),
                        "a value may not start with '-'".to_string(),
                    ));
                }
                argv.push(format!("--{key}"));
                argv.push(value.to_string());
            }
        }
    }

    Ok(())
}

/// A clap error, made safe and short enough for a web page: one line, and
/// no url that might carry an API key.
fn clean(message: &str) -> String {
    let line = message
        .lines()
        .find(|line| line.starts_with("error:"))
        .unwrap_or_else(|| message.lines().next().unwrap_or(message));

    crate::tokens::redact::redact_urls(
        line.trim_start_matches("error:").trim(),
    )
}

#[cfg(test)]
mod tests {
    use super::{super::parse_chain, *};
    use clap::{ArgAction, CommandFactory};

    fn base() -> FleetConfig {
        FleetConfig {
            database_url: "http://default:pw@localhost:8123/indexer"
                .to_string(),
            hypersync_token: "00000000-0000-0000-0000-000000000000"
                .to_string(),
            rpc_url: None,
            redis_url: None,
            metrics_addr: None,
            admin_addr: DEFAULT_ADMIN_ADDR.parse().unwrap(),
            admin_allow_remote: false,
            admin_secure_cookie: false,
            admin_trust_forwarded_proto: false,
            chains: Vec::new(),
            max_inflight_mb: 2_048,
            solana_queries_per_minute: 25,
            no_migrate: false,
            debug: false,
        }
    }

    fn settings(pairs: &[(&str, &str)]) -> ChainSettings {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// The claim this file makes: every setting name is a real long flag
    /// of `indexer run`, and its kind matches what clap does with it. If
    /// someone renames a flag, this fails instead of the panel silently
    /// losing a setting.
    #[test]
    fn every_setting_is_a_real_run_flag_of_the_matching_kind() {
        let command = IndexerArgs::command();

        for setting in CHAIN_SETTINGS {
            let arg = command
                .get_arguments()
                .find(|arg| arg.get_long() == Some(setting.name))
                .unwrap_or_else(|| {
                    panic!(
                        "--{} is not an option of `indexer run`",
                        setting.name
                    )
                });

            let is_flag = matches!(arg.get_action(), ArgAction::SetTrue);

            assert_eq!(
                is_flag,
                setting.kind == SettingKind::Flag,
                "--{}: clap action and SettingKind disagree",
                setting.name
            );
        }
    }

    /// The process-wide options must not be editable from a web page.
    #[test]
    fn process_wide_options_are_not_chain_settings() {
        for forbidden in [
            "database",
            "hypersync-token",
            "redis",
            "metrics-addr",
            "no-migrate",
            "debug",
            "chain",
        ] {
            assert!(
                setting(forbidden).is_none(),
                "--{forbidden} must not be a per-chain setting"
            );
        }
    }

    #[test]
    fn an_empty_settings_map_is_the_run_defaults() {
        let config = base().chain_config(8453, &ChainSettings::new());
        let config = config.unwrap();

        assert_eq!(config.chain_id, 8453);
        assert_eq!(config.start_block, 0);
        assert_eq!(config.confirmations, 0);
        assert_eq!(config.max_reorg_depth, 512);
        assert!(config.dex && config.predictions && config.launchpads);
        // The fleet owns both of these, never a chain.
        assert_eq!(config.metrics_addr, None);
        assert!(config.no_migrate);
    }

    #[test]
    fn settings_are_applied_through_the_command_line_parser() {
        let config = base()
            .chain_config(
                1,
                &settings(&[
                    ("start-block", "18000000"),
                    ("confirmations", "12"),
                    ("no-predictions", "true"),
                    ("no-dex", "false"),
                    ("rpc", "https://mine.example,auto"),
                ]),
            )
            .unwrap();

        assert_eq!(config.start_block, 18_000_000);
        assert_eq!(config.confirmations, 12);
        assert!(!config.predictions);
        // `false` means the flag is simply not passed.
        assert!(config.dex);
        assert_eq!(
            config.rpc_url.as_deref(),
            Some("https://mine.example,auto")
        );
    }

    #[test]
    fn a_blank_value_means_the_default() {
        let config = base()
            .chain_config(
                1,
                &settings(&[("start-block", "  "), ("rpc", "")]),
            )
            .unwrap();

        assert_eq!(config.start_block, 0);
        assert_eq!(config.rpc_url, None);
    }

    #[test]
    fn a_bad_value_is_refused_by_the_same_parser_the_cli_uses() {
        let refused = |pairs: &[(&str, &str)]| {
            base()
                .chain_config(1, &settings(pairs))
                .expect_err("should have been refused")
        };

        // Not a number.
        assert!(refused(&[("start-block", "soon")])
            .message
            .to_lowercase()
            .contains("start-block"));
        // Not a boolean spelling the CLI knows.
        let error = refused(&[("no-dex", "maybe")]);
        assert_eq!(error.key.as_deref(), Some("no-dex"));
        // Not a setting at all.
        let error = refused(&[("delete-everything", "yes")]);
        assert_eq!(error.key.as_deref(), Some("delete-everything"));
        assert!(error.message.contains("unknown setting"));
        // A value that would smuggle in another flag.
        let error = refused(&[("rpc", "--database")]);
        assert_eq!(error.key.as_deref(), Some("rpc"));
    }

    /// A refusal is shown in a browser: it must not echo an API key back.
    #[test]
    fn a_refusal_never_repeats_a_url_that_could_hold_a_key() {
        let error = base()
            .chain_config(
                1,
                &settings(&[
                    ("hypersync-url", "https://x.example/hunter2secret"),
                    ("start-block", "nope"),
                ]),
            )
            .unwrap_err();

        assert!(!error.message.contains("hunter2secret"), "{error}");
    }

    /// A fleet is configured once, at start. A `START_BLOCK` left in a
    /// compose file must not silently apply to every chain the panel adds
    /// later - which is what clap's environment fallbacks would do.
    #[test]
    fn the_process_environment_never_decides_a_chains_options() {
        let _guard = super::super::tests::env_lock();

        for (name, value) in [
            ("START_BLOCK", "77"),
            ("CONFIRMATIONS", "99"),
            ("NO_DEX", "true"),
            ("CHAIN_ID", "8453"),
            ("RPC_URL", "https://leaked.example"),
        ] {
            std::env::set_var(name, value);
        }

        let config =
            base().chain_config(1, &ChainSettings::new()).unwrap();

        for name in [
            "START_BLOCK",
            "CONFIRMATIONS",
            "NO_DEX",
            "CHAIN_ID",
            "RPC_URL",
        ] {
            std::env::remove_var(name);
        }

        assert_eq!(config.chain_id, 1);
        assert_eq!(config.start_block, 0);
        assert_eq!(config.confirmations, 0);
        assert!(config.dex);
        assert_eq!(config.rpc_url, None);
    }

    #[test]
    fn desired_defaults_to_stopped_for_anything_unknown() {
        assert_eq!(Desired::parse("running"), Desired::Running);
        assert_eq!(Desired::parse("RUNNING"), Desired::Running);
        assert_eq!(Desired::parse("stopped"), Desired::Stopped);
        assert_eq!(Desired::parse(""), Desired::Stopped);
        assert_eq!(Desired::parse("maybe"), Desired::Stopped);
        assert_eq!(Desired::Running.as_str(), "running");
    }

    #[test]
    fn solana_is_accepted_by_name_like_everywhere_else() {
        assert_eq!(parse_chain("solana").unwrap(), 1_399_811_149);
        assert_eq!(parse_chain("8453").unwrap(), 8453);
        assert!(parse_chain("mainnet").is_err());
    }
}
