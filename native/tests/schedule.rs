use remuda_core::Registry;
use remuda_native::{image::Image, tick::Counters};
use std::path::Path;
use std::sync::Arc;

fn schedule_image() -> Image {
    Image::spawn(
        Path::new("/tmp/remuda-schedule-after.sock"),
        Arc::new(Registry::new()),
        Arc::new(Counters::default()),
    )
}

fn eval(image: &Image, code: &str) {
    image.eval(code, None).expect("schedule Lua assertion");
}

#[test]
fn after_is_seconds_from_creation_and_every_applies_after_the_first_fire() {
    let image = schedule_image();
    eval(&image, "remuda._run_due_schedules(100)");
    eval(
        &image,
        r#"
        local fires = 0
        local handle = remuda.schedule({
          every = 10,
          after = 5,
          run = function() fires = fires + 1 end,
        })
        remuda._run_due_schedules(100)
        assert(fires == 0, "after must replace the uptime-based first deadline")
        remuda._run_due_schedules(104.9)
        assert(fires == 0, "schedule fired before after seconds elapsed")
        remuda._run_due_schedules(105)
        assert(fires == 1, "schedule did not fire at creation time plus after")
        remuda._run_due_schedules(114.9)
        assert(fires == 1, "every must wait after the first fire")
        remuda._run_due_schedules(115)
        assert(fires == 2, "every must govern subsequent firings")
        remuda.cancel(handle)
        "#,
    );
}

#[test]
fn omitted_after_keeps_the_uptime_dependent_first_fire() {
    let image = schedule_image();
    eval(&image, "remuda._run_due_schedules(100)");
    eval(
        &image,
        r#"
        local fires = 0
        local handle = remuda.schedule({ every = 10, run = function() fires = fires + 1 end })
        remuda._run_due_schedules(100)
        assert(fires == 1, "without after, an already-uptime-due schedule fires on the next tick")
        remuda.cancel(handle)
        "#,
    );
}

#[test]
fn after_must_be_a_nonnegative_number() {
    let image = schedule_image();
    eval(
        &image,
        r#"
        for _, after in ipairs({ "5", -1 }) do
          local ok = pcall(remuda.schedule, {
            every = 10, after = after, run = function() end,
          })
          assert(not ok, "invalid after value was accepted")
        end
        "#,
    );
}

#[test]
fn each_due_schedule_gets_a_fresh_instruction_budget() {
    let image = schedule_image();
    eval(
        &image,
        r#"
        local total = 0
        for _ = 1, 4 do
          remuda.schedule({
            every = 1,
            run = function()
              local sum = 0
              for i = 1, 100000 do sum = sum + i end
              total = total + sum
            end,
          })
        end
        remuda._run_due_schedules(10)
        assert(total == 20000200000, "each due callback should complete within its own budget")
        "#,
    );
}
