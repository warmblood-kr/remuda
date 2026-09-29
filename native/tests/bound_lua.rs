use remuda_core::Registry;
use remuda_native::{image::Image, tick::Counters};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn an_infinite_tool_callback_errors_and_the_image_answers_the_next_request() {
    let image = Image::spawn(
        Path::new("/tmp/remuda-bound-lua-test.sock"),
        Arc::new(Registry::new()),
        Arc::new(Counters::default()),
    );
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
        .recv_timeout(Duration::from_secs(4))
        .expect("the infinite callback returns within its budget")
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
