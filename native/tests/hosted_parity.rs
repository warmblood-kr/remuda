//! Host-backed sessions behave like in-process ones (#713). Each test runs the
//! same scenario against both backends; see `daemon_support/parity.rs`.
#![cfg(unix)]

use remuda_core::protocol::{Request, Response};

#[path = "daemon_support/parity.rs"]
mod parity;
use parity::{Parity, MODES};

#[test]
fn styled_capture_reads_the_screen_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        daemon.shell("sh");
        daemon
            .eval(r"remuda.send('sh', [[printf '\033[1mBO''LD\033[0m pl''ain\n']])")
            .unwrap();
        daemon.wait_for("sh", "BOLD plain");

        let Response::StyledScreen { rows, cursor, .. } = daemon.request(&Request::CaptureStyled {
            name: "sh".into(),
            scrollback: 0,
        }) else {
            panic!("{mode:?}: styled capture refused");
        };
        assert_eq!(rows.len(), 24, "{mode:?}");
        let text: Vec<String> = rows
            .iter()
            .map(|runs| runs.iter().map(|run| run.text.as_str()).collect())
            .collect();
        assert!(
            text.iter().any(|row| row.contains("BOLD plain")),
            "{mode:?}: {text:?}"
        );
        assert!(cursor.row < 24 && cursor.col < 80, "{mode:?}: {cursor:?}");

        // The Lua word that readiness checks are built on.
        assert_eq!(
            daemon.eval("return tostring(#remuda.capture_styled('sh').rows)"),
            Ok("24".into()),
            "{mode:?}"
        );
    }
}

#[test]
fn lua_input_words_reach_the_session_in_both_backends() {
    for mode in MODES {
        let daemon = Parity::start(mode);
        let instance = daemon.shell("sh");

        // Text burst, then a verified submit.
        daemon
            .eval("remuda.input.text('sh', [[echo TEXT:$((1+2))]])")
            .unwrap();
        let submitted = daemon
            .eval("return remuda.input.submit('sh', [[echo TEXT:$((1+2))]])")
            .unwrap();
        assert!(
            ["submitted", "unverified"].contains(&submitted.as_str()),
            "{mode:?}: {submitted}"
        );
        daemon.wait_for("sh", "TEXT:3");

        // The composite type-and-submit act.
        let typed = daemon
            .eval("return remuda.type_text('sh', [[echo TYPED:$((2+3))]], 0)")
            .unwrap();
        assert!(
            ["submitted", "unverified"].contains(&typed.as_str()),
            "{mode:?}: {typed}"
        );
        daemon.wait_for("sh", "TYPED:5");

        // Pinned to the launch: the right instance writes, a wrong one never does.
        daemon
            .eval(&format!(
                "return remuda._input_type_text_if_instance('sh', '{instance}', [[echo FENCED:$((3+4))]], 0)"
            ))
            .unwrap();
        daemon.wait_for("sh", "FENCED:7");
        let wrong = daemon
            .eval("return remuda._input_type_text_if_instance('sh', 'not-this-launch', [[echo WRONG:$((4+5))]], 0)")
            .unwrap_err();
        assert!(wrong.contains("instance changed"), "{mode:?}: {wrong}");
        let wrong_key = daemon
            .eval("return remuda._input_key_if_instance('sh', 'not-this-launch', 'RET')")
            .unwrap_err();
        assert!(
            wrong_key.contains("instance changed"),
            "{mode:?}: {wrong_key}"
        );
        assert_eq!(
            daemon.eval(&format!(
                "return tostring(remuda._input_key_if_instance('sh', '{instance}', 'RET'))"
            )),
            Ok("true".into()),
            "{mode:?}"
        );

        let missing = daemon.eval("remuda.input.text('nobody', 'x')").unwrap_err();
        assert!(missing.contains("no such session"), "{mode:?}: {missing}");

        // Let a late write show before checking that the refused one never landed.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!daemon.screen("sh").contains("WRONG:9"), "{mode:?}");
    }
}
