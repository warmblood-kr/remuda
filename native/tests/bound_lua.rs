use remuda_core::protocol::{Request, Response};
use remuda_native::{client, daemon};
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

struct TempRoot(PathBuf);
impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn an_infinite_mod_tool_does_not_wedge_the_daemon() {
    let root = std::env::temp_dir().join(format!("rbl-{}", std::process::id()));
    let _cleanup = TempRoot(root.clone());
    let runtime = root.clone();
    let data = root.join("data");
    let package = data.join("remuda/mods/looping/packages/looping");
    fs::create_dir_all(&package).expect("private package directory");
    fs::write(
        data.join("remuda/mods/looping/extension.toml"),
        "name = \"looping\"\nentry = \"packages/looping/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .expect("manifest");
    fs::write(
        package.join("init.lua"),
        r#"return { api = "remuda-module-v1", state_version = 1,
          initialize = function() return {} end,
          tools = {{ name = "loop_forever", about = "Hang forever.", run = function()
            while true do end
          end }} }"#,
    )
    .expect("mod entry");

    let config = root.join("config");
    let home = root.join("home");
    fs::create_dir_all(&config).expect("private config");
    fs::create_dir_all(&home).expect("private home");
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", &config);
    std::env::set_var("XDG_DATA_HOME", &data);
    std::env::set_var("REMUDA_RUNTIME_DIR", &runtime);

    let socket = daemon::socket_path_in(&runtime, "s");
    let serving_path = socket.clone();
    let serving_runtime = runtime.clone();
    std::thread::spawn(move || {
        if let Err(error) = daemon::serve_with_runtime(&serving_path, &serving_runtime) {
            eprintln!("private daemon startup failed: {error}");
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while remuda_native::ipc::connect(&socket).is_err() {
        assert!(std::time::Instant::now() < deadline, "private daemon never bound {socket:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
    let loaded = client::request(
        &socket,
        &Request::Eval { code: "remuda.exec('looping')".into(), name: None },
    )
    .expect("load looping mod");
    assert!(matches!(loaded, Response::Value(_)), "mod did not load: {loaded:?}");

    let (started_tx, started_rx) = mpsc::channel();
    let looping_socket = socket.clone();
    std::thread::spawn(move || {
        let _ = started_tx.send(());
        let _ = client::request(
            &looping_socket,
            &Request::Eval { code: "remuda.tools.loop_forever()".into(), name: None },
        );
    });
    started_rx.recv().expect("start loop request");
    std::thread::sleep(Duration::from_millis(200));

    let (reply_tx, reply_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = reply_tx.send(client::request(&socket, &Request::Version));
    });
    let answer = reply_rx.recv_timeout(Duration::from_secs(3));
    assert!(
        matches!(answer, Ok(Ok(Response::Value(_))) | Ok(Ok(Response::Error(_)))),
        "daemon did not answer while the mod callback looped: {answer:?}"
    );

    // The daemon is isolated to this test process and its private socket.
}
