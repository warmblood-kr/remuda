use remuda_core::{ManualWallClock, WallClock};
use std::time::Duration;

#[test]
fn manual_wall_clock_supports_unix_time_advance_and_rollback() {
    let clock = ManualWallClock::new(1_700_000_000);
    assert_eq!(clock.unix_seconds(), 1_700_000_000);
    clock.advance(Duration::from_secs(17));
    assert_eq!(clock.unix_seconds(), 1_700_000_017);
    clock.set_unix_seconds(1_600_000_000);
    assert_eq!(clock.unix_seconds(), 1_600_000_000);
}
