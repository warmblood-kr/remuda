use remuda_core::Registry;
use remuda_native::{image::Image, tick::Counters};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

// XDG_DATA_HOME is process-global; tests that set it must not overlap.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct DataHome {
    root: PathBuf,
    old_data: Option<std::ffi::OsString>,
    _env: std::sync::MutexGuard<'static, ()>,
}

impl DataHome {
    fn new() -> Self {
        let env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "remuda-mod-lifecycle-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("private data home");
        let old_data = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", &root);
        Self {
            root,
            old_data,
            _env: env,
        }
    }

    fn entry(&self) -> PathBuf {
        self.root
            .join("remuda/mods/sample/packages/sample/init.lua")
    }
}

impl Drop for DataHome {
    fn drop(&mut self) {
        if let Some(old) = self.old_data.take() {
            std::env::set_var("XDG_DATA_HOME", old);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn write_entry(path: &Path, source: &str) {
    fs::create_dir_all(path.parent().expect("entry parent")).expect("package directory");
    fs::write(path, source).expect("write mod entry");
}

fn read_value(image: &Image, code: &str) -> String {
    image.eval(code, None).expect("eval in persistent image")
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario; PR #104 reshapes this file"
)]
fn installed_mod_reloads_in_the_same_image_without_losing_state_or_old_code_on_failure() {
    let home = DataHome::new();
    let manifest = home.root.join("remuda/mods/sample/extension.toml");
    fs::create_dir_all(manifest.parent().expect("manifest parent")).unwrap();
    fs::write(
        &manifest,
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();

    let entry = home.entry();
    write_entry(
        &entry,
        r#"return {
          api = "remuda-module-v1", state_version = 1,
          initialize = function() return { count = 0 } end,
          hooks = {{ event = "probe", run = function(state)
            state.count = state.count + 1
          end }},
          tools = {{ name = "sample_status",
            about = "Read the persistent state for this test mod.",
            run = function(state) return tostring(state.count) end }},
        }"#,
    );

    let image = Image::spawn(
        Path::new("/tmp/remuda-mod-lifecycle-unused.sock"),
        Arc::new(Registry::new()),
        Arc::new(Counters::default()),
    );
    read_value(&image, include_str!("api/v3.lua"));
    read_value(&image, "remuda.exec('sample')");
    assert_eq!(
        read_value(&image, "return remuda.tools.sample_status()"),
        "0"
    );
    read_value(&image, "remuda.emit('probe')");
    assert_eq!(
        read_value(&image, "return remuda.tools.sample_status()"),
        "1"
    );

    write_entry(
        &entry,
        r#"return {
          api = "remuda-module-v1", state_version = 1,
          initialize = function() error("reload must preserve state") end,
          hooks = {{ event = "probe", run = function(state)
            state.count = state.count + 10
          end }},
          tools = {{ name = "sample_status",
            about = "Read the persistent state for this test mod.",
            run = function(state) return tostring(state.count) end }},
        }"#,
    );
    read_value(&image, "api_v3_phase = 'same_version'");
    read_value(&image, include_str!("api/v3.lua"));

    write_entry(
        &entry,
        r#"return {
          api = "remuda-module-v1", state_version = 2,
          initialize = function() return { called = false } end,
          migrations = {[1] = function(state)
            state.count = state.count + 100
            return state
          end},
          hooks = {{ event = "probe", run = function(state)
            state.count = state.count + 20
          end }},
          tools = {{ name = "sample_status",
            about = "Read the persistent state for this test mod.",
            run = function(state) return tostring(state.count) end }},
        }"#,
    );
    read_value(&image, "api_v3_phase = 'migration'");
    read_value(&image, include_str!("api/v3.lua"));

    write_entry(
        &entry,
        r#"return {
          api = "remuda-module-v1", state_version = 3,
          initialize = function() return {} end,
          migrations = {[2] = function(state)
            state.count = -1
            error("intentional migration failure")
          end},
          hooks = {}, tools = {},
        }"#,
    );
    read_value(&image, "api_v3_phase = 'migration_failure'");
    read_value(&image, include_str!("api/v3.lua"));

    write_entry(
        &entry,
        r#"_G.lifecycle_staging_marker = true
           remuda.on("probe", function() end)
           return { api = "remuda-module-v1", state_version = 1,
             initialize = function() return {} end }"#,
    );
    assert!(image.eval("remuda.reload('sample')", None).is_err());
    assert_eq!(
        read_value(&image, "return rawget(_G, 'lifecycle_staging_marker')"),
        "nil"
    );
    read_value(&image, "remuda.emit('probe')");
    assert_eq!(
        read_value(&image, "return remuda.tools.sample_status()"),
        "171"
    );

    fs::write(
        &manifest,
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\n",
    )
    .unwrap();
    read_value(&image, "api_v3_phase = 'legacy_refusal'");
    read_value(&image, include_str!("api/v3.lua"));
}

#[test]
fn activated_lifecycle_hook_can_call_remuda_and_hook_errors_are_visible() {
    let home = DataHome::new();
    let manifest = home.root.join("remuda/mods/sample/extension.toml");
    fs::create_dir_all(manifest.parent().expect("manifest parent")).unwrap();
    fs::write(
        &manifest,
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();
    write_entry(
        &home.entry(),
        r#"return {
          api = "remuda-module-v1", state_version = 1,
          initialize = function() return {} end,
          hooks = {
            { event = "probe", run = function() remuda.emit("nested") end },
            { event = "probe", run = function() error("visible lifecycle hook error") end },
            { event = "probe", run = function(state) state.called = true end },
          },
          tools = {{ name = "sample_called",
            about = "Read whether the last lifecycle hook ran.",
            run = function(state) return tostring(state.called) end }},
        }"#,
    );

    let image = Image::spawn(
        Path::new("/tmp/remuda-mod-lifecycle-hook-error.sock"),
        Arc::new(Registry::new()),
        Arc::new(Counters::default()),
    );
    read_value(&image, include_str!("api/v3.lua"));
    read_value(&image, "remuda.exec('sample')");
    image
        .eval("remuda.emit('probe')", None)
        .expect("emit runs hooks");
    assert_eq!(
        read_value(&image, "return remuda.tools.sample_called()"),
        "true"
    );
}

#[test]
fn lifecycle_start_runs_once_after_activation_and_surfaces_errors() {
    let home = DataHome::new();
    let manifest = home.root.join("remuda/mods/sample/extension.toml");
    fs::create_dir_all(manifest.parent().expect("manifest parent")).unwrap();
    fs::write(
        &manifest,
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();
    let entry = home.entry();
    write_entry(
        &entry,
        r#"return {
          api = "remuda-module-v1", state_version = 1,
          initialize = function() return { starts = 0, hooks = 0 } end,
          start = function(state)
            state.starts = state.starts + 1
            remuda.emit("start_probe")
          end,
          hooks = {{ event = "start_probe", run = function(state)
            state.hooks = state.hooks + 1
          end }},
          tools = {{ name = "sample_start_count",
            about = "Read how often this module started.",
            run = function(state) return state.starts .. ":" .. state.hooks end }},
        }"#,
    );
    let image = Image::spawn(
        Path::new("/tmp/remuda-mod-lifecycle-start.sock"),
        Arc::new(Registry::new()),
        Arc::new(Counters::default()),
    );
    read_value(&image, include_str!("api/v3.lua"));
    read_value(&image, "remuda.exec('sample')");
    assert_eq!(
        read_value(&image, "return remuda.tools.sample_start_count()"),
        "1:1"
    );
    read_value(&image, "remuda.reload('sample')");
    assert_eq!(
        read_value(&image, "return remuda.tools.sample_start_count()"),
        "2:2"
    );

    write_entry(
        &entry,
        r#"return {
      api = "remuda-module-v1", state_version = 1,
      initialize = function() return {} end,
      start = function() error("visible lifecycle start error") end,
    }"#,
    );
    let error = image.eval("remuda.reload('sample')", None).unwrap_err();
    assert!(error.contains("visible lifecycle start error"), "{error}");
}

#[test]
fn mod_command_handler_receives_caller_env() {
    // #95: the handler runs in the daemon, so `os.getenv` is the daemon's; the
    // caller's `REMUDA_*` variables arrive as the second handler argument.
    let image = Image::spawn(
        Path::new("/tmp/remuda-mod-caller-unused.sock"),
        Arc::new(Registry::new()),
        Arc::new(Counters::default()),
    );
    read_value(
        &image,
        "remuda.extension_command('probe', function(args, caller) \
           return args[1] .. ':' .. caller.env.REMUDA_BUTLER_AGENT_ID end)",
    );
    assert_eq!(
        read_value(
            &image,
            "return remuda._dispatch_extension_command('probe', {'inbox'}, \
             {env = {REMUDA_BUTLER_AGENT_ID = 'dev-lead'}})"
        ),
        "inbox:dev-lead"
    );
    // An older CLI sends no caller table; handlers still get one.
    assert_eq!(
        read_value(
            &image,
            "remuda.extension_command('bare', function(_, caller) return type(caller) end); \
             return remuda._dispatch_extension_command('bare', {})"
        ),
        "table"
    );
}

// #116: a reload must leave the image in the shape of one activation, and
// `exec` on an already-active mod (a no-args mod command) must not restart it.
#[test]
fn reloads_own_declared_schedules_and_exec_does_not_restart_an_active_mod() {
    let home = DataHome::new();
    let manifest = home.root.join("remuda/mods/sample/extension.toml");
    fs::create_dir_all(manifest.parent().expect("manifest parent")).unwrap();
    fs::write(
        &manifest,
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();
    write_entry(
        &home.entry(),
        r#"return {
          api = "remuda-module-v1", state_version = 1,
          initialize = function() return { starts = 0, ticks = 0 } end,
          start = function(state) state.starts = state.starts + 1 end,
          schedules = {{ name = "sample_tick", every = 1, run = function(state)
            state.ticks = state.ticks + 1
          end }},
          tools = {{ name = "sample_counts",
            about = "Read this module's start and tick counts.",
            run = function(state) return state.starts .. ":" .. state.ticks end }},
        }"#,
    );
    let image = Image::spawn(
        Path::new("/tmp/remuda-mod-lifecycle-schedules.sock"),
        Arc::new(Registry::new()),
        Arc::new(Counters::default()),
    );
    read_value(&image, include_str!("api/v3.lua"));
    let schedules =
        "local n = 0 for _ in pairs(remuda.schedules) do n = n + 1 end return tostring(n)";

    read_value(&image, "remuda.exec('sample')");
    read_value(&image, "for _ = 1, 10 do remuda.reload('sample') end");
    assert_eq!(read_value(&image, schedules), "1");
    read_value(&image, "remuda._run_due_schedules(1e9)");
    assert_eq!(
        read_value(&image, "return remuda.tools.sample_counts()"),
        "11:1"
    );

    read_value(&image, "remuda.exec('sample')");
    assert_eq!(
        read_value(&image, "return remuda.tools.sample_counts()"),
        "11:1"
    );
    assert_eq!(read_value(&image, schedules), "1");
}

// #129: a reload whose start() fails must leave the previous activation's
// hooks, tools and schedules in place, as a failed declaration already does.
#[test]
fn reload_into_a_failing_start_keeps_the_previous_registrations() {
    let home = DataHome::new();
    let manifest = home.root.join("remuda/mods/sample/extension.toml");
    fs::create_dir_all(manifest.parent().expect("manifest parent")).unwrap();
    fs::write(
        &manifest,
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();
    let entry = home.entry();
    write_entry(
        &entry,
        r#"return {
          api = "remuda-module-v1", state_version = 1,
          initialize = function() return { hooks = 0 } end,
          hooks = {{ event = "probe", run = function(state) state.hooks = state.hooks + 1 end }},
          schedules = {{ name = "sample_tick", every = 1, run = function() end }},
          tools = {
            { name = "sample_old", about = "Report the old activation hook count.", run = function(state) return "old:" .. state.hooks end },
            { name = "sample_kept", about = "Report which activation owns this tool.", run = function() return "old" end },
          },
        }"#,
    );
    let image = Image::spawn(
        Path::new("/tmp/remuda-mod-lifecycle-rollback.sock"),
        Arc::new(Registry::new()),
        Arc::new(Counters::default()),
    );
    read_value(&image, include_str!("api/v3.lua"));
    read_value(&image, "remuda.exec('sample')");

    write_entry(
        &entry,
        r#"return {
          api = "remuda-module-v1", state_version = 1,
          initialize = function() return {} end,
          start = function() error("failing reload start") end,
          tools = {{ name = "sample_kept", about = "Report which activation owns this tool.", run = function() return "new" end }},
        }"#,
    );
    let error = image.eval("remuda.reload('sample')", None).unwrap_err();
    assert!(error.contains("failing reload start"), "{error}");

    read_value(&image, "remuda.emit('probe')");
    assert_eq!(
        read_value(&image, "return remuda.tools.sample_old()"),
        "old:1"
    );
    assert_eq!(
        read_value(&image, "return remuda.tools.sample_kept()"),
        "old"
    );
    assert_eq!(
        read_value(
            &image,
            "local n = 0 for _ in pairs(remuda.schedules) do n = n + 1 end return tostring(n)"
        ),
        "1"
    );
}

/// A hook a failing `start` registered is owned by the mod (hook-design §3),
/// so rollback drops it; hooks from outside the mod keep their place.
#[test]
fn rollback_drops_hooks_the_failed_start_registered() {
    let home = DataHome::new();
    let manifest = home.root.join("remuda/mods/sample/extension.toml");
    fs::create_dir_all(manifest.parent().expect("manifest parent")).unwrap();
    fs::write(
        &manifest,
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();
    let entry = home.entry();
    write_entry(
        &entry,
        r#"return { api = "remuda-module-v1", state_version = 1,
          initialize = function() return {} end }"#,
    );
    let image = Image::spawn(
        Path::new("/tmp/remuda-mod-lifecycle-depth.sock"),
        Arc::new(Registry::new()),
        Arc::new(Counters::default()),
    );
    read_value(&image, include_str!("api/v3.lua"));
    read_value(
        &image,
        "remuda._ran = {}; remuda.on('order', function() table.insert(remuda._ran, '0') end, { group = 'base', id = 'zero' })",
    );
    read_value(&image, "remuda.exec('sample')");
    write_entry(
        &entry,
        r#"return { api = "remuda-module-v1", state_version = 1,
          initialize = function() return {} end,
          start = function()
            remuda.on("order", function() table.insert(remuda._ran, "-100") end, { group = "late", id = "first", depth = -100 })
            error("start fails after registering")
          end }"#,
    );
    let error = image.eval("remuda.reload('sample')", None).unwrap_err();
    assert!(error.contains("start fails after registering"), "{error}");

    assert_eq!(
        read_value(
            &image,
            "remuda.emit('order'); return table.concat(remuda._ran, ',')"
        ),
        "0"
    );
    assert_eq!(
        read_value(
            &image,
            "local d = {} for i, h in ipairs(remuda.hook_list('order')) do d[i] = h.depth end return table.concat(d, ',')"
        ),
        "0"
    );
}

#[test]
fn imperative_contributions_are_owned_by_the_lifecycle_extent() {
    let home = DataHome::new();
    let manifest = home.root.join("remuda/mods/sample/extension.toml");
    fs::create_dir_all(manifest.parent().expect("manifest parent")).unwrap();
    fs::write(
        &manifest,
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();
    let entry = home.entry();
    write_entry(
        &entry,
        r#"return { api = "remuda-module-v1", state_version = 1,
          initialize = function() return {} end,
          start = function() remuda.contribute("probe", "v1", { label = "v1" }) end }"#,
    );
    let image = Image::spawn(
        Path::new("/tmp/remuda-mod-lifecycle-contribution-owner.sock"),
        Arc::new(Registry::new()),
        Arc::new(Counters::default()),
    );
    read_value(&image, include_str!("api/v3.lua"));
    read_value(&image, "remuda.exec('sample')");
    let ids = "local rows, ids = remuda.contributions('probe'), {} for _, row in ipairs(rows) do ids[#ids + 1] = row.id end return table.concat(ids, ',')";
    assert_eq!(read_value(&image, ids), "v1");

    write_entry(
        &entry,
        r#"return { api = "remuda-module-v1", state_version = 1,
          initialize = function() return {} end,
          start = function() remuda.contribute("probe", "v2", { label = "v2" }) end }"#,
    );
    read_value(&image, "remuda.reload('sample')");
    assert_eq!(read_value(&image, ids), "v2");

    write_entry(
        &entry,
        r#"return { api = "remuda-module-v1", state_version = 1,
          initialize = function() return {} end,
          start = function()
            remuda.contribute("probe", "failed", { label = "failed" })
            error("contribution start fails")
          end }"#,
    );
    let error = image.eval("remuda.reload('sample')", None).unwrap_err();
    assert!(error.contains("contribution start fails"), "{error}");
    assert_eq!(read_value(&image, ids), "v2");
}

#[test]
fn failed_start_drops_its_imperative_contributions() {
    let home = DataHome::new();
    let manifest = home.root.join("remuda/mods/sample/extension.toml");
    fs::create_dir_all(manifest.parent().expect("manifest parent")).unwrap();
    fs::write(
        &manifest,
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();
    let entry = home.entry();
    write_entry(
        &entry,
        r#"return { api = "remuda-module-v1", state_version = 1,
          initialize = function() return {} end,
          start = function() remuda.contribute("probe", "live", { label = "live" }) end }"#,
    );
    let image = Image::spawn(
        Path::new("/tmp/remuda-mod-lifecycle-contribution-rollback.sock"),
        Arc::new(Registry::new()),
        Arc::new(Counters::default()),
    );
    read_value(&image, include_str!("api/v3.lua"));
    read_value(&image, "remuda.exec('sample')");
    write_entry(
        &entry,
        r#"return { api = "remuda-module-v1", state_version = 1,
          initialize = function() return {} end,
          start = function()
            remuda.contribute("probe", "failed", { label = "failed" })
            error("contribution start fails")
          end }"#,
    );
    let error = image.eval("remuda.reload('sample')", None).unwrap_err();
    assert!(error.contains("contribution start fails"), "{error}");
    assert_eq!(
        read_value(
            &image,
            "local rows = remuda.contributions('probe'); local ids = {} for _, row in ipairs(rows) do ids[#ids + 1] = row.id end return table.concat(ids, ',')"
        ),
        "live"
    );
}

#[test]
fn failed_start_cannot_take_over_another_mods_imperative_contribution() {
    let home = DataHome::new();
    let owner_manifest = home.root.join("remuda/mods/owner/extension.toml");
    fs::create_dir_all(owner_manifest.parent().expect("manifest parent")).unwrap();
    fs::write(
        &owner_manifest,
        "name = \"owner\"\nentry = \"packages/owner/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();
    let owner_entry = home.root.join("remuda/mods/owner/packages/owner/init.lua");
    write_entry(
        &owner_entry,
        r#"return { api = "remuda-module-v1", state_version = 1,
          initialize = function() return {} end,
          start = function() remuda.contribute("probe", "shared", { label = "owner" }) end }"#,
    );

    let intruder_manifest = home.root.join("remuda/mods/intruder/extension.toml");
    fs::create_dir_all(intruder_manifest.parent().expect("manifest parent")).unwrap();
    fs::write(
        &intruder_manifest,
        "name = \"intruder\"\nentry = \"packages/intruder/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();
    let intruder_entry = home
        .root
        .join("remuda/mods/intruder/packages/intruder/init.lua");
    write_entry(
        &intruder_entry,
        r#"return { api = "remuda-module-v1", state_version = 1,
          initialize = function() return {} end,
          start = function()
            remuda.contribute("probe", "shared", { label = "intruder" })
            error("intruder start fails")
          end }"#,
    );

    let image = Image::spawn(
        Path::new("/tmp/remuda-mod-lifecycle-contribution-conflict.sock"),
        Arc::new(Registry::new()),
        Arc::new(Counters::default()),
    );
    read_value(&image, include_str!("api/v3.lua"));
    read_value(&image, "remuda.exec('owner')");
    let error = image.eval("remuda.exec('intruder')", None).unwrap_err();
    assert!(error.contains("owner"), "{error}");
    assert_eq!(
        read_value(
            &image,
            "local rows = remuda.contributions('probe'); return rows[1].owner .. ':' .. rows[1].entry.label"
        ),
        "owner:owner"
    );
}

/// Hook-design (b): what a mod registers imperatively while its own code runs
/// (`start`, a declared hook) is owned by it, so reload replaces it instead of
/// piling up, and a failed reload's `start` leaves the previous set intact.
#[test]
fn registrations_made_in_a_mods_extent_are_owned_and_replaced_on_reload() {
    let home = DataHome::new();
    let manifest = home.root.join("remuda/mods/sample/extension.toml");
    fs::create_dir_all(manifest.parent().expect("manifest parent")).unwrap();
    fs::write(
        &manifest,
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\nlifecycle = \"remuda-module-v1\"\n",
    )
    .unwrap();
    let entry = home.entry();
    write_entry(
        &entry,
        r#"return {
          api = "remuda-module-v1", state_version = 1,
          initialize = function() return {} end,
          start = function()
            remuda.on("owned_probe", function() end)
            remuda.extension_command("sample-cmd", function() return "v1" end)
          end,
          hooks = {{ event = "kick", run = function() remuda.on("kicked_probe", function() end) end }},
        }"#,
    );
    let image = Image::spawn(
        Path::new("/tmp/remuda-mod-lifecycle-owner.sock"),
        Arc::new(Registry::new()),
        Arc::new(Counters::default()),
    );
    read_value(&image, include_str!("api/v3.lua"));
    read_value(
        &image,
        "remuda.on('owned_probe', function() end, { group = 'outsider' })",
    );
    read_value(&image, "remuda.exec('sample'); remuda.emit('kick')");
    read_value(
        &image,
        "for _ = 1, 5 do remuda.reload('sample') end; remuda.emit('kick')",
    );
    let owners = "local o = {} for _, h in ipairs(remuda.hook_list(EVENT)) do o[#o + 1] = tostring(h.owner) end \
                  return table.concat(o, ',')";
    assert_eq!(
        read_value(&image, &owners.replace("EVENT", "'owned_probe'")),
        "nil,sample"
    );
    assert_eq!(
        read_value(&image, &owners.replace("EVENT", "'kicked_probe'")),
        "sample"
    );
    assert_eq!(
        read_value(
            &image,
            "return remuda._dispatch_extension_command('sample-cmd')"
        ),
        "v1"
    );

    // A reload whose start fails keeps v1's owned registrations, drops v2's.
    write_entry(
        &entry,
        r#"return {
          api = "remuda-module-v1", state_version = 1,
          initialize = function() return {} end,
          start = function()
            remuda.on("v2_probe", function() end)
            error("v2 start fails")
          end,
        }"#,
    );
    assert!(image.eval("remuda.reload('sample')", None).is_err());
    assert_eq!(
        read_value(&image, &owners.replace("EVENT", "'owned_probe'")),
        "nil,sample"
    );
    assert_eq!(
        read_value(&image, &owners.replace("EVENT", "'v2_probe'")),
        ""
    );
    assert_eq!(
        read_value(
            &image,
            "return remuda._dispatch_extension_command('sample-cmd')"
        ),
        "v1"
    );

    // A successful reload that no longer registers them drops them all.
    write_entry(
        &entry,
        r#"return { api = "remuda-module-v1", state_version = 1, initialize = function() return {} end }"#,
    );
    read_value(&image, "remuda.reload('sample')");
    assert_eq!(
        read_value(&image, &owners.replace("EVENT", "'owned_probe'")),
        "nil"
    );
    assert_eq!(
        read_value(&image, &owners.replace("EVENT", "'kicked_probe'")),
        ""
    );
    assert!(image
        .eval(
            "return remuda._dispatch_extension_command('sample-cmd')",
            None
        )
        .is_err());
}
