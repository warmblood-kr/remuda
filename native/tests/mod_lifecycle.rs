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
