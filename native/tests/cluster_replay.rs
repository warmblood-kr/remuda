#[path = "../src/net/replay.rs"]
mod replay;

#[test]
fn replay_accepts_inclusive_sixty_second_boundaries() {
    let mut window = replay::ReplayWindow::new(8);
    assert!(window.check_and_insert([1; 32], 940, 1000).is_ok());
    assert!(window.check_and_insert([2; 32], 1060, 1000).is_ok());
}

#[test]
fn replay_rejects_old_new_and_replayed_ephemerals() {
    let mut window = replay::ReplayWindow::new(8);
    assert!(matches!(
        window.check_and_insert([1; 32], 939, 1000),
        Err(replay::ReplayError::OutsideWindow)
    ));
    assert!(matches!(
        window.check_and_insert([2; 32], 1061, 1000),
        Err(replay::ReplayError::OutsideWindow)
    ));
    window.check_and_insert([3; 32], 1000, 1000).unwrap();
    assert!(matches!(
        window.check_and_insert([3; 32], 1000, 1000),
        Err(replay::ReplayError::AlreadySeen)
    ));
}

#[test]
fn replayed_close_frame_is_refused_before_dispatch() {
    let mut window = replay::ReplayWindow::new(8);
    let close_ephemeral = [9; 32];
    window
        .check_and_insert(close_ephemeral, 1000, 1000)
        .unwrap();
    assert!(matches!(
        window.check_and_insert(close_ephemeral, 1000, 1000),
        Err(replay::ReplayError::AlreadySeen)
    ));
}

#[test]
fn replay_prunes_by_timestamp_and_enforces_capacity() {
    let mut window = replay::ReplayWindow::new(1);
    window.check_and_insert([1; 32], 1000, 1000).unwrap();
    assert!(matches!(
        window.check_and_insert([2; 32], 1001, 1001),
        Err(replay::ReplayError::Capacity)
    ));
    window.check_and_insert([2; 32], 1062, 1062).unwrap();
}
