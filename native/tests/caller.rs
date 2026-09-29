use remuda_core::protocol::{Request, Response};
use remuda_core::Registry;
use remuda_core::Size;
use remuda_native::{daemon, image::Image, tick::Counters};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[path = "daemon_support/spawn.rs"]
mod spawn;

#[test]
fn caller_identifies_a_client_running_inside_a_managed_session() {
    let runtime = std::env::temp_dir().join(format!("r-caller-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::create_dir_all(&runtime).expect("create runtime");
    let socket = daemon::socket_path_in(&runtime, "s");
    let _daemon = spawn::Daemon::spawn(&runtime);
    let result_path = runtime.join("caller.txt");
    let lua = format!(
        "remuda.caller=function() return {{kind='outside'}} end; remuda.extension_command('probe', function(_, caller) local f=assert(io.open('{}','w')); f:write(tostring(caller.session), ':', caller.kind); f:close() end); return remuda._dispatch_extension_command('probe')",
        result_path.display()
    );
    let command = format!(
        "sleep 0.2; {} -s s -e \"{lua}\"",
        env!("CARGO_BIN_EXE_remuda"),
        lua = lua
    );
    assert_eq!(
        remuda_native::client::request(
            &socket,
            &Request::New {
                name: Some("caller-probe".into()),
                command: vec!["sh".into(), "-c".into(), command],
                size: Size::new(80, 24),
                cwd: None,
                env: None,
            },
        )
        .expect("start caller probe session"),
        Response::Value("caller-probe".into())
    );

    let deadline = Instant::now() + Duration::from_secs(8);
    let result = loop {
        if let Ok(contents) = std::fs::read_to_string(&result_path) {
            break contents;
        }
        assert!(
            Instant::now() < deadline,
            "session client did not write caller info"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(result, "caller-probe:session");
}

#[test]
fn in_process_evaluations_and_schedules_have_unknown_callers() {
    let socket = std::env::temp_dir().join(format!(
        "remuda-caller-image-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos()
    ));
    let image = Image::spawn(
        &socket,
        Arc::new(Registry::new()),
        Arc::new(Counters::default()),
    );
    assert_eq!(
        image.eval("return remuda.caller().kind", None).unwrap(),
        "unknown"
    );
    image
        .eval(
            "remuda.schedule({every = 1, run = function() scheduled_caller_kind = remuda.caller().kind end})",
            None,
        )
        .unwrap();
    image
        .eval("remuda._run_due_schedules(10.0)", None)
        .expect("run due schedule");
    assert_eq!(
        image.eval("return scheduled_caller_kind", None).unwrap(),
        "unknown"
    );
}

#[test]
fn pending_cancellation_callback_has_unknown_caller() {
    let runtime = std::env::temp_dir().join(format!("r-caller-pending-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::create_dir_all(&runtime).expect("create runtime");
    let socket = daemon::socket_path_in(&runtime, "s");
    let _daemon = spawn::Daemon::spawn(&runtime);

    let response = remuda_native::client::request(
        &socket,
        &Request::Eval {
            code: "return remuda.pending({timeout = 0.1, on_cancel = function() pending_caller_kind = remuda.caller().kind end})".into(),
            name: None,
        },
    )
    .expect("pending request response");
    assert!(matches!(response, Response::Error(_)));

    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let response = remuda_native::client::request(
            &socket,
            &Request::Eval {
                code: "return pending_caller_kind or 'waiting'".into(),
                name: None,
            },
        )
        .expect("read cancellation callback result");
        if response == Response::Value("unknown".into()) {
            break;
        }
        assert!(Instant::now() < deadline, "pending callback did not run");
        thread::sleep(Duration::from_millis(25));
    }
}
