use super::{
    encode::{escape_help, escape_label_value, Encoder, Kind},
    *,
};
use std::net::SocketAddr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::oneshot,
    task::JoinHandle,
};

const START: u64 = 1_700_000_000;

fn fixed(chain: &str) -> Metrics {
    Metrics::with_start_time(
        chain.to_string(),
        Duration::from_secs(120),
        START,
    )
}

/// Value of the series that is written exactly as `series`.
fn value(text: &str, series: &str) -> Option<f64> {
    text.lines()
        .find_map(|line| line.strip_prefix(series)?.strip_prefix(' '))
        .map(|v| v.parse().unwrap())
}

// ---------------------------------------------------------------------
// Exposition
// ---------------------------------------------------------------------

#[test]
fn golden_fresh_state() {
    let metrics = fixed("1");
    metrics.build_info("3.0.0", "abc1234");

    let expected = r#"# HELP evm_indexer_build_info Build information; the value is always 1.
# TYPE evm_indexer_build_info gauge
evm_indexer_build_info{chain="1",version="3.0.0",commit="abc1234"} 1
# HELP evm_indexer_start_time_seconds Unix time the process started.
# TYPE evm_indexer_start_time_seconds gauge
evm_indexer_start_time_seconds{chain="1"} 1700000000
# HELP evm_indexer_ready 1 when /readyz answers 200, else 0.
# TYPE evm_indexer_ready gauge
evm_indexer_ready{chain="1"} 0
# HELP evm_indexer_flushes_total Flushes (one multi-table batch each), by result.
# TYPE evm_indexer_flushes_total counter
evm_indexer_flushes_total{chain="1",result="ok"} 0
evm_indexer_flushes_total{chain="1",result="error"} 0
# HELP evm_indexer_flush_duration_seconds Duration of a flush, retries included.
# TYPE evm_indexer_flush_duration_seconds histogram
evm_indexer_flush_duration_seconds_bucket{chain="1",le="0.01"} 0
evm_indexer_flush_duration_seconds_bucket{chain="1",le="0.025"} 0
evm_indexer_flush_duration_seconds_bucket{chain="1",le="0.05"} 0
evm_indexer_flush_duration_seconds_bucket{chain="1",le="0.1"} 0
evm_indexer_flush_duration_seconds_bucket{chain="1",le="0.25"} 0
evm_indexer_flush_duration_seconds_bucket{chain="1",le="0.5"} 0
evm_indexer_flush_duration_seconds_bucket{chain="1",le="1"} 0
evm_indexer_flush_duration_seconds_bucket{chain="1",le="2.5"} 0
evm_indexer_flush_duration_seconds_bucket{chain="1",le="5"} 0
evm_indexer_flush_duration_seconds_bucket{chain="1",le="10"} 0
evm_indexer_flush_duration_seconds_bucket{chain="1",le="30"} 0
evm_indexer_flush_duration_seconds_bucket{chain="1",le="60"} 0
evm_indexer_flush_duration_seconds_bucket{chain="1",le="120"} 0
evm_indexer_flush_duration_seconds_bucket{chain="1",le="300"} 0
evm_indexer_flush_duration_seconds_bucket{chain="1",le="+Inf"} 0
evm_indexer_flush_duration_seconds_sum{chain="1"} 0
evm_indexer_flush_duration_seconds_count{chain="1"} 0
# HELP evm_indexer_stream_errors_total Sync passes that failed and were retried.
# TYPE evm_indexer_stream_errors_total counter
evm_indexer_stream_errors_total{chain="1"} 0
# HELP evm_indexer_reorgs_total Chain reorganizations detected.
# TYPE evm_indexer_reorgs_total counter
evm_indexer_reorgs_total{chain="1"} 0
# HELP evm_indexer_reorg_blocks_total Sum of the depths of all detected reorganizations.
# TYPE evm_indexer_reorg_blocks_total counter
evm_indexer_reorg_blocks_total{chain="1"} 0
# HELP evm_indexer_purge_duration_seconds Duration of purge_range (reorg rollback or gap healing).
# TYPE evm_indexer_purge_duration_seconds histogram
evm_indexer_purge_duration_seconds_bucket{chain="1",le="0.05"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="0.1"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="0.25"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="0.5"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="1"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="2.5"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="5"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="10"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="30"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="60"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="300"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="900"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="+Inf"} 0
evm_indexer_purge_duration_seconds_sum{chain="1"} 0
evm_indexer_purge_duration_seconds_count{chain="1"} 0
# HELP evm_indexer_purged_blocks_total Blocks removed by purge_range.
# TYPE evm_indexer_purged_blocks_total counter
evm_indexer_purged_blocks_total{chain="1"} 0
"#;

    assert_eq!(metrics.render_at(START * 1000), expected);
}

#[test]
fn golden_known_state() {
    let metrics = fixed("1");
    metrics.build_info("3.0.0", "abc1234");

    metrics.set_head_at(1_000, (START + 640) * 1000);
    metrics.set_indexed_height(990);
    metrics.set_head_timestamp(START + 600);
    metrics.set_indexed_timestamp(START + 480);

    metrics.rows_inserted("transactions", 500);
    metrics.rows_inserted("blocks", 10);
    metrics.rows_inserted("blocks", 5);

    // Exactly on a bound (`le` is inclusive), below the first bound,
    // beyond the last bound, and a failure.
    let at = (START + 650) * 1000;
    metrics.flush_observed_at(Duration::from_millis(250), 300, true, at);
    metrics.flush_observed_at(Duration::from_millis(4), 1, true, at);
    metrics.flush_observed_at(
        Duration::from_secs(2),
        7,
        false,
        at + 5_000,
    );
    metrics.flush_observed_at(Duration::from_secs(400), 515, true, at);

    metrics.flush_retry("logs");
    metrics.flush_retry("logs");
    metrics.channel_fill(3, 4);
    metrics.stream_error();
    metrics.reorg(2);
    metrics.reorg(5);
    metrics.purge_observed(Duration::from_millis(1_500), 5);
    metrics.set_token_stats(TokenStatsSnapshot {
        queue_depth: 12,
        resolved: 100,
        negative: 3,
        dropped: 1,
        cache_hits: 900,
        cache_misses: 104,
        breaker_open: false,
        endpoints_healthy: 2,
    });
    metrics.set_ready(true);

    let expected = r#"# HELP evm_indexer_build_info Build information; the value is always 1.
# TYPE evm_indexer_build_info gauge
evm_indexer_build_info{chain="1",version="3.0.0",commit="abc1234"} 1
# HELP evm_indexer_start_time_seconds Unix time the process started.
# TYPE evm_indexer_start_time_seconds gauge
evm_indexer_start_time_seconds{chain="1"} 1700000000
# HELP evm_indexer_ready 1 when /readyz answers 200, else 0.
# TYPE evm_indexer_ready gauge
evm_indexer_ready{chain="1"} 1
# HELP evm_indexer_head_block Chain head reported by the source.
# TYPE evm_indexer_head_block gauge
evm_indexer_head_block{chain="1"} 1000
# HELP evm_indexer_indexed_block Highest block durably stored.
# TYPE evm_indexer_indexed_block gauge
evm_indexer_indexed_block{chain="1"} 990
# HELP evm_indexer_lag_blocks Blocks between the chain head and the indexed block.
# TYPE evm_indexer_lag_blocks gauge
evm_indexer_lag_blocks{chain="1"} 10
# HELP evm_indexer_head_timestamp_seconds Timestamp of the chain head block.
# TYPE evm_indexer_head_timestamp_seconds gauge
evm_indexer_head_timestamp_seconds{chain="1"} 1700000600
# HELP evm_indexer_indexed_timestamp_seconds Timestamp of the highest block durably stored.
# TYPE evm_indexer_indexed_timestamp_seconds gauge
evm_indexer_indexed_timestamp_seconds{chain="1"} 1700000480
# HELP evm_indexer_lag_seconds Age of the indexed block relative to the head block, or to the wall clock when the head timestamp is unknown.
# TYPE evm_indexer_lag_seconds gauge
evm_indexer_lag_seconds{chain="1"} 120
# HELP evm_indexer_rows_inserted_total Rows durably inserted, per table.
# TYPE evm_indexer_rows_inserted_total counter
evm_indexer_rows_inserted_total{chain="1",table="blocks"} 15
evm_indexer_rows_inserted_total{chain="1",table="transactions"} 500
# HELP evm_indexer_flushes_total Flushes (one multi-table batch each), by result.
# TYPE evm_indexer_flushes_total counter
evm_indexer_flushes_total{chain="1",result="ok"} 3
evm_indexer_flushes_total{chain="1",result="error"} 1
# HELP evm_indexer_flush_duration_seconds Duration of a flush, retries included.
# TYPE evm_indexer_flush_duration_seconds histogram
evm_indexer_flush_duration_seconds_bucket{chain="1",le="0.01"} 1
evm_indexer_flush_duration_seconds_bucket{chain="1",le="0.025"} 1
evm_indexer_flush_duration_seconds_bucket{chain="1",le="0.05"} 1
evm_indexer_flush_duration_seconds_bucket{chain="1",le="0.1"} 1
evm_indexer_flush_duration_seconds_bucket{chain="1",le="0.25"} 2
evm_indexer_flush_duration_seconds_bucket{chain="1",le="0.5"} 2
evm_indexer_flush_duration_seconds_bucket{chain="1",le="1"} 2
evm_indexer_flush_duration_seconds_bucket{chain="1",le="2.5"} 3
evm_indexer_flush_duration_seconds_bucket{chain="1",le="5"} 3
evm_indexer_flush_duration_seconds_bucket{chain="1",le="10"} 3
evm_indexer_flush_duration_seconds_bucket{chain="1",le="30"} 3
evm_indexer_flush_duration_seconds_bucket{chain="1",le="60"} 3
evm_indexer_flush_duration_seconds_bucket{chain="1",le="120"} 3
evm_indexer_flush_duration_seconds_bucket{chain="1",le="300"} 3
evm_indexer_flush_duration_seconds_bucket{chain="1",le="+Inf"} 4
evm_indexer_flush_duration_seconds_sum{chain="1"} 402.254
evm_indexer_flush_duration_seconds_count{chain="1"} 4
# HELP evm_indexer_last_flush_rows Rows in the most recent flush.
# TYPE evm_indexer_last_flush_rows gauge
evm_indexer_last_flush_rows{chain="1"} 515
# HELP evm_indexer_last_successful_flush_timestamp_seconds Unix time of the most recent successful flush.
# TYPE evm_indexer_last_successful_flush_timestamp_seconds gauge
evm_indexer_last_successful_flush_timestamp_seconds{chain="1"} 1700000650
# HELP evm_indexer_flush_retries_total Inserts that failed and were retried, per table.
# TYPE evm_indexer_flush_retries_total counter
evm_indexer_flush_retries_total{chain="1",table="logs"} 2
# HELP evm_indexer_channel_len Batches queued between the transformer and the writer.
# TYPE evm_indexer_channel_len gauge
evm_indexer_channel_len{chain="1"} 3
# HELP evm_indexer_channel_capacity Capacity of the transformer to writer channel.
# TYPE evm_indexer_channel_capacity gauge
evm_indexer_channel_capacity{chain="1"} 4
# HELP evm_indexer_stream_errors_total Sync passes that failed and were retried.
# TYPE evm_indexer_stream_errors_total counter
evm_indexer_stream_errors_total{chain="1"} 1
# HELP evm_indexer_reorgs_total Chain reorganizations detected.
# TYPE evm_indexer_reorgs_total counter
evm_indexer_reorgs_total{chain="1"} 2
# HELP evm_indexer_reorg_blocks_total Sum of the depths of all detected reorganizations.
# TYPE evm_indexer_reorg_blocks_total counter
evm_indexer_reorg_blocks_total{chain="1"} 7
# HELP evm_indexer_reorg_last_depth Depth in blocks of the most recent reorganization.
# TYPE evm_indexer_reorg_last_depth gauge
evm_indexer_reorg_last_depth{chain="1"} 5
# HELP evm_indexer_purge_duration_seconds Duration of purge_range (reorg rollback or gap healing).
# TYPE evm_indexer_purge_duration_seconds histogram
evm_indexer_purge_duration_seconds_bucket{chain="1",le="0.05"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="0.1"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="0.25"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="0.5"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="1"} 0
evm_indexer_purge_duration_seconds_bucket{chain="1",le="2.5"} 1
evm_indexer_purge_duration_seconds_bucket{chain="1",le="5"} 1
evm_indexer_purge_duration_seconds_bucket{chain="1",le="10"} 1
evm_indexer_purge_duration_seconds_bucket{chain="1",le="30"} 1
evm_indexer_purge_duration_seconds_bucket{chain="1",le="60"} 1
evm_indexer_purge_duration_seconds_bucket{chain="1",le="300"} 1
evm_indexer_purge_duration_seconds_bucket{chain="1",le="900"} 1
evm_indexer_purge_duration_seconds_bucket{chain="1",le="+Inf"} 1
evm_indexer_purge_duration_seconds_sum{chain="1"} 1.5
evm_indexer_purge_duration_seconds_count{chain="1"} 1
# HELP evm_indexer_purged_blocks_total Blocks removed by purge_range.
# TYPE evm_indexer_purged_blocks_total counter
evm_indexer_purged_blocks_total{chain="1"} 5
# HELP evm_indexer_resolver_queue_depth Addresses waiting to be resolved by a background worker.
# TYPE evm_indexer_resolver_queue_depth gauge
evm_indexer_resolver_queue_depth{chain="1",worker="tokens"} 12
# HELP evm_indexer_resolver_resolved_total Addresses resolved with metadata.
# TYPE evm_indexer_resolver_resolved_total counter
evm_indexer_resolver_resolved_total{chain="1",worker="tokens"} 100
# HELP evm_indexer_resolver_negative_total Addresses resolved to nothing (reverts, garbage).
# TYPE evm_indexer_resolver_negative_total counter
evm_indexer_resolver_negative_total{chain="1",worker="tokens"} 3
# HELP evm_indexer_resolver_dropped_total Discoveries dropped because the queue was full.
# TYPE evm_indexer_resolver_dropped_total counter
evm_indexer_resolver_dropped_total{chain="1",worker="tokens"} 1
# HELP evm_indexer_resolver_cache_hits_total Resolver cache hits.
# TYPE evm_indexer_resolver_cache_hits_total counter
evm_indexer_resolver_cache_hits_total{chain="1",worker="tokens"} 900
# HELP evm_indexer_resolver_cache_misses_total Resolver cache misses.
# TYPE evm_indexer_resolver_cache_misses_total counter
evm_indexer_resolver_cache_misses_total{chain="1",worker="tokens"} 104
# HELP evm_indexer_resolver_breaker_open 1 when every RPC endpoint's circuit breaker is open.
# TYPE evm_indexer_resolver_breaker_open gauge
evm_indexer_resolver_breaker_open{chain="1",worker="tokens"} 0
# HELP evm_indexer_resolver_endpoints_healthy RPC endpoints currently considered healthy.
# TYPE evm_indexer_resolver_endpoints_healthy gauge
evm_indexer_resolver_endpoints_healthy{chain="1",worker="tokens"} 2
"#;

    assert_eq!(metrics.render_at((START + 660) * 1000), expected);
}

#[test]
fn both_workers_share_families() {
    let metrics = fixed("1");
    metrics.set_pool_stats(PoolStatsSnapshot {
        queue_depth: 7,
        breaker_open: true,
        ..Default::default()
    });
    metrics.set_token_stats(TokenStatsSnapshot::default());

    let text = metrics.render_at(0);

    assert_eq!(
        text.matches("# TYPE evm_indexer_resolver_queue_depth ").count(),
        1
    );
    let tokens = text
        .find("evm_indexer_resolver_queue_depth{chain=\"1\",worker=\"tokens\"} 0\n")
        .unwrap();
    let pools = text
        .find("evm_indexer_resolver_queue_depth{chain=\"1\",worker=\"pools\"} 7\n")
        .unwrap();
    assert!(tokens < pools);
    assert_eq!(
        value(
            &text,
            "evm_indexer_resolver_breaker_open{chain=\"1\",worker=\"pools\"}"
        ),
        Some(1.0)
    );
}

#[test]
fn lag_seconds_falls_back_to_the_wall_clock() {
    let metrics = fixed("1");
    metrics.set_indexed_timestamp(START);

    let text = metrics.render_at((START + 42) * 1000);
    assert_eq!(
        value(&text, "evm_indexer_lag_seconds{chain=\"1\"}"),
        Some(42.0)
    );
    assert!(!text.contains("evm_indexer_head_timestamp_seconds"));

    // Clock skew or an indexed block ahead of a stale head: never negative.
    metrics.set_head_timestamp(START - 10);
    metrics.set_head(5);
    metrics.set_indexed_height(9);
    let text = metrics.render_at((START + 42) * 1000);
    assert_eq!(
        value(&text, "evm_indexer_lag_seconds{chain=\"1\"}"),
        Some(0.0)
    );
    assert_eq!(
        value(&text, "evm_indexer_lag_blocks{chain=\"1\"}"),
        Some(0.0)
    );
}

#[test]
fn label_values_and_help_are_escaped() {
    let mut out = String::new();
    escape_label_value("a\"b\\c\nd é", &mut out);
    assert_eq!(out, r#"a\"b\\c\nd é"#);

    let mut out = String::new();
    escape_help("say \"hi\" \\ twice\nplease", &mut out);
    assert_eq!(out, r#"say "hi" \\ twice\nplease"#);

    let mut encoder = Encoder::new("main\"net\\\n");
    encoder.family("x_total", "line one\nline \\two", Kind::Counter);
    encoder.sample("x_total", &[("table", "we\"ird\\name\n")], 3);

    assert_eq!(
        encoder.finish(),
        concat!(
            "# HELP evm_indexer_x_total line one\\nline \\\\two\n",
            "# TYPE evm_indexer_x_total counter\n",
            "evm_indexer_x_total{chain=\"main\\\"net\\\\\\n\",",
            "table=\"we\\\"ird\\\\name\\n\"} 3\n",
        )
    );

    // End to end through the handle: every line stays a single line.
    let metrics = fixed("1");
    metrics.build_info("3.0.0\n\"dirty\"", "back\\slash");
    metrics.rows_inserted("bad\"table\n", 1);
    let text = metrics.render_at(0);

    assert!(text.contains(
        r#"evm_indexer_build_info{chain="1",version="3.0.0\n\"dirty\"",commit="back\\slash"} 1"#
    ));
    assert!(text.contains(
        r#"evm_indexer_rows_inserted_total{chain="1",table="bad\"table\n"} 1"#
    ));
    assert!(text
        .lines()
        .all(|line| line.starts_with("# ")
            || line.starts_with("evm_indexer_")));
}

#[test]
fn histogram_buckets_are_cumulative() {
    static BOUNDS: &[f64] = &[0.1, 1.0, 10.0];
    let histogram = Histogram::new(BOUNDS);

    for millis in [50, 100, 101, 1_000, 5_000, 10_001, 60_000] {
        histogram.observe(Duration::from_millis(millis));
    }

    let snapshot = histogram.snapshot();
    assert_eq!(snapshot.buckets, vec![(0.1, 2), (1.0, 4), (10.0, 5)]);
    assert_eq!(snapshot.count, 7);
    assert!((snapshot.sum_seconds - 76.252).abs() < 1e-9);

    let mut encoder = Encoder::new("1");
    encoder.histogram("h_seconds", "h", &histogram);
    assert_eq!(
        encoder.finish(),
        r#"# HELP evm_indexer_h_seconds h
# TYPE evm_indexer_h_seconds histogram
evm_indexer_h_seconds_bucket{chain="1",le="0.1"} 2
evm_indexer_h_seconds_bucket{chain="1",le="1"} 4
evm_indexer_h_seconds_bucket{chain="1",le="10"} 5
evm_indexer_h_seconds_bucket{chain="1",le="+Inf"} 7
evm_indexer_h_seconds_sum{chain="1"} 76.252
evm_indexer_h_seconds_count{chain="1"} 7
"#
    );
}

// ---------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------

#[test]
fn disabled_handle_is_a_no_op() {
    for metrics in [Metrics::disabled(), Metrics::default()] {
        assert!(!metrics.is_enabled());

        metrics.build_info("1", "2");
        metrics.set_head(1);
        metrics.set_indexed_height(1);
        metrics.set_head_timestamp(1);
        metrics.set_indexed_timestamp(1);
        metrics.rows_inserted("blocks", 1);
        metrics.flush_observed(Duration::from_secs(1), 1, true);
        metrics.flush_retry("blocks");
        metrics.channel_fill(1, 4);
        metrics.reorg(3);
        metrics.purge_observed(Duration::from_secs(1), 3);
        metrics.stream_error();
        metrics.set_token_stats(TokenStatsSnapshot::default());
        metrics.set_pool_stats(PoolStatsSnapshot::default());
        metrics.set_ready(true);

        assert_eq!(metrics.render(), "");
        assert_eq!(
            metrics.readiness().unwrap_err(),
            "not ready: metrics are disabled"
        );
        assert_eq!(format!("{metrics:?}"), "Metrics(disabled)");
        // Nothing behind the handle.
        assert_eq!(
            std::mem::size_of::<Metrics>(),
            std::mem::size_of::<usize>()
        );
    }
}

#[test]
fn clones_share_state() {
    let metrics = Metrics::new(10, Duration::from_secs(60));
    let clone = metrics.clone();

    clone.reorg(4);
    assert!(metrics.is_enabled());
    assert_eq!(
        value(&metrics.render(), "evm_indexer_reorgs_total{chain=\"10\"}"),
        Some(1.0)
    );
}

#[test]
fn readiness_needs_the_flag_and_a_recent_sign_of_life() {
    let metrics = fixed("1");
    let t0 = START * 1000;

    assert_eq!(
        metrics.readiness_at(t0).unwrap_err(),
        "not ready: startup has not completed"
    );

    // A sign of life alone is not enough.
    metrics.set_head_at(100, t0);
    assert!(metrics.readiness_at(t0).is_err());

    metrics.set_ready(true);
    assert_eq!(metrics.readiness_at(t0), Ok(()));
    assert_eq!(metrics.readiness_at(t0 + 120_000), Ok(()));
    assert_eq!(
        metrics.readiness_at(t0 + 121_000).unwrap_err(),
        "not ready: last successful flush or head poll was 121s ago \
         (limit 120s)"
    );

    // A failed flush is not a sign of life, a successful one is.
    metrics.flush_observed_at(Duration::ZERO, 1, false, t0 + 121_000);
    assert!(metrics.readiness_at(t0 + 121_000).is_err());
    metrics.flush_observed_at(Duration::ZERO, 1, true, t0 + 121_000);
    assert_eq!(metrics.readiness_at(t0 + 121_000), Ok(()));

    metrics.set_ready(false);
    assert!(metrics.readiness_at(t0 + 121_000).is_err());

    let never = fixed("1");
    never.set_ready(true);
    assert_eq!(
        never.readiness_at(t0).unwrap_err(),
        "not ready: no successful flush or head poll yet"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_updates_are_not_lost() {
    const TASKS: u64 = 32;
    const ROUNDS: u64 = 2_000;
    const TABLES: [&str; 4] = ["blocks", "logs", "dex_swaps", "transactions"];

    let metrics = Metrics::new(1, Duration::from_secs(60));

    // Scrapes race with the writers; every one must be self-consistent.
    let scraper = {
        let metrics = metrics.clone();
        tokio::spawn(async move {
            for _ in 0..200 {
                let text = metrics.render();
                let inf = value(
                    &text,
                    "evm_indexer_flush_duration_seconds_bucket{chain=\"1\",le=\"+Inf\"}",
                );
                let count = value(
                    &text,
                    "evm_indexer_flush_duration_seconds_count{chain=\"1\"}",
                );
                assert_eq!(inf, count);

                let buckets: Vec<f64> = text
                    .lines()
                    .filter(|l| {
                        l.starts_with(
                            "evm_indexer_flush_duration_seconds_bucket",
                        )
                    })
                    .map(|l| {
                        l.rsplit(' ').next().unwrap().parse().unwrap()
                    })
                    .collect();
                assert!(buckets.windows(2).all(|w| w[0] <= w[1]));

                tokio::task::yield_now().await;
            }
        })
    };

    let writers: Vec<_> = (0..TASKS)
        .map(|task| {
            let metrics = metrics.clone();
            tokio::spawn(async move {
                for round in 0..ROUNDS {
                    let table = TABLES[((task + round) % 4) as usize];
                    metrics.rows_inserted(table, 3);
                    metrics.flush_retry(table);
                    metrics.flush_observed(
                        Duration::from_millis(round % 700),
                        round,
                        round % 2 == 0,
                    );
                    metrics.reorg(2);
                    metrics.purge_observed(Duration::from_millis(1), 2);
                    metrics.stream_error();
                    metrics.set_head(round);

                    if round % 256 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            })
        })
        .collect();

    for writer in writers {
        writer.await.unwrap();
    }
    scraper.await.unwrap();

    let text = metrics.render();
    let total = (TASKS * ROUNDS) as f64;
    let get = |series: &str| value(&text, series).unwrap();

    for table in TABLES {
        assert_eq!(
            get(&format!(
                "evm_indexer_rows_inserted_total{{chain=\"1\",table=\"{table}\"}}"
            )),
            total / 4.0 * 3.0
        );
        assert_eq!(
            get(&format!(
                "evm_indexer_flush_retries_total{{chain=\"1\",table=\"{table}\"}}"
            )),
            total / 4.0
        );
    }

    assert_eq!(
        get("evm_indexer_flushes_total{chain=\"1\",result=\"ok\"}"),
        total / 2.0
    );
    assert_eq!(
        get("evm_indexer_flushes_total{chain=\"1\",result=\"error\"}"),
        total / 2.0
    );
    assert_eq!(
        get("evm_indexer_flush_duration_seconds_count{chain=\"1\"}"),
        total
    );
    assert_eq!(get("evm_indexer_reorgs_total{chain=\"1\"}"), total);
    assert_eq!(
        get("evm_indexer_reorg_blocks_total{chain=\"1\"}"),
        total * 2.0
    );
    assert_eq!(
        get("evm_indexer_purged_blocks_total{chain=\"1\"}"),
        total * 2.0
    );
    assert_eq!(get("evm_indexer_stream_errors_total{chain=\"1\"}"), total);
    assert_eq!(
        get("evm_indexer_purge_duration_seconds_bucket{chain=\"1\",le=\"0.05\"}"),
        total
    );
}

// ---------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------

struct TestServer {
    addr: SocketAddr,
    stop: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
}

async fn start(metrics: Metrics, request_timeout: Duration) -> TestServer {
    let server = bind("127.0.0.1:0".parse().unwrap(), metrics)
        .await
        .unwrap()
        .with_request_timeout(request_timeout);
    let addr = server.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async move {
        let _ = stopped.await;
    }));

    TestServer { addr, stop, task }
}

struct Reply {
    status: u16,
    head: String,
    body: String,
}

/// Sends raw bytes, reads until the server closes the connection.
async fn raw(addr: SocketAddr, request: &[u8]) -> Reply {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request).await.unwrap();

    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.unwrap();
    let text = String::from_utf8(bytes).unwrap();

    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head
        .strip_prefix("HTTP/1.1 ")
        .and_then(|rest| rest.get(..3))
        .unwrap()
        .parse()
        .unwrap();

    Reply { status, head: head.to_string(), body: body.to_string() }
}

async fn get(addr: SocketAddr, path: &str) -> Reply {
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: test\r\nAccept: */*\r\n\r\n"
    );
    raw(addr, request.as_bytes()).await
}

#[tokio::test]
async fn serves_metrics_and_healthz() {
    let metrics = Metrics::new(143, Duration::from_secs(60));
    metrics.rows_inserted("blocks", 9);
    let server = start(metrics.clone(), Duration::from_secs(5)).await;

    let reply = get(server.addr, "/healthz").await;
    assert_eq!((reply.status, reply.body.as_str()), (200, "ok\n"));

    let reply = get(server.addr, "/metrics").await;
    assert_eq!(reply.status, 200);
    assert!(reply.head.contains(
        "Content-Type: text/plain; version=0.0.4; charset=utf-8"
    ));
    assert!(reply
        .head
        .contains(&format!("Content-Length: {}", reply.body.len())));
    assert!(reply.head.contains("Connection: close"));
    assert!(reply.body.contains(
        "evm_indexer_rows_inserted_total{chain=\"143\",table=\"blocks\"} 9\n"
    ));

    // Live state, query strings, HEAD.
    metrics.rows_inserted("blocks", 1);
    let reply = get(server.addr, "/metrics?format=text").await;
    assert!(reply.body.contains("table=\"blocks\"} 10\n"));

    let reply =
        raw(server.addr, b"HEAD /metrics HTTP/1.1\r\nHost: t\r\n\r\n")
            .await;
    assert_eq!(reply.status, 200);
    assert!(reply.body.is_empty());
    assert!(!reply.head.contains("Content-Length: 0"));

    // A disabled handle still answers.
    let disabled =
        start(Metrics::disabled(), Duration::from_secs(5)).await;
    let reply = get(disabled.addr, "/metrics").await;
    assert_eq!((reply.status, reply.body.as_str()), (200, ""));
    assert_eq!(get(disabled.addr, "/healthz").await.status, 200);
    assert_eq!(get(disabled.addr, "/readyz").await.status, 503);
}

#[tokio::test]
async fn readyz_follows_the_readiness_transitions() {
    let metrics = Metrics::new(1, Duration::from_millis(300));
    let server = start(metrics.clone(), Duration::from_secs(5)).await;

    let reply = get(server.addr, "/readyz").await;
    assert_eq!(reply.status, 503);
    assert_eq!(reply.body, "not ready: startup has not completed\n");

    metrics.set_ready(true);
    let reply = get(server.addr, "/readyz").await;
    assert_eq!(reply.status, 503);
    assert_eq!(
        reply.body,
        "not ready: no successful flush or head poll yet\n"
    );

    metrics.set_head(100);
    let reply = get(server.addr, "/readyz").await;
    assert_eq!((reply.status, reply.body.as_str()), (200, "ready\n"));
    let text = get(server.addr, "/metrics").await.body;
    assert_eq!(value(&text, "evm_indexer_ready{chain=\"1\"}"), Some(1.0));

    // Nothing happens for longer than the staleness limit.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let reply = get(server.addr, "/readyz").await;
    assert_eq!(reply.status, 503);
    assert!(reply.body.starts_with(
        "not ready: last successful flush or head poll was "
    ));
    assert_eq!(reply.body.lines().count(), 1);
    let text = get(server.addr, "/metrics").await.body;
    assert_eq!(value(&text, "evm_indexer_ready{chain=\"1\"}"), Some(0.0));

    // A failed flush does not help, a successful one does.
    metrics.flush_observed(Duration::from_millis(5), 10, false);
    assert_eq!(get(server.addr, "/readyz").await.status, 503);
    metrics.flush_observed(Duration::from_millis(5), 10, true);
    assert_eq!(get(server.addr, "/readyz").await.status, 200);

    metrics.set_ready(false);
    assert_eq!(get(server.addr, "/readyz").await.status, 503);

    // Liveness never depended on any of it.
    assert_eq!(get(server.addr, "/healthz").await.status, 200);
}

#[tokio::test]
async fn malformed_requests_get_an_error_and_do_no_harm() {
    let metrics = Metrics::new(1, Duration::from_secs(60));
    let server = start(metrics, Duration::from_millis(300)).await;
    let addr = server.addr;

    for garbage in [
        &b"\x00\xff\xfe\x01 binary garbage\r\n\r\n"[..],
        b"GARBAGE\r\n\r\n",
        b"\r\n\r\n",
        b"\n\n",
        b"GET\r\n\r\n",
        b"GET /metrics\r\n\r\n",
        b"GET /metrics HTTP/1.1 extra\r\n\r\n",
        b"GET metrics HTTP/1.1\r\n\r\n",
        b"GET /metrics SPDY/9\r\n\r\n",
        b"GET  /metrics HTTP/1.1\r\n\r\n",
    ] {
        let reply = raw(addr, garbage).await;
        assert_eq!(reply.status, 400, "{garbage:?}");
        assert_eq!(reply.body, "bad request\n");
    }

    let reply =
        raw(addr, b"POST /metrics HTTP/1.1\r\nContent-Length: 0\r\n\r\n")
            .await;
    assert_eq!(reply.status, 405);
    assert!(reply.head.contains("Allow: GET, HEAD"));

    assert_eq!(get(addr, "/").await.status, 404);
    assert_eq!(get(addr, "/metrics/").await.status, 404);
    assert_eq!(get(addr, "/../etc/passwd").await.status, 404);

    // Bare LF line endings are tolerated.
    assert_eq!(raw(addr, b"GET /healthz HTTP/1.0\n\n").await.status, 200);

    // Headers that never end: cut off at the size limit.
    let mut oversized = b"GET /metrics HTTP/1.1\r\nX-Junk: ".to_vec();
    oversized.resize(8 * 1024, b'a');
    assert_eq!(raw(addr, &oversized).await.status, 431);

    // A request that never completes: cut off at the time limit.
    let started = std::time::Instant::now();
    let reply = raw(addr, b"GET /metrics HTTP/1.1\r\nHost: slow").await;
    assert_eq!(reply.status, 408);
    assert!(started.elapsed() < Duration::from_secs(3));

    // A client that connects and leaves.
    drop(TcpStream::connect(addr).await.unwrap());

    assert_eq!(get(addr, "/healthz").await.status, 200);
    assert!(!server.task.is_finished());
}

#[tokio::test]
async fn stuck_clients_cannot_exhaust_the_server() {
    let metrics = Metrics::new(1, Duration::from_secs(60));
    let server = start(metrics, Duration::from_millis(300)).await;

    // More silent connections than the server serves at once.
    let mut idle = Vec::new();
    for _ in 0..80 {
        idle.push(TcpStream::connect(server.addr).await.unwrap());
    }

    // They are all timed out and released; the clients are still open.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(get(server.addr, "/healthz").await.status, 200);
    drop(idle);
}

#[tokio::test]
async fn a_taken_port_is_a_clear_error() {
    let metrics = Metrics::new(1, Duration::from_secs(60));
    let server = start(metrics.clone(), Duration::from_secs(5)).await;

    let error = serve(server.addr, metrics, std::future::pending())
        .await
        .unwrap_err();
    let message = format!("{error:#}");

    assert!(message.contains("cannot bind the metrics endpoint to"));
    assert!(message.contains(&server.addr.to_string()));
    assert!(message.contains("already in use"));
}

#[tokio::test]
async fn shutdown_stops_the_server() {
    let metrics = Metrics::new(1, Duration::from_secs(60));
    let server = start(metrics, Duration::from_secs(30)).await;

    assert_eq!(get(server.addr, "/healthz").await.status, 200);

    // An in-flight, never finishing request must not delay the shutdown.
    let mut stuck = TcpStream::connect(server.addr).await.unwrap();
    stuck.write_all(b"GET /metr").await.unwrap();

    server.stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server.task)
        .await
        .expect("server did not stop")
        .unwrap()
        .unwrap();

    // The stuck connection was dropped, and nobody is listening any more.
    let mut rest = Vec::new();
    let _ = stuck.read_to_end(&mut rest).await;
    assert!(rest.is_empty());
    assert!(TcpStream::connect(server.addr).await.is_err());
}
