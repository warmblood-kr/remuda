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
    // Lua accepts forward slashes on Windows; raw `Path::display()` backslashes
    // become escape sequences in the Lua string passed through the shell.
    let result_path_lua = result_path.to_string_lossy().replace('\\', "/");
    let lua = format!(
        "remuda.caller=function() return {{kind='outside'}} end; remuda.extension_command('probe', function(_, caller) local f=assert(io.open('{}','w')); f:write(tostring(caller.session), ':', caller.kind, ':', tostring(caller.instance_id)); f:close() end); return remuda._dispatch_extension_command('probe')",
        result_path_lua
    );
    let remuda = env!("CARGO_BIN_EXE_remuda").replace('\\', "/");
    let command = format!("sleep 0.2; \"{remuda}\" -s s -e \"{lua}\"", lua = lua);
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
    let mut fields = result.split(':');
    assert_eq!(fields.next(), Some("caller-probe"));
    assert_eq!(fields.next(), Some("session"));
    assert!(fields
        .next()
        .is_some_and(|id| !id.is_empty() && id != "nil"));
}

#[test]
fn live_mcp_bridge_inside_a_session_receives_native_instance_snapshot() {
    let runtime = std::env::temp_dir().join(format!("r-caller-mcp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::create_dir_all(&runtime).expect("create runtime");
    let socket = daemon::socket_path_in(&runtime, "s");
    let _daemon = spawn::Daemon::spawn(&runtime);
    let response = remuda_native::client::request(
        &socket,
        &Request::Eval {
            code: r#"remuda.tool{ name="who_called", about="Return the daemon captured caller snapshot fields.", run=function(_, caller) return table.concat({tostring(caller.capability), tostring(caller.kind), tostring(caller.session), tostring(caller.instance_id)}, ":") end }"#.into(),
            name: None,
        },
    )
    .expect("register probe tool");
    assert!(matches!(response, Response::Value(_)), "{response:?}");

    let output = runtime.join("mcp.json");
    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "who_called", "arguments": {}}
    })
    .to_string();
    let remuda = env!("CARGO_BIN_EXE_remuda").replace('\\', "/");
    let output_arg = output.to_string_lossy().replace('\\', "/");
    let command =
        format!("printf '%s\\n' '{request}' | \"{remuda}\" -s s mcp > '{output_arg}'; sleep 1");
    let response = remuda_native::client::request(
        &socket,
        &Request::New {
            name: Some("mcp-caller".into()),
            command: vec!["sh".into(), "-c".into(), command],
            size: Size::new(80, 24),
            cwd: None,
            env: Some(std::collections::HashMap::from([(
                "REMUDA_SESSION_CAPABILITY".into(),
                "bridge-capability".into(),
            )])),
        },
    )
    .expect("start MCP bridge in session");
    assert_eq!(response, Response::Value("mcp-caller".into()));

    let deadline = Instant::now() + Duration::from_secs(8);
    let value = loop {
        if let Ok(contents) = std::fs::read_to_string(&output) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) {
                break value;
            }
        }
        assert!(
            Instant::now() < deadline,
            "session MCP bridge did not answer"
        );
        thread::sleep(Duration::from_millis(20));
    };
    let caller = value["result"]["content"][0]["text"]
        .as_str()
        .expect("tool text");
    let mut fields = caller.split(':');
    assert_eq!(fields.next(), Some("bridge-capability"));
    assert_eq!(fields.next(), Some("session"));
    assert_eq!(fields.next(), Some("mcp-caller"));
    let caller_instance = fields.next().expect("caller instance id");
    assert!(!caller_instance.is_empty() && caller_instance != "nil");
    let sessions = match remuda_native::client::request(&socket, &Request::List)
        .expect("list session instance")
    {
        Response::Sessions(sessions) => sessions,
        response => panic!("unexpected list response: {response:?}"),
    };
    let listed_instance = sessions
        .iter()
        .find(|session| session.name == "mcp-caller")
        .and_then(|session| session.instance_id.as_deref())
        .expect("listed caller instance id");
    assert_eq!(caller_instance, listed_instance);
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
        image
            .eval(
                "return remuda.caller().kind .. ':' .. tostring(remuda.caller().instance_id)",
                None
            )
            .unwrap(),
        "unknown:nil"
    );
    image
        .eval(
            "remuda.schedule({every = 1, run = function() scheduled_caller_kind = remuda.caller().kind .. ':' .. tostring(remuda.caller().instance_id) end})",
            None,
        )
        .unwrap();
    image
        .eval("remuda._run_due_schedules(10.0)", None)
        .expect("run due schedule");
    assert_eq!(
        image.eval("return scheduled_caller_kind", None).unwrap(),
        "unknown:nil"
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
            code: "return remuda.pending({timeout = 0.1, on_cancel = function() pending_caller_kind = remuda.caller().kind .. ':' .. tostring(remuda.caller().instance_id) end})".into(),
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
        if response == Response::Value("unknown:nil".into()) {
            break;
        }
        assert!(Instant::now() < deadline, "pending callback did not run");
        thread::sleep(Duration::from_millis(25));
    }
}
