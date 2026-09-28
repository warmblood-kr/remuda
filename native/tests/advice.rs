//! Hook-design (c): nadvice-style advice on named `remuda.*` functions.

use remuda_core::Registry;
use remuda_native::{image::Image, tick::Counters};
use std::path::Path;
use std::sync::Arc;

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
