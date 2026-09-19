//! The supervisor, driven with fake chains (`super::fixtures`): start,
//! stop, restart, the backoff, "running elsewhere", and the promise that
//! one broken chain never touches the others.

use super::{
    fixtures::{
        config, desired, fleet, settings, until, Behaviour, FakeRunner,
        MemoryStore,
    },
    supervisor::{self, restart_backoff, CommandError, Supervisor},
};
use crate::{
    configs::Desired, fleet::chains::ForeignChain,
    pipeline::status::ChainState,
};
use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

async fn state(supervisor: &Arc<Supervisor>, chain: u64) -> String {
    supervisor
        .views()
        .await
        .into_iter()
        .find(|view| view.chain == chain)
        .map(|view| view.live.state.to_string())
        .unwrap_or_else(|| "missing".to_string())
}

#[tokio::test]
async fn the_chains_of_the_table_are_started_at_boot() {
    let (supervisor, runner, _) = fleet(&[1, 8453]);
    supervisor.load_and_start().await.unwrap();

    until("both chains running", || {
        runner.is_live(1) && runner.is_live(8453)
    })
    .await;

    assert_eq!(runner.starts(1), 1);
    assert_eq!(runner.starts(8453), 1);

    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_chain_the_table_says_is_stopped_is_not_started() {
    let (supervisor, runner, store) = fleet(&[]);
    store.seed(desired(1, Desired::Running));
    store.seed(desired(10, Desired::Stopped));

    supervisor.load_and_start().await.unwrap();
    until("chain 1 running", || runner.is_live(1)).await;

    assert_eq!(runner.starts(10), 0);
    assert_eq!(state(&supervisor, 10).await, "stopped");

    supervisor.shutdown().await;
}

#[tokio::test]
async fn stopping_one_chain_leaves_the_others_indexing() {
    let (supervisor, runner, store) = fleet(&[1, 8453, 10]);
    supervisor.load_and_start().await.unwrap();

    until("all three running", || {
        runner.is_live(1) && runner.is_live(8453) && runner.is_live(10)
    })
    .await;

    supervisor.stop(8453).await.unwrap();

    until("8453 stopped", || !runner.is_live(8453)).await;

    // The other two never even noticed.
    assert!(runner.is_live(1));
    assert!(runner.is_live(10));
    assert_eq!(runner.starts(1), 1);
    assert_eq!(runner.starts(10), 1);

    // ... and the stop was remembered for the next start of the process.
    assert_eq!(store.stored(8453).unwrap().desired, Desired::Stopped);
    assert_eq!(state(&supervisor, 8453).await, "stopped");

    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_stopped_chain_starts_again_on_command() {
    let (supervisor, runner, store) = fleet(&[1]);
    supervisor.load_and_start().await.unwrap();
    until("running", || runner.is_live(1)).await;

    supervisor.stop(1).await.unwrap();
    until("stopped", || !runner.is_live(1)).await;

    supervisor.start(1).await.unwrap();
    until("running again", || runner.is_live(1)).await;

    assert_eq!(runner.starts(1), 2);
    assert_eq!(store.stored(1).unwrap().desired, Desired::Running);

    supervisor.shutdown().await;
}

#[tokio::test]
async fn restart_stops_and_starts_the_same_chain_with_no_backoff() {
    let (supervisor, runner, _) = fleet(&[1]);
    supervisor.load_and_start().await.unwrap();
    until("running", || runner.is_live(1)).await;

    supervisor.restart(1).await.unwrap();

    until("started a second time", || runner.starts(1) == 2).await;
    until("running again", || runner.is_live(1)).await;

    supervisor.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_failing_chain_is_restarted_with_a_growing_wait() {
    let (supervisor, runner, _) = fleet(&[1, 8453]);
    runner.behave(1, Behaviour::Fails);
    supervisor.load_and_start().await.unwrap();

    // The healthy chain came up and stays up while the other one flaps.
    until("the healthy chain is running", || runner.is_live(8453)).await;

    until("the broken chain failed once", || runner.starts(1) >= 1).await;
    assert_eq!(state(&supervisor, 1).await, "failed");

    // Nothing happens until the first wait is over ...
    tokio::time::sleep(restart_backoff(1) / 2).await;
    assert_eq!(runner.starts(1), 1, "restarted before its backoff");

    // ... and then it tries again.
    tokio::time::sleep(restart_backoff(1)).await;
    until("restarted", || runner.starts(1) >= 2).await;

    // The wait grows: a whole second backoff has to pass for the third.
    let before = runner.starts(1);
    tokio::time::sleep(restart_backoff(2) + Duration::from_secs(1)).await;
    until("restarted again", || runner.starts(1) > before).await;

    // The healthy chain was started exactly once, the whole time.
    assert_eq!(runner.starts(8453), 1);
    assert!(runner.is_live(8453));

    supervisor.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_broken_chain_never_waits_longer_than_five_minutes() {
    let (supervisor, runner, _) = fleet(&[1]);
    runner.behave(1, Behaviour::Fails);
    supervisor.load_and_start().await.unwrap();

    until("it failed once", || runner.starts(1) >= 1).await;

    // Twenty minutes with a five minute cap: at least three more attempts.
    tokio::time::sleep(Duration::from_secs(20 * 60)).await;
    until("it kept trying", || runner.starts(1) >= 4).await;

    supervisor.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_lease_held_elsewhere_is_a_state_and_not_an_error_loop() {
    let (supervisor, runner, _) = fleet(&[1]);
    runner.behave(1, Behaviour::LeaseHeldElsewhere);
    supervisor.load_and_start().await.unwrap();

    until("it looked once", || runner.starts(1) >= 1).await;
    until("it says so", || true).await;
    assert_eq!(state(&supervisor, 1).await, "running_elsewhere");

    // It is NOT counted as a failure: no error is shown and no restart is
    // announced.
    let view = supervisor
        .views()
        .await
        .into_iter()
        .find(|view| view.chain == 1)
        .unwrap();
    assert_eq!(view.live.last_error, None);
    assert_eq!(view.live.restarts, 0);

    // And it keeps looking at a steady, unhurried interval rather than
    // backing off to five minutes.
    tokio::time::sleep(Duration::from_secs(5 * 60)).await;
    let looks = runner.starts(1);
    assert!(looks >= 5, "only looked {looks} times in five minutes");

    // The moment the other process lets go, this one takes over.
    runner.behave(1, Behaviour::Indexes);
    tokio::time::sleep(Duration::from_secs(60)).await;
    until("took over", || runner.is_live(1)).await;
    assert_eq!(state(&supervisor, 1).await, "starting");

    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_chain_that_reaches_its_end_block_stops_and_stays_stopped() {
    let (supervisor, runner, _) = fleet(&[1]);
    runner.behave(1, Behaviour::Finishes);
    supervisor.load_and_start().await.unwrap();

    until("it finished", || runner.starts(1) >= 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(runner.starts(1), 1, "a finished chain was restarted");
    assert_eq!(state(&supervisor, 1).await, "stopped");

    supervisor.shutdown().await;
}

#[tokio::test]
async fn adding_a_chain_needs_only_its_id() {
    let (supervisor, runner, store) = fleet(&[]);
    supervisor.load_and_start().await.unwrap();

    supervisor.add(8453, Default::default()).await.unwrap();

    until("running", || runner.is_live(8453)).await;
    assert_eq!(store.stored(8453).unwrap().desired, Desired::Running);
    assert!(store.stored(8453).unwrap().settings.is_empty());

    // Twice is a mistake, not a restart.
    let again = supervisor.add(8453, Default::default()).await;
    assert!(matches!(again, Err(CommandError::AlreadyThere(8453))));

    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_setting_the_command_line_would_refuse_never_reaches_a_chain() {
    let (supervisor, runner, store) = fleet(&[]);
    supervisor.load_and_start().await.unwrap();

    let refused =
        supervisor.add(1, settings(&[("delete-everything", "yes")])).await;

    assert!(matches!(refused, Err(CommandError::Settings(_))));
    assert_eq!(runner.starts(1), 0);
    assert!(store.stored(1).is_none());

    supervisor.shutdown().await;
}

#[tokio::test]
async fn new_settings_apply_at_the_next_start_and_are_remembered() {
    let (supervisor, runner, store) = fleet(&[1]);
    supervisor.load_and_start().await.unwrap();
    until("running", || runner.is_live(1)).await;

    assert!(runner.started_with(1).unwrap().is_empty());

    supervisor
        .update_settings(1, settings(&[("confirmations", "12")]))
        .await
        .unwrap();

    // The chain that is running was NOT restarted behind the owner's back.
    assert_eq!(runner.starts(1), 1);
    assert!(runner.started_with(1).unwrap().is_empty());
    assert_eq!(store.stored(1).unwrap().settings["confirmations"], "12");

    // A restart is what applies them.
    supervisor.restart(1).await.unwrap();
    until("started again", || runner.starts(1) == 2).await;
    until("with the new settings", || {
        runner
            .started_with(1)
            .is_some_and(|s| s.contains_key("confirmations"))
    })
    .await;

    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_command_for_a_chain_that_is_not_here_is_refused() {
    let (supervisor, _, _) = fleet(&[]);
    supervisor.load_and_start().await.unwrap();

    for result in [
        supervisor.start(99).await,
        supervisor.stop(99).await,
        supervisor.restart(99).await,
        supervisor.update_settings(99, Default::default()).await,
    ] {
        assert!(matches!(result, Err(CommandError::NoSuchChain(99))));
    }

    assert!(supervisor.events(99).is_none());
}

#[tokio::test]
async fn a_database_that_is_down_never_blocks_a_command() {
    let (supervisor, runner, store) = fleet(&[1]);
    supervisor.load_and_start().await.unwrap();
    until("running", || runner.is_live(1)).await;

    store.broken.store(true, Ordering::SeqCst);

    // The command takes effect in memory even though nothing can be
    // written: only the NEXT start of the process forgets it.
    supervisor.stop(1).await.unwrap();
    until("stopped", || !runner.is_live(1)).await;
    assert_eq!(store.stored(1).unwrap().desired, Desired::Running);

    supervisor.shutdown().await;
}

#[tokio::test]
async fn chains_of_another_process_are_listed_read_only() {
    let (supervisor, _, store) = fleet(&[1]);
    store.set_foreign(vec![
        ForeignChain {
            chain: 137,
            host: "indexer-2".to_string(),
            heartbeat_ms: 1,
        },
        // Ours: must not be listed twice.
        ForeignChain {
            chain: 1,
            host: "someone-else".to_string(),
            heartbeat_ms: 1,
        },
    ]);

    supervisor.load_and_start().await.unwrap();

    let views = supervisor.views().await;
    assert_eq!(views.len(), 2, "{views:?}");

    let mine = views.iter().find(|v| v.chain == 1).unwrap();
    assert!(mine.managed);

    let theirs = views.iter().find(|v| v.chain == 137).unwrap();
    assert!(!theirs.managed);
    assert_eq!(theirs.live.state, "running_elsewhere");
    assert_eq!(theirs.host.as_deref(), Some("indexer-2"));
    assert!(theirs.settings.is_empty());

    // This process holds no task for it, so no button can reach it.
    assert!(!supervisor.knows(137));

    supervisor.shutdown().await;
}

#[tokio::test]
async fn the_memory_cap_is_split_over_the_chains_that_run() {
    let runner = FakeRunner::new();
    let store = MemoryStore::new();
    let mut config = config();
    config.max_inflight_mb = 1_024;
    config.chains = vec![1, 10, 8453, 137];

    let supervisor =
        Supervisor::new(config, runner.clone(), store.clone());
    supervisor.load_and_start().await.unwrap();

    until("all four running", || runner.total_starts() == 4).await;

    // 1024 MB / 512 bytes a row / 4 chains.
    let expected = 1_024 * 1024 * 1024 / 512 / 4;
    for chain in [1, 10, 8453, 137] {
        assert_eq!(runner.flush_rows(chain), Some(expected));
    }

    supervisor.shutdown().await;
}

/// Review MINOR 9: a signed-in session could add chains until the process
/// ran out of memory.
#[tokio::test]
async fn a_fleet_will_not_hold_more_chains_than_its_cap() {
    let (supervisor, runner, _) = fleet(&[]);
    supervisor.load_and_start().await.unwrap();

    for chain in 1..=(supervisor::MAX_CHAINS as u64) {
        supervisor
            .add(chain, Default::default())
            .await
            .unwrap_or_else(|e| panic!("chain {chain}: {e}"));
    }

    let refused = supervisor.add(9_999, Default::default()).await;
    assert!(
        matches!(refused, Err(CommandError::TooMany(_))),
        "{refused:?}"
    );
    assert_eq!(runner.starts(9_999), 0);

    supervisor.shutdown().await;
}

/// Review MINOR 9: `shutdown` awaited every chain with no deadline, so one
/// chain that never returned hung the process for ever.
#[tokio::test(start_paused = true)]
async fn shutdown_gives_up_on_a_chain_that_will_not_stop() {
    let (supervisor, runner, _) = fleet(&[1]);
    runner.behave(1, Behaviour::NeverStops);
    supervisor.load_and_start().await.unwrap();
    until("running", || runner.is_live(1)).await;

    // Without a deadline this never returns.
    let stopping = tokio::time::timeout(
        supervisor::SHUTDOWN_DEADLINE * 3,
        supervisor.shutdown(),
    )
    .await;

    assert!(stopping.is_ok(), "the fleet hung on one stuck chain");
}

#[tokio::test]
async fn shutting_the_fleet_down_stops_every_chain_gracefully() {
    let (supervisor, runner, _) = fleet(&[1, 10, 8453]);
    supervisor.load_and_start().await.unwrap();
    until("all running", || runner.total_starts() == 3).await;

    supervisor.shutdown().await;

    // `shutdown` waits for the tasks, so by the time it returns nothing is
    // inside `run` any more and nothing was restarted.
    for chain in [1, 10, 8453] {
        assert!(!runner.is_live(chain), "chain {chain} still running");
        assert_eq!(runner.starts(chain), 1);
    }
}

#[tokio::test]
async fn the_events_of_a_chain_read_like_a_story() {
    let (supervisor, runner, _) = fleet(&[1]);
    runner.behave(1, Behaviour::Fails);
    supervisor.load_and_start().await.unwrap();

    until("it failed", || {
        supervisor
            .events(1)
            .is_some_and(|events| events.iter().any(|e| e.kind == "error"))
    })
    .await;

    let events = supervisor.events(1).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind).collect();

    assert!(kinds.contains(&"error"), "{kinds:?}");
    assert!(kinds.contains(&"state"), "{kinds:?}");
    assert!(
        events.iter().all(|e| e.at_unix_ms > 0),
        "every event is stamped"
    );
    assert!(events
        .iter()
        .any(|e| e.message.contains("source is unreachable")));

    supervisor.shutdown().await;
}

#[tokio::test]
async fn the_state_the_panel_prints_is_one_of_the_six_design_15_names() {
    let known: Vec<&str> = [
        ChainState::Starting,
        ChainState::Backfilling,
        ChainState::Following,
        ChainState::Stopped,
        ChainState::Failed,
        ChainState::RunningElsewhere,
    ]
    .iter()
    .map(|state| state.as_str())
    .collect();

    let (supervisor, _, store) = fleet(&[1]);
    store.set_foreign(vec![ForeignChain {
        chain: 137,
        host: "elsewhere".to_string(),
        heartbeat_ms: 1,
    }]);
    supervisor.load_and_start().await.unwrap();

    for view in supervisor.views().await {
        assert!(known.contains(&view.live.state), "{}", view.live.state);
        assert!(!view.live.state_text.is_empty());
    }

    supervisor.shutdown().await;
}
