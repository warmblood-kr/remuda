//! Hook-design (c): nadvice-style advice on named `remuda.*` functions.

use remuda_core::Registry;
use remuda_native::{image::Image, tick::Counters};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct IsolatedModHome {
    root: std::path::PathBuf,
    old_home: Option<std::ffi::OsString>,
    old_xdg_data_home: Option<std::ffi::OsString>,
    _lock: MutexGuard<'static, ()>,
}

impl IsolatedModHome {
    fn new(test_name: &str) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("remuda-{test_name}-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(root.join("data")).unwrap();
        let old_home = std::env::var_os("HOME");
        let old_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &root);
        std::env::set_var("XDG_DATA_HOME", root.join("data"));
        Self {
            root,
            old_home,
            old_xdg_data_home,
            _lock: lock,
        }
    }
}

impl Drop for IsolatedModHome {
    fn drop(&mut self) {
        match self.old_home.take() {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match self.old_xdg_data_home.take() {
            Some(value) => std::env::set_var("XDG_DATA_HOME", value),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn image(tag: &str) -> Image {
    let image = Image::spawn(
        Path::new(&format!("/tmp/remuda-advice-{tag}.sock")),
        Arc::new(Registry::new()),
        Arc::new(Counters::default()),
    );
    read(&image, include_str!("api/v3.lua"));
    image
}

fn read(image: &Image, code: &str) -> String {
    image.eval(code, None).expect("eval in the image")
}

#[test]
fn each_kind_of_advice_wraps_the_base_as_nadvice_does() {
    let image = image("kinds");
    read(
        &image,
        "remuda.t = {} function remuda.t.f(x) return 'base:' .. x end",
    );
    let call = |setup: &str| {
        read(
            &image,
            &format!(
                "remuda._log = {{}} {setup} local r = tostring(remuda.t.f('x')) \
                 for _, a in ipairs(remuda.advice_list('remuda.t.f')) do remuda.unadvise('remuda.t.f', a.id) end \
                 return r .. '|' .. table.concat(remuda._log, ',')"
            ),
        )
    };
    let log = "table.insert(remuda._log, 'a')";
    assert_eq!(
        call(&format!(
            "remuda.advise('remuda.t.f', 'before', function() {log} end, {{ id = 'a' }})"
        )),
        "base:x|a"
    );
    assert_eq!(
        call(&format!(
            "remuda.advise('remuda.t.f', 'after', function() {log} end, {{ id = 'a' }})"
        )),
        "base:x|a"
    );
    assert_eq!(call("remuda.advise('remuda.t.f', 'around', function(orig, x) return '[' .. orig(x .. '!') .. ']' end, { id = 'a' })"), "[base:x!]|");
    assert_eq!(call("remuda.advise('remuda.t.f', 'override', function(x) return 'over:' .. x end, { id = 'a' })"), "over:x|");
    assert_eq!(call("remuda.advise('remuda.t.f', 'filter_args', function(x) return x .. 'y' end, { id = 'a' })"), "base:xy|");
    assert_eq!(call("remuda.advise('remuda.t.f', 'filter_return', function(r) return r .. '!' end, { id = 'a' })"), "base:x!|");
    assert_eq!(call("remuda.advise('remuda.t.f', 'before_while', function() return false end, { id = 'a' })"), "false|");
    assert_eq!(
        call(
            "remuda.advise('remuda.t.f', 'before_while', function() return true end, { id = 'a' })"
        ),
        "base:x|"
    );
    assert_eq!(call("remuda.advise('remuda.t.f', 'before_until', function() return 'early' end, { id = 'a' })"), "early|");
    assert_eq!(
        call(
            "remuda.advise('remuda.t.f', 'before_until', function() return nil end, { id = 'a' })"
        ),
        "base:x|"
    );
    // With no advice left, the slot holds the original function again.
    assert_eq!(
        read(
            &image,
            "return tostring(remuda.advice_member('remuda.t.f', 'a'))"
        ),
        "false"
    );
    assert_eq!(read(&image, "return remuda.t.f('z')"), "base:z");
}

#[test]
fn depth_orders_the_chain_and_an_id_replaces() {
    let image = image("depth");
    read(&image, "function remuda._adv_target(x) return x end");
    read(
        &image,
        "local wrap = function(tag) return function(orig, x) return tag .. '(' .. orig(x) .. ')' end end \
         remuda.advise('remuda._adv_target', 'around', wrap('inner'), { id = 'inner', depth = 100 }) \
         remuda.advise('remuda._adv_target', 'around', wrap('outer'), { id = 'outer', depth = -100 }) \
         remuda.advise('remuda._adv_target', 'around', wrap('mid'), { id = 'mid' }) \
         remuda.advise('remuda._adv_target', 'around', wrap('MID'), { id = 'mid' })",
    );
    assert_eq!(
        read(&image, "return remuda._adv_target('x')"),
        "outer(MID(inner(x)))"
    );
    assert_eq!(
        read(&image, "local ids = {} for _, a in ipairs(remuda.advice_list('remuda._adv_target')) do ids[#ids + 1] = a.id .. ':' .. a.depth end return table.concat(ids, ',')"),
        "outer:-100,mid:0,inner:100"
    );
}

#[test]
fn advice_errors_name_the_chain_and_bad_calls_are_refused() {
    let image = image("errors");
    read(
        &image,
        "function remuda._adv_boom() error('base failed', 0) end",
    );
    read(&image, "remuda.advise('remuda._adv_boom', 'around', function(orig) return orig() end, { id = 'wrapper', depth = 5 })");
    let error = image.eval("remuda._adv_boom()", None).unwrap_err();
    assert!(error.contains("base failed"), "{error}");
    assert!(
        error.contains("<- advice wrapper (around, depth 5) on remuda._adv_boom"),
        "{error}"
    );

    for bad in [
        "remuda.advise('remuda._nothing_here', 'around', function() end, { id = 'x' })",
        "remuda.advise('remuda._adv_boom', 'sideways', function() end, { id = 'x' })",
        "remuda.advise('remuda._adv_boom', 'around', function() end, {})",
        "remuda.advise('os.exit', 'around', function() end, { id = 'x' })",
    ] {
        assert!(image.eval(bad, None).is_err(), "should refuse: {bad}");
    }
}

/// A mod that redefines an advised function on load keeps its advice: the
/// new definition becomes the base (Emacs `defalias` respecting advice).
#[test]
fn advice_survives_a_mod_redefining_the_function() {
    let isolated_home = IsolatedModHome::new("advice-reattach");
    let root = &isolated_home.root;
    let mod_dir = root.join("data/remuda/mods/host");
    std::fs::create_dir_all(mod_dir.join("packages/host")).unwrap();
    std::fs::write(
        mod_dir.join("extension.toml"),
        "name = \"host\"\nentry = \"packages/host/init.lua\"\napi = \"remuda-lua-v1\"\n",
    )
    .unwrap();
    let entry = mod_dir.join("packages/host/init.lua");
    let image = image("reattach");
    std::fs::write(&entry, "function remuda._adv_host(x) return 'v1:' .. x end").unwrap();
    read(&image, "remuda.exec('host')");
    read(&image, "remuda.advise('remuda._adv_host', 'around', function(orig, x) return '[' .. orig(x) .. ']' end, { id = 'wrap' })");
    std::fs::write(&entry, "function remuda._adv_host(x) return 'v2:' .. x end").unwrap();
    read(&image, "remuda.exec('host')");
    assert_eq!(read(&image, "return remuda._adv_host('x')"), "[v2:x]");
}

/// PR #144 review: the monkey-patch idiom `local orig = remuda.x; function
/// remuda.x(...) return orig(...) end` over an advised function must
/// terminate. The captured `orig` is the old trampoline and keeps its old
/// composition (Emacs-like), so the advice runs twice, around each layer.
#[test]
fn a_monkey_patch_over_advice_terminates() {
    let image = image("monkey");
    read(
        &image,
        "remuda._runs = 0 function remuda._adv_x(v) return v end",
    );
    read(&image, "remuda.advise('remuda._adv_x', 'around', function(orig, v) remuda._runs = remuda._runs + 1 return 'a(' .. orig(v) .. ')' end, { id = 'a' })");
    read(&image, "local orig = remuda._adv_x; function remuda._adv_x(v) return 'm(' .. orig(v) .. ')' end; remuda._advice_reattach()");
    let got = image.eval(
        "return remuda._adv_x('x') .. ' runs=' .. remuda._runs",
        None,
    );
    assert_eq!(got.as_deref(), Ok("a(m(a(x))) runs=2"), "{got:?}");
    // Putting one of our own trampolines back is not a redefinition.
    read(
        &image,
        "remuda._runs = 0; local t = remuda._adv_x; remuda._adv_x = t; remuda._advice_reattach()",
    );
    assert_eq!(
        read(
            &image,
            "return remuda._adv_x('y') .. ' runs=' .. remuda._runs"
        ),
        "a(m(a(y))) runs=2"
    );
}
