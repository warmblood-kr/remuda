use remuda_core::protocol::{Request, Response};
use remuda_core::Size;
use remuda_native::daemon;
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
    let command = format!(
        "sleep 0.2; {} -s s -e \"remuda.extension_command('probe', function() local c=remuda.caller(); local f=assert(io.open('{}','w')); f:write(tostring(c.session), ':', tostring(c.inside)); f:close() end); return remuda._dispatch_extension_command('probe')\"",
        env!("CARGO_BIN_EXE_remuda"),
        result_path.display()
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
    assert_eq!(result, "caller-probe:true");
}
