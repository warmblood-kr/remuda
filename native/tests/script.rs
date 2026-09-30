//! The Lua runtime: what it can reach, and what it does when refused.

use remuda_core::protocol::{Request, Response};
use remuda_native::{client, daemon, script};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[path = "daemon_support/spawn.rs"]
mod spawn;

const PATIENCE: Duration = Duration::from_secs(10);

/// A directory of our own, short enough for `sun_path` (~108 bytes).
fn scratch(tag: &str) -> PathBuf {
    let root = if cfg!(unix) {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    let dir = root.join(format!("remuda-s{}-{tag}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Start a daemon and return once it actually answers, not once it was spawned.
fn daemon_at(path: &Path, dir: &Path) -> spawn::Daemon {
    debug_assert_eq!(path, daemon::socket_path_in(dir, "s"));
    let mut command = spawn::base_command(dir);
    command.env("REMUDA_SUPPRESS_DEPRECATIONS", "1");
    spawn::spawn_and_wait(command, dir)
}

#[test]
fn deprecated_flat_session_alias_warns_once_per_process() {
    let dir = scratch("deprecation-once");
    let path = daemon::socket_path_in(&dir, "s");
    let mut command = spawn::base_command(&dir);
    command
        .env_remove("REMUDA_SUPPRESS_DEPRECATIONS")
        .stderr(std::process::Stdio::piped());
    let mut daemon = spawn::spawn_and_wait(command, &dir);

    script::run_source(&path, "=deprecation-once", "remuda.ls(); remuda.ls()")
        .expect("deprecated aliases should remain callable");
    let _ = daemon.0.kill();
    let _ = daemon.0.wait();
    let mut stderr = String::new();
    daemon
        .0
        .stderr
        .take()
        .expect("stderr is piped")
        .read_to_string(&mut stderr)
        .expect("read daemon stderr");

    let notice = "deprecated: remuda.ls; use remuda.session.list";
    assert_eq!(
        stderr.lines().filter(|line| *line == notice).count(),
        1,
        "expected one deprecation notice, got stderr: {stderr}"
    );
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

/// `request_counts()`'s three fields, read back through one `Eval` — the
/// same "one object, read from Lua" a runtime caller gets, just parsed here
/// instead of eyeballed.
fn request_counts(path: &Path) -> (u64, u64, u64) {
    let code = "local c = remuda.request_counts(); \
                 return c.list .. ',' .. c.eval .. ',' .. c.capture_styled";
    match client::request(
        path,
        &Request::Eval {
            code: code.to_string(),
            name: None,
        },
    ) {
        Ok(Response::Value(text)) => {
            let parts: Vec<u64> = text.split(',').map(|n| n.parse().unwrap()).collect();
            (parts[0], parts[1], parts[2])
        }
        other => panic!("request_counts failed: {other:?}"),
    }
}

#[test]
fn expect_option_strips_whole_utf8_selection_markers() {
    let dir = scratch("expect-option-utf8");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path, &dir);
    let code = r#"
        local highlighted = remuda.expect_option(
            "  Update available\n› 1. Update now\n  2. Skip\n",
            function(label) return label == "Update now" end
        )
        assert(highlighted == "1", "the highlighted UTF-8 option marker must be stripped")

        local boxed = remuda.expect_option(
            "┌ Options\n│ ❯ 2. Foo\n│   3. Bar\n└\n",
            function(label) return label == "Foo" end
        )
        assert(boxed == "2", "box and selection markers must both be stripped as whole characters")
        return highlighted .. "," .. boxed
    "#;
    let result = match client::request(
        &path,
        &Request::Eval {
            code: code.to_string(),
            name: None,
        },
    ) {
        Ok(Response::Value(value)) => value,
        other => panic!("expect_option evaluation failed: {other:?}"),
    };
    assert_eq!(result, "1,2");
}

#[test]
fn daemon_request_counts_reflect_real_requests_seen_at_dispatch() {
    // "One object, readable two ways": this Rust test is one of those two
    // ways, reading the same `request_counts()` binding a Lua/MCP caller
    // would. A direct List (outside any script) and the Eval this test uses
    // to read the counts are each real, separately dispatched requests, so
    // the deltas below are exact, not approximate.
    let dir = scratch("request-counts");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path, &dir);

    let before = request_counts(&path); // itself one Eval
    client::request(&path, &Request::List).expect("list");
    let after = request_counts(&path); // itself one more Eval

    assert_eq!(
        after.0,
        before.0 + 1,
        "one direct List must add exactly one to the list count"
    );
    assert_eq!(
        after.1,
        before.1 + 1,
        "exactly one new Eval happened between the two snapshots (the second snapshot's own read) \
         — the first snapshot's read is already baked into `before`, since the daemon counts a \
         request before dispatching it"
    );
}

#[test]
fn the_bound_surface_is_exactly_the_protocols() {
    // The property this guards is the reason embedding Lua is safe at all: the
    // script's vocabulary is `Request` and nothing else. The check runs in both
    // directions — a binding added without a decision fails, and a name listed
    // in BINDINGS but never bound fails too.
    let dir = scratch("surface");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path, &dir);

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
        local session_names = {{}}
        for key in pairs(remuda.session) do session_names[#session_names + 1] = key end
        table.sort(session_names)
        local session_got = table.concat(session_names, ",")
        local session_want = "attach,close,list,new,resize"
        if session_got ~= session_want then
          error("session namespace is " .. session_got .. ", expected " .. session_want)
        end
        assert(type(remuda.process) == "table", "process namespace is a table")
        local process_words = {{}}
        for key in pairs(remuda.process) do process_words[#process_words + 1] = key end
        table.sort(process_words)
        assert(table.concat(process_words, ",") == "run", "process namespace surface must be exactly run")
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

/// Run `source` in a fresh daemon of its own; the script asserts in Lua.
fn run_lua(tag: &str, source: &str) {
    let dir = scratch(tag);
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path, &dir);
    script::run(&path, &write(&dir, &format!("{tag}.lua"), source)).expect(tag);
}

#[test]
fn hooks_run_in_depth_order_and_an_id_replaces_its_hook() {
    // Emacs add-hook DEPTH: lower runs first, ties keep registration order;
    // re-registering (event, group, id) replaces rather than duplicates.
    run_lua(
        "hook-depth",
        r#"
        local ran = {}
        local function mark(tag) return function() ran[#ran + 1] = tag end end
        remuda.on("d", mark("zero-a"))
        remuda.on("d", mark("late"), { depth = 90 })
        remuda.on("d", mark("early"), { depth = -50 })
        remuda.on("d", mark("zero-b"))
        remuda.emit("d")
        assert(table.concat(ran, ",") == "early,zero-a,zero-b,late", table.concat(ran, ","))

        ran = {}
        remuda.on("r", mark("v1"), { group = "g", id = "x" })
        remuda.on("r", mark("other"), { group = "g" })
        remuda.on("r", mark("v2"), { group = "g", id = "x" })
        remuda.on("r", mark("same-id-other-group"), { group = "h", id = "x" })
        remuda.emit("r")
        assert(table.concat(ran, ",") == "other,v2,same-id-other-group", table.concat(ran, ","))
        assert(not pcall(remuda.on, "r", mark("bad"), { depth = "high" }), "a non-number depth is refused")
        "#,
    );
}

#[test]
fn emit_protocols_answer_veto_and_filter_and_errors_are_no_answer() {
    // run-hook-with-args-until-success / -until-failure, and a filter chain.
    // A handler that errors is "no answer": never a result, never a veto.
    run_lua(
        "hook-protocols",
        r#"
        remuda.on("ask", function() error("boom") end, { depth = -10 })
        remuda.on("ask", function() return nil end)
        remuda.on("ask", function(x) return "answer:" .. x end)
        remuda.on("ask", function() error("never reached") end, { depth = 50 })
        assert(remuda.emit_until_success("ask", "q") == "answer:q")
        assert(remuda.emit_until_success("nobody") == nil)

        remuda.on("allow", function() error("boom") end)
        remuda.on("allow", function() return nil end)
        assert(remuda.emit_until_failure("allow") == true, "errors and nil do not veto")
        remuda.on("allow", function() return false end, { id = "no" })
        assert(remuda.emit_until_failure("allow") == false, "false vetoes")
        assert(remuda.emit_until_failure("nobody") == true)

        remuda.on("f", function(v, add) return v + add end)
        remuda.on("f", function() error("boom") end)
        remuda.on("f", function() return nil end)
        remuda.on("f", function(v) return v * 10 end, { depth = 10 })
        assert(remuda.emit_filter("f", 1, 2) == 30, "errors and nil leave the value unchanged")
        assert(remuda.emit_filter("nobody", "same") == "same")

        local counts = remuda.event_counts()
        assert(counts.ask == 1 and counts.allow == 2 and counts.f == 1, "protocol emits are counted")
        "#,
    );
}

#[test]
fn hook_list_reports_each_hook_and_its_errors_as_a_copy() {
    run_lua(
        "hook-list",
        r#"
        remuda.on("e", function() error("first") end, { group = "g", id = "bad", depth = 5 })
        remuda.on("e", function() end)
        remuda.on("other", function() end)
        remuda.emit("e")
        remuda.emit("e")

        local list = remuda.hook_list("e")
        assert(#list == 2, "only the asked event")
        local bad = list[2]
        assert(bad.event == "e" and bad.group == "g" and bad.id == "bad" and bad.depth == 5)
        assert(bad.errors == 2, "errors counted per hook: " .. tostring(bad.errors))
        assert(tostring(bad.last_error):find("first", 1, true), tostring(bad.last_error))
        assert(type(bad.src) == "string" and bad.src ~= "", "src names where the hook was defined")
        assert(list[1].errors == 0 and list[1].depth == 0)
        local all = remuda.hook_list()
        assert(#all == 4, "no event means every event, including the built-in session_output hook")
        local found_output_hook = false
        for _, hook in ipairs(all) do
          if hook.event == "session_output" and hook.group == "remuda.expect" then
            found_output_hook = true
          end
        end
        assert(found_output_hook, "remuda.expect installs its session_output hook")

        bad.errors, bad.fn = 99, nil
        assert(remuda.hook_list("e")[2].errors == 2, "hook_list hands out copies")
        assert(list[1].fn == nil, "copies never expose the callback")
        "#,
    );
}

#[test]
fn butlers_filter_by_assignment_still_works_on_the_new_entry_shape() {
    // remuda-butler main.lua drops legacy ungrouped Matrix hooks by rebuilding
    // `remuda.hooks[event]` (writable, deprecated). This must keep working.
    run_lua(
        "hook-butler-compat",
        r#"
        local fired = {}
        remuda.on("butler-matrix-line", function() fired[#fired + 1] = "legacy" end)
        remuda.on("butler-matrix-line", function() fired[#fired + 1] = "grouped" end, { group = "butler", id = "line" })
        for _, event in ipairs({ "butler-matrix-line", "butler-matrix-submit" }) do
          local kept = {}
          for _, hook in ipairs(remuda.hooks[event] or {}) do
            if hook.group then kept[#kept + 1] = hook end
          end
          remuda.hooks[event] = kept
        end
        remuda.emit("butler-matrix-line")
        assert(table.concat(fired, ",") == "grouped", table.concat(fired, ","))
        local list = remuda.hook_list("butler-matrix-line")
        assert(#list == 1 and list[1].id == "line", "hook_list sees the filtered table")
        remuda.on("butler-matrix-line", function() fired[#fired + 1] = "again" end, { group = "butler", id = "line" })
        fired = {}
        remuda.emit("butler-matrix-line")
        assert(table.concat(fired, ",") == "again", "id replace works after a hand filter")
        "#,
    );
}

#[test]
fn every_word_has_a_registry_entry() {
    // Every BINDINGS name, plus every remuda.tool() registrant (wait_for is
    // reachable only through remuda.tools, never as a top-level BINDINGS
    // name) — the two categories the reference manual has to cover.
    let dir = scratch("registry-completeness");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path, &dir);

    let names = script::BINDINGS.join(",");
    let source = format!(
        r#"
        local missing = {{}}
        for name in string.gmatch("{names}", "[^,]+") do
          if remuda._registry[name] == nil then missing[#missing + 1] = name end
        end
        for name in pairs(remuda.tools) do
          if remuda._registry[name] == nil then missing[#missing + 1] = name end
        end
        if #missing > 0 then
          table.sort(missing)
          error("no registry entry for: " .. table.concat(missing, ", "))
        end
        "#
    );

    script::run(&path, &write(&dir, "completeness.lua", &source)).expect("registry complete");
}

#[test]
fn remuda_json_round_trips_values_and_rejects_bad_inputs() {
    run_lua(
        "json-surface",
        r#"
        assert(type(remuda.json) == "table")
        assert(remuda.json.null ~= nil)
        local decoded, err = remuda.json.decode('{"empty_array":[],"empty_object":{},"nil":null,"count":4,"ratio":1.25}')
        assert(decoded and err == nil, tostring(err))
        assert(remuda.json.encode(decoded) == '{"count":4,"empty_array":[],"empty_object":{},"nil":null,"ratio":1.25}')
        assert(decoded.empty_array[1] == nil and decoded.empty_object.any == nil)
        assert(decoded["nil"] == remuda.json.null)
        assert(type(decoded.count) == "number" and decoded.count == 4)
        assert(type(decoded.ratio) == "number" and decoded.ratio == 1.25)
        local number_kinds = remuda.json.decode('[4,4.0]')
        assert(math.type(number_kinds[1]) == "integer" and math.type(number_kinds[2]) == "float")
        assert(remuda.json.encode(remuda.json.array{}) == "[]")
        assert(remuda.json.encode(remuda.json.object{}) == "{}")
        local foreign = setmetatable({}, { __index = function() return "foreign" end })
        assert(not pcall(remuda.json.array, foreign), "array tagging refuses a foreign metatable")
        assert(not pcall(remuda.json.object, foreign), "object tagging refuses a foreign metatable")
        assert(not pcall(remuda.json.encode, {}), "an empty untagged table is ambiguous")
        assert(not pcall(remuda.json.encode, { 1, name = "mixed" }), "mixed keys are refused")
        local sparse_ok, sparse_error = pcall(remuda.json.encode, { [2] = "sparse" })
        assert(not sparse_ok and tostring(sparse_error):find("sparse array", 1, true), tostring(sparse_error))
        assert(not pcall(remuda.json.encode, 0/0), "NaN is refused")
        assert(not pcall(remuda.json.encode, math.huge), "infinity is refused")
        assert(not pcall(remuda.json.encode, function() end), "functions are refused")
        assert(not pcall(remuda.json.encode, coroutine.create(function() end)), "threads are refused")
        assert(not pcall(remuda.json.encode, nil), "nil must use the null sentinel")

        local cycle = {}; cycle.self = cycle
        assert(not pcall(remuda.json.encode, cycle), "cycles are refused")
        local too_deep_value = 0
        for _ = 1, 65 do too_deep_value = { too_deep_value } end
        assert(not pcall(remuda.json.encode, too_deep_value), "deep Lua tables are refused")
        assert(not pcall(remuda.json.encode, { [string.char(255)] = true }), "invalid UTF-8 keys are refused")

        local duplicate, duplicate_error = remuda.json.decode('{"x":1,"x":2}')
        assert(duplicate == nil and duplicate_error == "duplicate key", tostring(duplicate_error))
        local truncated, truncated_error = remuda.json.decode('{"x":')
        assert(truncated == nil and type(truncated_error) == "string")
        local invalid, invalid_error = remuda.json.decode(string.char(255))
        assert(invalid == nil and type(invalid_error) == "string")
        local huge, huge_error = remuda.json.decode("1e9999")
        assert(huge == nil and type(huge_error) == "string")
        local wide_integer, wide_error = remuda.json.decode("18446744073709551615")
        assert(wide_integer ~= nil and wide_error == nil and wide_integer > 1e18)
        local deep = string.rep("[", 65) .. "0" .. string.rep("]", 65)
        local too_deep, depth_error = remuda.json.decode(deep)
        assert(too_deep == nil and type(depth_error) == "string")
        local many_values = "[" .. string.rep("null,", 100000) .. "null]"
        local too_many, count_error = remuda.json.decode(many_values)
        assert(too_many == nil and count_error:find("maximum JSON value count exceeded", 1, true), tostring(count_error))
        local too_large, size_error = remuda.json.decode(string.rep(" ", 8 * 1024 * 1024 + 1))
        assert(too_large == nil and type(size_error) == "string")

        local pretty = remuda.json.encode({ value = 1 }, { pretty = true })
        assert(pretty:find("\n", 1, true), pretty)
        local output_ok, output_error = pcall(remuda.json.encode, string.rep(string.char(0), 1500000))
        assert(not output_ok and tostring(output_error):find("encoded output exceeds", 1, true), tostring(output_error))
        local many = remuda.json.array{}
        for i = 1, 100000 do many[i] = remuda.json.null end
        local values_ok, values_error = pcall(remuda.json.encode, many)
        assert(not values_ok and tostring(values_error):find("maximum JSON value count exceeded", 1, true), tostring(values_error))
        "#,
    );
}

#[test]
fn remuda_json_nested_words_are_registered() {
    run_lua(
        "json-registry",
        r#"
        for _, name in ipairs({ "json", "json.decode", "json.encode", "json.null", "json.array", "json.object" }) do
          assert(remuda._registry[name] ~= nil, "missing registry row for " .. name)
        end
        "#,
    );
}

#[test]
fn random_bytes_returns_csprng_bytes_and_rejects_invalid_lengths() {
    run_lua(
        "random-bytes",
        r#"
        assert(type(remuda.random_bytes) == "function", "remuda.random_bytes is missing")

        assert(#remuda.random_bytes(1) == 1, "random_bytes accepts the lower boundary")
        assert(#remuda.random_bytes(65536) == 65536, "random_bytes accepts the upper boundary")
        assert(#remuda.random_bytes(32.0) == 32, "integer-valued Lua floats are accepted")
        local first = remuda.random_bytes(32)
        local second = remuda.random_bytes(32)
        assert(type(first) == "string" and #first == 32, "random_bytes returns exactly n bytes")
        assert(first ~= second, "independent random_bytes calls should differ")

        for _, n in ipairs({ 0, -1, 1.5, "4", 65537, 2^53, 0/0, math.huge }) do
          local ok = pcall(remuda.random_bytes, n)
          assert(not ok, "random_bytes should reject " .. tostring(n))
        end
        "#,
    );
}

#[test]
fn registry_documentation_formats_are_live_and_structured() {
    let dir = scratch("registry-docs");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path, &dir);

    let eval = |code: &str| match client::request(
        &path,
        &Request::Eval {
            code: code.to_string(),
            name: None,
        },
    ) {
        Ok(Response::Value(value)) => value,
        other => panic!("documentation evaluation failed: {other:?}"),
    };

    let document: Value = serde_json::from_str(&eval("return remuda._registry_dump('json')"))
        .expect("JSON documentation must be valid JSON");
    assert_eq!(document["name"], "remuda");
    assert!(document["runtime"]["classes"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(document["runtime"]["functions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["name"] == "ls"));
    assert!(document["runtime"]["functions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["name"] == "session.resize"));
    let random_bytes = document["runtime"]["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "random_bytes")
        .expect("random_bytes is documented");
    assert!(random_bytes["description"]
        .as_str()
        .unwrap()
        .contains("OS CSPRNG"));
    assert!(random_bytes["description"]
        .as_str()
        .unwrap()
        .contains("65536"));
    for section in ["functions", "variables"] {
        assert!(
            document["runtime"][section]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| !entry["name"].as_str().unwrap_or_default().starts_with('_')),
            "public reference exposed a private word in {section}"
        );
    }
    assert!(document["runtime"]["variables"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["name"] == "tools"));
    assert!(eval("return remuda._registry_dump('markdown')").starts_with("# Remuda Lua runtime"));
    assert!(eval("return remuda._registry_dump('rst')").starts_with("Remuda Lua runtime"));
}

#[test]
fn every_frozen_api_version_still_runs() {
    // `tests/api/vN.lua` is a script written against version N of the Lua API,
    // and it is frozen: widening the surface means adding `v2.lua`, never
    // editing `v1.lua`. That is what makes this a control rather than a
    // ceremony — a check you may edit to make it pass checks nothing. Renaming
    // a binding, reordering its arguments, or making an optional argument
    // required now fails the build instead of someone's plugin.
    let dir = scratch("compat");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path, &dir);

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
    let _daemon = daemon_at(&path, &dir);

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
    let _daemon = daemon_at(&path, &dir);

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
    let _daemon = daemon_at(&path, &dir);

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

#[test]
fn a_session_handle_is_not_a_buffer() {
    // Both nouns, on purpose: `remuda.session(name).buffer` for the noun
    // that owns text, `remuda.buffer.set` for the free-function spelling of
    // the same thing — and `is_busy` must exist on the first and not the
    // second, since a buffer is inert text with no notion of "working".
    let dir = scratch("session-buffer");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path, &dir);

    let source = r#"
        remuda.new("driven", {"sh"})
        local s = remuda.session("driven")
        assert(type(s.is_busy) == "boolean", "session.is_busy must be a boolean")

        s.buffer:set("via session.buffer")
        assert(remuda.buffer.new("driven"):get() == "via session.buffer",
          "session.buffer must be the buffer named after the session")

        remuda.buffer.set("driven", "via buffer.set")
        assert(s.buffer:get() == "via buffer.set",
          "buffer.set(name, text) must reach the same buffer session.buffer does")

        local b = remuda.buffer.new("standalone")
        assert(b.is_busy == nil, "a buffer must never carry is_busy")
        assert(b.context_left == nil, "a buffer must never carry context_left")
    "#;

    script::run(&path, &write(&dir, "session_buffer.lua", source)).expect("session vs buffer");
}

#[test]
fn is_busy_tracks_streaming_output_then_goes_idle() {
    let dir = scratch("busy-from-output");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path, &dir);

    let source = r#"
        remuda.new("streaming", {"sh", "-c", "i=0; while [ $i -lt 20 ]; do printf x; sleep 0.1; i=$((i + 1)); done; sleep 30"})
        local s = remuda.session("streaming")
        remuda.sleep(1.0)
        assert(s.is_busy, "a session producing output without input must stay busy")
        local row = remuda.ls()[1]
        assert(row.idle > 0.8, "ls().idle must keep its since-input meaning")
        assert(row.output_idle < 2.0, "ls().output_idle must track recent output")

        for _ = 1, 100 do
            if not s.is_busy then break end
            remuda.sleep(0.1)
        end
        assert(not s.is_busy, "a session quiet for more than 2s must become idle")
        row = remuda.ls()[1]
        assert(row.idle > 3.0, "output must not reset since-input idle")
        assert(row.output_idle >= 2.0, "output_idle must age after streaming stops")
    "#;

    script::run_source(&path, "=streaming-busy", source)
        .expect("is_busy follows output and becomes idle after output stops");
}

#[test]
fn clearing_one_group_leaves_the_others_hooks_firing() {
    // The augroup model: a group clears as a unit, and clearing one must
    // never reach a hook registered under a different group — even on the
    // same event.
    let dir = scratch("hooks");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path, &dir);

    let source = r#"
        remuda.fired_a, remuda.fired_b = 0, 0
        remuda.on("tick", function() remuda.fired_a = remuda.fired_a + 1 end, { group = "a" })
        remuda.on("tick", function() remuda.fired_b = remuda.fired_b + 1 end, { group = "b" })

        remuda.emit("tick")
        remuda.emit("tick")
        assert(remuda.fired_a == 2, "group a must fire on every emit before clearing")
        assert(remuda.fired_b == 2, "group b must fire on every emit before clearing")

        remuda.clear_hooks({ group = "a" })
        remuda.emit("tick")
        remuda.emit("tick")
        assert(remuda.fired_a == 2, "a cleared group must not fire again")
        assert(remuda.fired_b == 4, "an untouched group must keep firing")

        local ok = pcall(remuda.clear_hooks, {})
        assert(not ok, "clear_hooks must refuse to run with no group")
    "#;

    script::run(&path, &write(&dir, "hooks.lua", source)).expect("hook groups");
}

/// #137: a script can tell dim text (a TUI's ghost suggestion) from typed text,
/// and see where the cursor is.
#[test]
fn capture_styled_marks_dim_spans_and_reports_the_cursor() {
    let dir = scratch("styled");
    let path = daemon::socket_path_in(&dir, "s");
    let _daemon = daemon_at(&path, &dir);
    let code = r#"
        -- Wait on what the screen shows, never on timing: first the shell's
        -- prompt (so the command is not typed before sh reads), then the
        -- output row itself. A timeout fails loudly with the screen.
        local function await(pattern)
          for _ = 1, 1000 do
            if remuda.capture("styled"):find(pattern) then return end
            remuda.sleep(0.02)
          end
          error("never saw " .. pattern .. " on screen:\n" .. remuda.capture("styled"))
        end
        remuda.new("styled", {"sh"})
        await("%S")
        remuda.send("styled", "printf '\\033[2mgh%sst\\033[0m-plain\\n' o")
        await("ghost%-plain")
        local screen, dim = remuda.capture_styled("styled"), {}
        for _, row in ipairs(screen.rows) do
          if row[1] and row[1].text:find("^ghost") then -- the output, not the echo
            for _, span in ipairs(row) do
              if span.text:find("%S") then
                dim[#dim + 1] = span.text:match("%S+") .. "=" .. tostring(span.dim)
              end
            end
          end
        end
        local c = screen.cursor
        return table.concat(dim, " ") .. " cursor=" .. type(c.row) .. "," .. type(c.col) .. "," .. type(c.visible)
    "#;
    let got = match client::request(
        &path,
        &Request::Eval {
            code: code.into(),
            name: None,
        },
    ) {
        Ok(Response::Value(value)) => value,
        other => panic!("eval: {other:?}"),
    };
    assert_eq!(got, "ghost=true -plain=false cursor=number,number,boolean");
}
