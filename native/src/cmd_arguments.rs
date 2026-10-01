//! Which arguments may go to a Windows batch file (`.cmd`, `.bat`) through the
//! pty. Windows runs a batch file with cmd.exe, which reads the command line
//! by its own rules, while the pty quotes for ordinary programs. Where the two
//! disagree an argument can become a second command (the CVE-2024-24576
//! class). Plain string logic with no OS call, tested on every platform; only
//! the Windows spawn path calls it.

/// Whether Windows runs `program` with cmd.exe: the name ends in `.cmd` or
/// `.bat`, in any case. Windows ignores trailing dots and spaces in a name.
pub fn is_batch_file(program: &str) -> bool {
    let name = program.trim_end_matches(['.', ' ']).to_ascii_lowercase();
    name.ends_with(".cmd") || name.ends_with(".bat")
}

/// The file name portable-pty tries for one PATHEXT entry (portable-pty 0.9.0,
/// `cmdbuilder.rs:594-598`): the entry without its FIRST character, whatever
/// it is, replaces the extension. `None` where portable-pty itself panics.
pub fn pty_candidate(base: &std::path::Path, entry: &str) -> Option<std::path::PathBuf> {
    Some(base.with_extension(entry.get(1..)?))
}

/// The text portable-pty puts on the command line for `arg` (portable-pty
/// 0.9.0, `cmdbuilder.rs:702-745`): bare unless empty or holding a space,
/// tab, LF, VT or `"`; inside quotes `"` becomes `\"`.
pub fn pty_text(arg: &str) -> String {
    let bare = !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']);
    if bare {
        return arg.to_owned();
    }
    let mut text = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        // Backslashes before a quote are doubled, and the quote escaped.
        let kept = if c == '"' {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        text.extend(std::iter::repeat_n('\\', kept));
        text.push(c);
        backslashes = 0;
    }
    text.extend(std::iter::repeat_n('\\', backslashes * 2));
    text.push('"');
    text
}

/// Why `arg` cannot be given to a batch file, or `None` when it can. cmd.exe
/// does not know `\"`: every `"` flips its quote state, so the text is walked
/// that way and a special character outside quotes is refused.
pub fn cmd_argument_refusal(arg: &str) -> Option<&'static str> {
    if arg.contains(['\r', '\n']) {
        return Some("it contains a line break");
    }
    // cmd.exe expands %NAME% inside quotes too.
    if arg.contains('%') {
        return Some("it contains %");
    }
    let mut quoted = false;
    for c in pty_text(arg).chars() {
        match c {
            '"' => quoted = !quoted,
            // `!` is left alone: delayed expansion is off unless the machine
            // turns it on for every cmd.exe.
            '&' | '|' | '<' | '>' | '^' | '(' | ')' if !quoted => {
                return Some("it has a cmd.exe special character outside quotes");
            }
            _ => {}
        }
    }
    // An odd count would flip the quote state of every later argument.
    quoted.then_some("its double quotes do not pair up")
}

/// The one-line refusal for the argv of a batch file, or `None` when every
/// word is safe. It names the position, never the text: that may be a prompt.
pub fn batch_arguments_refusal(argv: &[String]) -> Option<String> {
    argv.iter().enumerate().find_map(|(index, word)| {
        let reason = cmd_argument_refusal(word)?;
        Some(match index {
            0 => format!(
                "the path of a .cmd or .bat program cannot be passed to cmd.exe safely: \
                 {reason}. Next: move or rename the folder, or start the .exe."
            ),
            _ => format!(
                "argument {index} cannot be passed to a .cmd or .bat program safely: \
                 {reason}. Next: start the .exe, or pass this text in a file."
            ),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The member prompt Butler passes today: double quotes, `$`, backticks
    /// and parentheses, all of which stay inside quotes for cmd.exe.
    const MEMBER_PROMPT: &str = "Use `remuda butler inbox`, `remuda butler send MEMBER \
        \"MESSAGE\"`, and `remuda butler send-to-leader RESULT...` for coordination; answer \
        mail with `remuda butler reply MESSAGE-ID -` (or `--file PATH`). Long bodies use stdin \
        (`-`) or `--file \"$PWD/path\"`; message bodies are limited to 64 KiB. Your leader is \
        team-lead.";

    #[test]
    fn the_pty_text_follows_portable_pty() {
        for (arg, text) in [
            ("plain", "plain"),
            ("a&b", "a&b"),
            ("", "\"\""),
            ("two words", "\"two words\""),
            ("say \"hi\"", "\"say \\\"hi\\\"\""),
            ("dir\\", "dir\\"),
            ("a b\\", "\"a b\\\\\""),
            ("a\\\"b", "\"a\\\\\\\"b\""),
        ] {
            assert_eq!(pty_text(arg), text, "{arg:?}");
        }
    }

    #[test]
    fn arguments_cmd_reads_as_text_pass() {
        for arg in [
            "plain",
            "",
            "two words",
            "say \"hi\"",
            "a b&c",
            // It holds a space, so the pty quotes it: measured intact on Windows.
            "a&echo INJECTED>marker.txt",
            "a ^b",
            "dir\\",
            "a b\\",
            "wait!",
            "--model",
            MEMBER_PROMPT,
        ] {
            assert_eq!(cmd_argument_refusal(arg), None, "{arg:?}");
        }
    }

    #[test]
    fn arguments_cmd_would_act_on_are_refused() {
        let outside = "it has a cmd.exe special character outside quotes";
        for (arg, reason) in [
            (
                "line1\necho INJECTED>marker.txt",
                "it contains a line break",
            ),
            ("one\rtwo", "it contains a line break"),
            ("100%", "it contains %"),
            ("a %OS% b", "it contains %"),
            ("a&echo.INJECTED>marker.txt", outside),
            ("\"&echo INJECTED>marker.txt&rem ", outside),
            ("x\"y&z", outside),
            ("^", outside),
            ("a|b", outside),
            ("(x)", outside),
            ("x\"y", "its double quotes do not pair up"),
        ] {
            assert_eq!(cmd_argument_refusal(arg), Some(reason), "{arg:?}");
        }
    }

    #[test]
    fn a_batch_file_is_known_by_its_name() {
        for program in ["a.cmd", "A.CMD", r"C:\npm\claude.Bat", "a.cmd.", "a.bat "] {
            assert!(is_batch_file(program), "{program:?}");
        }
        for program in ["claude", "claude.exe", "cmd", "a.cmd.exe", ""] {
            assert!(!is_batch_file(program), "{program:?}");
        }
    }

    #[test]
    fn the_refusal_is_one_line_that_names_the_position_and_not_the_text() {
        let argv = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        assert_eq!(
            batch_arguments_refusal(&argv(&["a.cmd", "ok", "two words"])),
            None
        );
        let refusal = batch_arguments_refusal(&argv(&["a.cmd", "ok", "secret%text"])).unwrap();
        assert_eq!(
            refusal,
            "argument 2 cannot be passed to a .cmd or .bat program safely: it contains %. \
             Next: start the .exe, or pass this text in a file."
        );
        assert!(!refusal.contains("secret") && !refusal.contains('\n'));
        let refusal = batch_arguments_refusal(&argv(&[r"C:\a&b\x.cmd"])).unwrap();
        assert_eq!(
            refusal,
            "the path of a .cmd or .bat program cannot be passed to cmd.exe safely: it has a \
             cmd.exe special character outside quotes. Next: move or rename the folder, or \
             start the .exe."
        );
    }

    #[test]
    fn a_pathext_entry_gives_the_candidate_portable_pty_tries() {
        let base = std::path::Path::new("tool.cmd");
        for (entry, name) in [
            (".CMD", Some("tool.CMD")),
            // Not "tool.cmd": the first character is dropped whatever it is.
            ("cmd", Some("tool.md")),
            ("..cmd", Some("tool..cmd")),
            ("", None),
        ] {
            let candidate = pty_candidate(base, entry);
            assert_eq!(
                candidate.as_deref(),
                name.map(std::path::Path::new),
                "{entry:?}"
            );
        }
    }
}
