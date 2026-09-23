use seshat::key_pool::{CombinedKeyPool, FailureClass, KeyPool, PoolError, SearchProvider};
use std::time::Duration;

#[test]
fn key_input_trims_blanks_and_deduplicates_in_order() {
    let pool = KeyPool::from_sources(
        Some(" alpha\n\n beta\nalpha "),
        Some("fallback"),
        "firecrawl",
    )
    .expect("pool should load");

    assert_eq!(pool.len(), 2);
    assert_eq!(
        pool.candidates().iter().map(|c| c.slot).collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[test]
fn file_source_wins_over_environment_source() {
    let path = std::env::temp_dir().join(format!("seshat-key-pool-{}", std::process::id()));
    std::fs::write(&path, "file-key\n").expect("test key file should be writable");
    let pool = KeyPool::from_file_or_env(path.to_str(), Some("env-key"), "firecrawl")
        .expect("pool should load");
    std::fs::remove_file(path).expect("test key file should be removable");

    assert_eq!(pool.len(), 1);
    assert_eq!(pool.candidates()[0].secret(), "file-key");
}

#[test]
fn round_robin_advances_the_starting_slot() {
    let pool = KeyPool::from_keys(["a", "b", "c"], "firecrawl").expect("pool should load");

    let first = pool.candidates();
    let second = pool.candidates();
    let third = pool.candidates();

    assert_eq!(
        first.iter().map(|c| c.slot).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(
        second.iter().map(|c| c.slot).collect::<Vec<_>>(),
        vec![1, 2, 0]
    );
    assert_eq!(
        third.iter().map(|c| c.slot).collect::<Vec<_>>(),
        vec![2, 0, 1]
    );
}

#[test]
fn retryable_failures_advance_and_caller_errors_do_not_rotate() {
    let pool = KeyPool::from_keys(["a", "b"], "firecrawl").expect("pool should load");

    assert!(FailureClass::from_status(408).is_retryable());
    assert!(FailureClass::from_status(425).is_retryable());
    assert!(FailureClass::from_status(429).is_retryable());
    assert!(FailureClass::from_status(503).is_retryable());
    assert!(FailureClass::from_status(401).is_retryable());
    assert!(FailureClass::from_status(403).is_retryable());
    assert!(!FailureClass::from_status(400).is_retryable());
    assert!(!FailureClass::from_status(422).is_retryable());

    pool.mark_failure(0, FailureClass::Status(503));
    assert_eq!(
        pool.candidates().iter().map(|c| c.slot).collect::<Vec<_>>(),
        vec![1]
    );

    pool.mark_failure(1, FailureClass::Status(400));
    assert_eq!(
        pool.candidates().iter().map(|c| c.slot).collect::<Vec<_>>(),
        vec![1]
    );
}

#[test]
fn cooldown_excludes_then_re_admits_a_failed_key() {
    let pool = KeyPool::with_policy(
        ["a"],
        "firecrawl",
        Duration::from_millis(10),
        Duration::from_millis(10),
    )
    .expect("pool should load");

    pool.mark_failure(0, FailureClass::Timeout);
    assert!(pool.candidates().is_empty());
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(
        pool.candidates().iter().map(|c| c.slot).collect::<Vec<_>>(),
        vec![0]
    );
}

#[test]
fn empty_pool_is_rejected_without_echoing_key_material() {
    let error = KeyPool::from_sources(Some(" \n"), Some("\n"), "firecrawl")
        .expect_err("empty pool must fail");

    assert!(matches!(error, PoolError::Empty { .. }));
    assert!(!error.to_string().contains("firecrawl-key"));
}

#[test]
fn all_candidates_are_bounded_to_one_attempt_per_request() {
    let pool = KeyPool::from_keys(["a", "b", "c"], "firecrawl").expect("pool should load");
    let candidates = pool.candidates();

    assert_eq!(candidates.len(), pool.len());
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.slot)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

#[test]
fn concurrent_selection_keeps_each_snapshot_bounded() {
    let pool = KeyPool::from_keys(["a", "b", "c"], "firecrawl").expect("pool should load");
    let handles = (0..32)
        .map(|_| {
            let pool = pool.clone();
            std::thread::spawn(move || pool.candidates())
        })
        .collect::<Vec<_>>();

    for handle in handles {
        let candidates = handle.join().expect("selection should not panic");
        assert!(candidates.len() <= 3);
        let mut slots = candidates
            .iter()
            .map(|candidate| candidate.slot)
            .collect::<Vec<_>>();
        slots.sort_unstable();
        slots.dedup();
        assert_eq!(slots.len(), candidates.len());
    }
}

#[test]
fn combined_pool_rotates_all_provider_keys_in_one_global_sequence() {
    let brave = KeyPool::from_keys(["brave-a", "brave-b", "brave-c"], "brave")
        .expect("Brave pool should load");
    let tavily = KeyPool::from_keys(["tavily-a", "tavily-b", "tavily-c", "tavily-d"], "tavily")
        .expect("Tavily pool should load");
    let pool = CombinedKeyPool::from_pools(&brave, &tavily);

    let selected = (0..7)
        .map(|_| {
            let candidates = pool.candidates();
            assert_eq!(candidates.len(), 7);
            let candidate = candidates
                .first()
                .expect("one candidate should be eligible");
            (candidate.provider(), candidate.slot)
        })
        .collect::<Vec<_>>();

    assert_eq!(
        selected,
        vec![
            (SearchProvider::Brave, 0),
            (SearchProvider::Brave, 1),
            (SearchProvider::Brave, 2),
            (SearchProvider::Tavily, 3),
            (SearchProvider::Tavily, 4),
            (SearchProvider::Tavily, 5),
            (SearchProvider::Tavily, 6),
        ]
    );
}

#[test]
fn combined_pool_cooldown_skips_a_pair_and_re_admits_it() {
    let brave = KeyPool::from_keys(["brave-a"], "brave").expect("Brave pool should load");
    let tavily = KeyPool::from_keys(["tavily-a"], "tavily").expect("Tavily pool should load");
    let pool = CombinedKeyPool::with_policy(
        &brave,
        &tavily,
        Duration::from_millis(10),
        Duration::from_millis(10),
    );

    pool.mark_failure(0, FailureClass::Timeout);
    let next = pool.candidates();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].provider(), SearchProvider::Tavily);

    std::thread::sleep(Duration::from_millis(20));
    let re_admitted = pool.candidates();
    assert_eq!(re_admitted.len(), 2);
}
