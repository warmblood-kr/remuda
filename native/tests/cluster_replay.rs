#[path = "../src/net/replay.rs"]
mod replay;

#[test]
fn replay_accepts_inclusive_sixty_second_boundaries() {
    let mut window = replay::ReplayWindow::new(8);
    assert!(window.check_and_insert("peer", [1; 32], 940, 1000).is_ok());
    assert!(window.check_and_insert("peer", [2; 32], 1060, 1000).is_ok());
}

#[test]
fn replay_rejects_old_new_and_replayed_ephemerals() {
    let mut window = replay::ReplayWindow::new(8);
    assert!(matches!(
        window.check_and_insert("peer", [1; 32], 939, 1000),
        Err(replay::ReplayError::OutsideWindow)
    ));
    assert!(matches!(
        window.check_and_insert("peer", [2; 32], 1061, 1000),
        Err(replay::ReplayError::OutsideWindow)
    ));
    window
        .check_and_insert("peer", [3; 32], 1000, 1000)
        .unwrap();
    assert!(matches!(
        window.check_and_insert("peer", [3; 32], 1000, 1000),
        Err(replay::ReplayError::AlreadySeen)
    ));
}

#[test]
fn replayed_close_frame_is_refused_before_dispatch() {
    let mut window = replay::ReplayWindow::new(8);
    let close_ephemeral = [9; 32];
    window
        .check_and_insert("peer", close_ephemeral, 1000, 1000)
        .unwrap();
    assert!(matches!(
        window.check_and_insert("peer", close_ephemeral, 1000, 1000),
        Err(replay::ReplayError::AlreadySeen)
    ));
}

#[test]
fn replay_prunes_by_timestamp_and_enforces_capacity() {
    let mut window = replay::ReplayWindow::new(1);
    let monotonic = std::time::Instant::now();
    window
        .check_and_insert_at("peer", [1; 32], 1000, 1000, monotonic)
        .unwrap();
    assert!(matches!(
        window.check_and_insert_at(
            "peer",
            [2; 32],
            1001,
            1001,
            monotonic + std::time::Duration::from_secs(1)
        ),
        Err(replay::ReplayError::Capacity)
    ));
    window
        .check_and_insert_at(
            "peer",
            [2; 32],
            1062,
            1062,
            monotonic + std::time::Duration::from_secs(120),
        )
        .unwrap();
}

#[test]
fn replay_peer_cannot_consume_the_global_cache_share() {
    let mut window = replay::ReplayWindow::with_peer_capacity(16, 1);
    let monotonic = std::time::Instant::now();
    window
        .check_and_insert_at("peer-a", [11; 32], 1000, 1000, monotonic)
        .unwrap();
    assert!(matches!(
        window.check_and_insert_at("peer-a", [12; 32], 1000, 1000, monotonic),
        Err(replay::ReplayError::PeerCapacity)
    ));
    assert!(window
        .check_and_insert_at("peer-b", [12; 32], 1000, 1000, monotonic)
        .is_ok());
}

#[test]
fn replay_eviction_uses_monotonic_age_across_wall_clock_jumps() {
    let mut window = replay::ReplayWindow::new(8);
    let monotonic = std::time::Instant::now();
    window
        .check_and_insert_at("peer", [11; 32], 1000, 1000, monotonic)
        .unwrap();
    window
        .check_and_insert_at(
            "peer",
            [12; 32],
            1100,
            1100,
            monotonic + std::time::Duration::from_secs(1),
        )
        .unwrap();
    assert!(matches!(
        window.check_and_insert_at(
            "peer",
            [11; 32],
            1000,
            1000,
            monotonic + std::time::Duration::from_secs(2)
        ),
        Err(replay::ReplayError::AlreadySeen)
    ));
}

#[test]
fn replay_with_future_skew_remains_cached_at_the_timestamp_window_edge() {
    let mut window = replay::ReplayWindow::new(8);
    let monotonic = std::time::Instant::now();
    window
        .check_and_insert_at("peer", [7; 32], 1060, 1000, monotonic)
        .unwrap();
    assert!(matches!(
        window.check_and_insert_at(
            "peer",
            [7; 32],
            1060,
            1120,
            monotonic + std::time::Duration::from_secs(120)
        ),
        Err(replay::ReplayError::AlreadySeen)
    ));
}
