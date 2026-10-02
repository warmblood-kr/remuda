use remuda_core::Registry;
use remuda_native::{image::Image, tick::Counters};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn image() -> Image {
    Image::spawn(
        Path::new("/tmp/remuda-bound-lua-test.sock"),
        Arc::new(Registry::new()),
        Arc::new(Counters::default()),
    )
}

fn expect_bounded_error(code: &str) {
    let image = image();
    let answer = image.submit(code, None).expect("queue bounded regression");
    let error = answer
        .recv_timeout(Duration::from_secs(15))
        .expect("Lua execution returns within the test bound")
        .expect_err("the script must exceed the instruction budget");
    assert!(
        error.contains("execution limit"),
        "unexpected error: {error}"
    );
}

#[test]
fn pcall_cannot_swallow_the_instruction_limit() {
    expect_bounded_error("return pcall(function() while true do end end)");
}

#[test]
fn xpcall_error_handler_cannot_swallow_the_instruction_limit() {
    expect_bounded_error(
        "return xpcall(function() error('trigger') end, function() while true do end end)",
    );
}

#[test]
fn coroutine_wrap_loop_is_bounded() {
    expect_bounded_error("return coroutine.wrap(function() while true do end end)()");
}

#[test]
fn a_coroutine_resumed_in_a_later_job_is_bounded() {
    let image = image();
    image
        .eval(
            "saved_coroutine = coroutine.create(function() while true do end end)",
            None,
        )
        .expect("create coroutine for a later job");
    let answer = image
        .submit("return coroutine.resume(saved_coroutine)", None)
        .expect("queue resume");
    let error = answer
        .recv_timeout(Duration::from_secs(15))
        .expect("resumed coroutine returns within the test bound")
        .expect_err("the coroutine must exceed the instruction budget");
    assert!(
        error.contains("execution limit"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_timer_wait_does_not_consume_the_instruction_budget() {
    let image = image();
    let answer = image
        .submit(
            "remuda.after(2.2, function() local sum = 0; for i = 1, 100000 do sum = sum + i end; remuda._timer_test_sum = sum end); return 'scheduled'",
            None,
        )
        .expect("queue timer and return without waiting for it");
    assert_eq!(
        answer
            .recv_timeout(Duration::from_secs(1))
            .expect("scheduling a timer must return promptly")
            .expect("schedule timer"),
        "scheduled"
    );

    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let answer = image
            .submit("return tostring(remuda._timer_test_sum)", None)
            .expect("queue timer result query");
        let result = answer
            .recv_timeout(Duration::from_secs(1))
            .expect("timer result query returns promptly")
            .expect("read timer result");
        if result != "nil" {
            assert_eq!(result, "5000050000");
            break;
        }
        assert!(Instant::now() < deadline, "timer callback did not run");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn nested_protected_calls_share_the_outer_instruction_budget() {
    expect_bounded_error(
        "return xpcall(function() return pcall(function() while true do end end) end, function() return 'caught' end)",
    );
}

#[test]
fn a_million_step_lua_loop_fits_the_budget() {
    let image = image();
    let answer = image
        .submit(
            "local sum = 0; for i = 1, 1000000 do sum = sum + i end; return sum",
            None,
        )
        .expect("queue ordinary Lua work");
    assert_eq!(
        answer
            .recv_timeout(Duration::from_secs(15))
            .expect("million-step loop completes within the test bound")
            .expect("ordinary Lua work fits the execution budget"),
        "500000500000"
    );
}

#[test]
fn invoking_the_lua_schedule_helper_cannot_reset_the_job_budget() {
    expect_bounded_error(
        "local now = 0; remuda.schedule({every = 1, run = function() local sum = 0; for i = 1, 1000000 do sum = sum + i end end}); while true do remuda._run_due_schedules(now); now = now + 1 end",
    );
}

#[test]
fn coroutine_fanout_contributes_to_the_shared_job_budget() {
    expect_bounded_error(
        "for i = 1, 12000 do local thread = coroutine.create(function() return true end); coroutine.resume(thread) end",
    );
}

#[test]
fn abandoned_suspended_coroutines_are_not_kept_alive_between_jobs() {
    let image = image();
    image
        .eval(
            "weak_threads = setmetatable({}, {__mode = 'v'}); do local thread = coroutine.create(function() coroutine.yield() end); coroutine.resume(thread); weak_threads[1] = thread end",
            None,
        )
        .expect("create and abandon suspended coroutine");
    image
        .eval(
            "collectgarbage('collect'); assert(weak_threads[1] == nil, 'abandoned coroutine retained by budget tracker')",
            None,
        )
        .expect("budget tracker releases abandoned coroutine");
}

#[test]
fn tool_callback_instruction_limit_is_enforced() {
    let image = image();
    image
        .eval(
            r#"remuda.tool({name = "loop_forever", about = "Test callback budget.", run = function()
              while true do end
            end})"#,
            None,
        )
        .expect("register infinite test tool");

    // Queue both requests in order. The second result proves the image worker
    // recovered after turning the first callback's instruction loop into an error.
    let callback = image
        .submit("return remuda.tools.loop_forever()", None)
        .expect("queue infinite callback");
    let next = image
        .submit(
            "local sum = 0; for i = 1, 20000 do sum = sum + i end; return sum",
            None,
        )
        .expect("queue follow-up request");

    let callback_error = callback
        .recv_timeout(Duration::from_secs(30))
        .expect("the infinite callback returns within its instruction budget")
        .expect_err("the callback must report its execution limit");
    assert!(
        callback_error.contains("execution limit"),
        "unexpected callback error: {callback_error}"
    );
    assert_eq!(
        next.recv_timeout(Duration::from_secs(1))
            .expect("the image answers the next request")
            .expect("follow-up eval succeeds"),
        "200010000"
    );
}
