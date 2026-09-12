//! The Lua runtime: what it can reach, and what it does when refused.
//!
//! 정수님, 2026-09-10: *"programming runtime을 심어서 코드를 실행할 수 있게 만들고
//! atomic function들을 물려서 연결합니다."* The three claims worth exercising are
//! that a script can *react* to a screen (which the shell could not), that the
//! bound surface is exactly the protocol's (embedding a language must not widen
//! the API), and that a refusal **stops** the script rather than being returned
//! for it to ignore.

use remuda_core::protocol::{Request, Response};
use remuda_native::{client, daemon, script};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(10);

/// A directory of our own, short enough for `sun_path` (~108 bytes).
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remuda-s{}-{tag}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Start a daemon and return once it actually answers, not once it was spawned.
fn daemon_at(path: &Path) -> impl Drop {
    let serving = path.to_path_buf();
    std::thread::spawn(move || {
        let _ = daemon::serve(&serving);
    });
    let deadline = Instant::now() + PATIENCE;
    while remuda_native::ipc::connect(path).is_err() {
        assert!(Instant::now() < deadline, "daemon never bound {path:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
    Cleanup(path.to_path_buf())
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn write(dir: &Path, name: &str, source: &str) -> PathBuf {
    let file = dir.join(name);
    std::fs::write(&file, source).expect("write script");
    file
}

fn capture(path: &Path, name: &str) -> String {
    match client::request(
        path,
        &Request::Capture {
            name: name.to_string(),
        },
    ) {
        Ok(Response::Screen(text)) => text,
        other => panic!("capture failed: {other:?}"),
    }
}

#[test]
fn the_bound_surface_is_exactly_the_protocols() {
    // The property this guards is the reason embedding Lua is safe at all: the
    // script's vocabulary is `Request` and nothing else. The check runs in both
    // directions — a binding added without a decision fails, and a name listed
    // in BINDINGS but never bound fails too.
    let dir = scratch("surface");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path);

    let expected = script::BINDINGS.join(",");
    let source = format!(
        r#"
        local names = {{}}
        for key in pairs(remuda) do names[#names + 1] = key end
        table.sort(names)
        local got = table.concat(names, ",")
        local want = "{expected}"
        if got ~= want then
          error("bound surface is " .. got .. ", expected " .. want)
        end
        "#
    );

    script::run(&path, &write(&dir, "surface.lua", &source)).expect("surface");

    // Negative control: the same script with one name removed from the
    // expectation must fail, so a pass above cannot come from the comparison
    // never running.
    let short = script::BINDINGS[1..].join(",");
    let source = source.replace(&expected, &short);
    let err = script::run(&path, &write(&dir, "control.lua", &source))
        .expect_err("a wrong expectation must fail");
    assert!(
        err.to_string().contains("bound surface is"),
        "failed for the wrong reason: {err}"
    );
}

#[test]
fn every_frozen_api_version_still_runs() {
    // 정수님, 2026-09-10: *"그 언어 API 에 대고 사용자들이 자기 함수를 얹어서
    // 설정하거나 플러그인, 워크플로 등을 만들면, 하위호환을 엄격하게 지켜야
    // 합니다."*
    //
    // `tests/api/vN.lua` is a script written against version N of the Lua API,
    // and it is frozen: widening the surface means adding `v2.lua`, never
    // editing `v1.lua`. That is what makes this a control rather than a
    // ceremony — a check you may edit to make it pass checks nothing. Renaming
    // a binding, reordering its arguments, or making an optional argument
    // required now fails the build instead of someone's plugin.
    let dir = scratch("compat");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path);

    let api = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/api");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&api)
        .expect("tests/api must exist")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "lua"))
        .collect();
    versions.sort();

    // Absence here would look exactly like success: an empty directory runs
    // zero scripts and passes. The count is asserted, so a lost or unreadable
    // file is a failure rather than a silent skip.
    assert!(
        !versions.is_empty(),
        "no frozen API scripts found in {api:?} — the compatibility gate is not running"
    );

    for version in versions {
        script::run(&path, &version)
            .unwrap_or_else(|e| panic!("{} no longer runs: {e}", version.display()));
    }
}

#[test]
fn a_script_reacts_to_what_a_session_shows() {
    // The whole point of the layer: send, wait, look, decide, send again. The
    // shell could express each step and not the loop between them.
    //
    // Arithmetic rather than a literal, per PRINCIPLES.md §4 — a pty echoes its
    // input, so waiting for a string that appears in the command proves only
    // that the echo happened.
    let dir = scratch("react");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path);

    let source = r#"
        remuda.new("driven", {"sh"})
        remuda.send("driven", "echo $((6*7))-first")

        local deadline = 500
        while deadline > 0 and not remuda.capture("driven"):find("42%-first") do
          remuda.sleep(0.02)
          deadline = deadline - 1
        end
        if deadline == 0 then error("the first answer never appeared") end

        -- The branch is the part shell cannot do: what is sent next depends on
        -- what came back.
        if remuda.capture("driven"):find("42%-first") then
          remuda.send("driven", "echo $((11*11))-second")
        else
          remuda.send("driven", "echo WRONG-BRANCH")
        end
    "#;

    script::run(&path, &write(&dir, "react.lua", source)).expect("script");

    let deadline = Instant::now() + PATIENCE;
    loop {
        let screen = capture(&path, "driven");
        if screen.contains("121-second") {
            assert!(
                !screen.contains("WRONG-BRANCH"),
                "the script took the branch it should not have:\n{screen}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the second command never ran. screen:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_refusal_stops_the_script_instead_of_being_returned() {
    // The failure this rules out is the shell one: `$(remuda -e 'remuda.capture("nosuch")')`
    // is an empty string, and the pipeline carries on over a session that was
    // never created. Here the daemon's refusal is raised, so the line after it
    // must not run — and the proof is on the far side, in a session that never
    // receives the marker.
    let dir = scratch("refusal");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path);

    let source = r#"
        remuda.new("witness", {"sh"})
        remuda.send("nosuch", "hello")
        remuda.send("witness", "echo REACHED-THE-LINE-AFTER")
    "#;

    let err = script::run(&path, &write(&dir, "refusal.lua", source))
        .expect_err("sending to a missing session must raise");
    assert!(
        err.to_string().contains("no such session"),
        "the daemon's own words must survive into Lua, got: {err}"
    );

    // Give a hypothetical stray write time to land before concluding it did not.
    std::thread::sleep(Duration::from_millis(300));
    let screen = capture(&path, "witness");
    assert!(
        !screen.contains("REACHED-THE-LINE-AFTER"),
        "execution continued past a refusal:\n{screen}"
    );
}

#[test]
fn remuda_new_can_set_cwd_and_env_on_the_launched_process() {
    // The launched process's OWN view — its own `pwd`, its own environment —
    // not the request/response round trip, and not typed input a pty would
    // just echo back (PRINCIPLES.md §4): `command` runs immediately as argv.
    let dir = scratch("new-cwd-env");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path);

    let target = scratch("new-cwd-env-target");
    // Escaped, not interpolated raw: on Windows this path contains `\`, which
    // a raw insert into Lua source would misparse as an escape sequence.
    let cwd_literal = target
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let source = format!(
        r#"
        remuda.new("probed", {{"sh", "-c", "pwd && echo $PROBE_VAR && echo path-has:$PATH"}}, "{cwd_literal}", {{PROBE_VAR = "remuda-env-probe-7f3a"}})
        "#
    );
    script::run(&path, &write(&dir, "cwd-env.lua", &source)).expect("script");

    let needle = target
        .file_name()
        .and_then(|n| n.to_str())
        .expect("scratch dir has a name")
        .to_string();

    let deadline = Instant::now() + PATIENCE;
    loop {
        let screen = capture(&path, "probed");
        // `path-has:/` proves env is ADDITIVE, not exclusive: the caller's map
        // only ever sets PROBE_VAR, so an inherited PATH surviving alongside
        // it means the daemon's own environment was layered under, not wiped.
        if screen.contains(&needle)
            && screen.contains("remuda-env-probe-7f3a")
            && screen.contains("path-has:/")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cwd/env never showed up on screen:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
