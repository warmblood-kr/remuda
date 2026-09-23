use remuda_core::Registry;
use remuda_native::{image::Image, tick::Counters};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

struct DataHome {
    root: PathBuf,
    old_data: Option<std::ffi::OsString>,
}

impl DataHome {
    fn new() -> Self {
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
        Self { root, old_data }
    }

    fn entry(&self) -> PathBuf {
        self.root
            .join("remuda/extensions/sample/packages/sample/init.lua")
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
fn installed_mod_reloads_in_the_same_image_without_losing_state_or_old_code_on_failure() {
    let home = DataHome::new();
    let manifest = home.root.join("remuda/extensions/sample/extension.toml");
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
    read_value(&image, "remuda.reload('sample')");
    read_value(&image, "remuda.emit('probe')");
    assert_eq!(
        read_value(&image, "return remuda.tools.sample_status()"),
        "11"
    );

    write_entry(
        &entry,
        r#"return {
          api = "remuda-module-v1", state_version = 2,
          initialize = function() return {} end,
          migrations = {[1] = function(state)
            state.count = -1
            error("intentional migration failure")
          end},
          hooks = {}, tools = {},
        }"#,
    );
    assert!(image.eval("remuda.reload('sample')", None).is_err());
    read_value(&image, "remuda.emit('probe')");
    assert_eq!(
        read_value(&image, "return remuda.tools.sample_status()"),
        "21"
    );

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
        "31"
    );

    fs::write(
        &manifest,
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\n",
    )
    .unwrap();
    assert!(image.eval("remuda.reload('sample')", None).is_err());
    assert_eq!(
        read_value(&image, "return remuda.tools.sample_status()"),
        "31"
    );
}
