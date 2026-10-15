//! Fault injection verifies that unknown kernel state never becomes an exit.
use super::*;
use std::cell::Cell;
struct Fake {
    wait: io::Result<bool>,
    code: io::Result<u32>,
    terminate: io::Result<()>,
    queries: Cell<usize>,
}
fn copy<T: Copy>(value: &io::Result<T>) -> io::Result<T> {
    value.as_ref().copied().map_err(|error| {
        io::Error::from_raw_os_error(error.raw_os_error().expect("injected OS error"))
    })
}
impl Api for Fake {
    fn wait(&self, _: HANDLE, _: Duration) -> io::Result<bool> {
        copy(&self.wait)
    }
    fn code(&self, _: HANDLE) -> io::Result<u32> {
        self.queries.set(self.queries.get() + 1);
        copy(&self.code)
    }
    fn terminate(&self, _: HANDLE) -> io::Result<()> {
        copy(&self.terminate)
    }
}
fn fake(wait: io::Result<bool>, code: io::Result<u32>, terminate: io::Result<()>) -> Fake {
    Fake {
        wait,
        code,
        terminate,
        queries: Cell::new(0),
    }
}
#[test]
fn only_a_signaled_wait_allows_exit_query_and_exit259_is_final() {
    let pending = fake(Ok(false), Ok(259), Ok(()));
    assert_eq!(
        observe(&pending, std::ptr::null_mut(), Duration::ZERO).unwrap(),
        None
    );
    assert_eq!(pending.queries.get(), 0);
    let failed = fake(Err(io::Error::from_raw_os_error(6)), Ok(7), Ok(()));
    assert_eq!(
        observe(&failed, std::ptr::null_mut(), Duration::ZERO)
            .unwrap_err()
            .raw_os_error(),
        Some(6)
    );
    assert_eq!(failed.queries.get(), 0);
    for code in [0, 7, 259, u32::MAX] {
        let exited = fake(Ok(true), Ok(code), Ok(()));
        assert_eq!(
            observe(&exited, std::ptr::null_mut(), Duration::ZERO).unwrap(),
            Some(code)
        );
    }
    let failed_query = fake(Ok(true), Err(io::Error::from_raw_os_error(5)), Ok(()));
    assert_eq!(
        observe(&failed_query, std::ptr::null_mut(), Duration::ZERO)
            .unwrap_err()
            .raw_os_error(),
        Some(5)
    );
}
#[test]
fn failed_termination_requires_observed_natural_exit_not_synthetic_success() {
    let denied = fake(Ok(false), Ok(0), Err(io::Error::from_raw_os_error(5)));
    assert_eq!(
        terminate(&denied, std::ptr::null_mut(), Duration::ZERO)
            .unwrap_err()
            .raw_os_error(),
        Some(5)
    );
    assert_eq!(denied.queries.get(), 0);
    let raced_exit = fake(Ok(true), Ok(259), Err(io::Error::from_raw_os_error(5)));
    assert_eq!(
        terminate(&raced_exit, std::ptr::null_mut(), Duration::ZERO).unwrap(),
        Some(259)
    );
    let initiated = fake(Ok(false), Ok(0), Ok(()));
    assert_eq!(
        terminate(&initiated, std::ptr::null_mut(), Duration::ZERO).unwrap(),
        None
    );
    let unknown = fake(Err(io::Error::from_raw_os_error(6)), Ok(0), Ok(()));
    assert_eq!(
        terminate(&unknown, std::ptr::null_mut(), Duration::ZERO)
            .unwrap_err()
            .raw_os_error(),
        Some(6)
    );
}
#[test]
fn native_invalid_handles_surface_wait_query_and_termination_errors() {
    assert!(Native.wait(std::ptr::null_mut(), Duration::ZERO).is_err());
    assert!(Native.code(std::ptr::null_mut()).is_err());
    assert!(Native.terminate(std::ptr::null_mut()).is_err());
}
