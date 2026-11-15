//! Bounded, single-flight subprocess bridge for instance-bound input.
//! Selection/focus currentness is the caller's responsibility.
use remuda_native::strict_input::{
    InputResult, InputTransport, LocalInput, PeerInput, StrictInput, MAX_BATCH,
};
use std::{
    io::{Read, Write},
    sync::mpsc,
    time::Duration,
};

const DEADLINE: Duration = Duration::from_secs(8);

// One watchdog bounds startup and any partially read frame or active write.
// An idle helper waits for a first byte and exits on parent pipe closure.
fn watchdog() -> mpsc::Sender<bool> {
    let (watch, deadlines) = mpsc::channel::<bool>();
    std::thread::spawn(move || loop {
        match deadlines.recv() {
            Ok(true) => {
                if deadlines.recv_timeout(DEADLINE).is_err() {
                    std::process::exit(124);
                }
            }
            _ => return,
        }
    });
    watch
}
fn serve<T: InputTransport>(mut input: StrictInput<T>, watch: mpsc::Sender<bool>) -> i32 {
    let _ = watch.send(false);
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    loop {
        let mut header = [0; 4];
        if stdin.read_exact(&mut header[..1]).is_err() {
            return 0;
        }
        if watch.send(true).is_err() {
            return 1;
        }
        if stdin.read_exact(&mut header[1..]).is_err() {
            return 1;
        }
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 || length > MAX_BATCH {
            return 1;
        }
        let mut bytes = vec![0; length];
        if stdin.read_exact(&mut bytes).is_err() {
            return 1;
        }
        let result = input.send(&bytes);
        let ack = u8::from(result == InputResult::Applied);
        if stdout
            .write_all(&[ack])
            .and_then(|()| stdout.flush())
            .is_err()
            || ack == 0
        {
            return 1;
        }
        let _ = watch.send(false);
    }
}

pub fn worker(args: &[String]) -> i32 {
    if args.len() != 4 || args[2].is_empty() || args[3].is_empty() {
        return 1;
    }
    let watch = watchdog();
    let _ = watch.send(true);
    let name = args[2].clone();
    let instance = args[3].clone();
    match args[0].as_str() {
        "local" if args[1].is_empty() => StrictInput::new(
            LocalInput(remuda_native::daemon::socket_path("default")),
            name,
            instance,
        )
        .map_or(1, |input| serve(input, watch)),
        "peer" if !args[1].is_empty() => PeerInput::new(args[1].clone())
            .and_then(|peer| StrictInput::new(peer, name, instance))
            .map_or(1, |input| serve(input, watch)),
        _ => 1,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;
    #[test]
    #[ignore = "child fixture for watchdog"]
    fn stalled_transport_fixture() {
        struct Stall;
        impl InputTransport for Stall {
            fn send(
                &mut self,
                _: &remuda_core::protocol::Request,
            ) -> std::io::Result<remuda_core::protocol::Response> {
                std::thread::sleep(Duration::from_secs(30));
                unreachable!()
            }
        }
        let sender = StrictInput::new(Stall, "s".into(), "i".into()).unwrap();
        let watch = watchdog();
        let _ = watch.send(true);
        std::process::exit(serve(sender, watch));
    }
    #[test]
    #[ignore = "child fixture for startup deadline"]
    fn stalled_startup_fixture() {
        let watch = watchdog();
        watch.send(true).unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }
    fn watchdog_child(fixture: &str, partial: bool) {
        use std::process::{Command, Stdio};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", fixture])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        if partial {
            input.write_all(&[0]).unwrap();
            input.flush().unwrap();
        }
        let deadline = Instant::now() + DEADLINE + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("watchdog did not terminate helper");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(124));
    }
    #[test]
    fn startup_deadline_exits() {
        watchdog_child("input_helper::tests::stalled_startup_fixture", false);
    }
    #[test]
    fn partial_frame_deadline_exits() {
        watchdog_child("input_helper::tests::stalled_transport_fixture", true);
    }
    #[test]
    fn helper_watchdog_exits_after_parent_pipe_closes_during_stall() {
        use std::process::{Command, Stdio};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "input_helper::tests::stalled_transport_fixture",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(&1u32.to_be_bytes()).unwrap();
        input.write_all(b"x").unwrap();
        drop(input);
        let deadline = Instant::now() + DEADLINE + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("watchdog did not terminate helper");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(124));
    }
}
