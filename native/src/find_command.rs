//! Where a command is on PATH, without a shell and without running it.
//!
//! The OS rule is an argument, not a `cfg`: both the Unix and the Windows
//! search are plain string logic over PATH, tested on every platform.

/// Why no path was returned. `Display` is one line with a `Next:` step.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    BareName,
    NoPath,
    NotFound(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BareName => f.write_str(
                "find_command needs a bare command name, not a path. \
                 Next: pass the name only, for example claude.",
            ),
            Self::NoPath => f.write_str(
                "PATH is not set for the remuda daemon. \
                 Next: start the daemon from a shell where PATH is set.",
            ),
            Self::NotFound(name) => write!(
                f,
                "'{name}' is not on PATH. \
                 Next: install it, or add its folder to PATH and restart remuda."
            ),
        }
    }
}

/// The environment a search runs in. `windows` picks the rule: `;` and
/// PATHEXT, or `:` and the name as given.
pub struct Rules<'a> {
    pub windows: bool,
    pub path: Option<&'a str>,
    pub pathext: Option<&'a str>,
}

/// What cmd.exe uses when PATHEXT is unset or empty.
pub const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// The first PATH candidate that `is_command` accepts. Only absolute PATH
/// entries are searched, never the current directory; nothing is executed.
pub fn find(
    name: &str,
    rules: &Rules,
    is_command: &dyn Fn(&str) -> bool,
) -> Result<String, Refusal> {
    let _ = (rules, is_command);
    Err(Refusal::NotFound(name.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::{find, Refusal, Rules};

    fn unix(path: &str) -> Rules<'_> {
        Rules {
            windows: false,
            path: Some(path),
            pathext: None,
        }
    }

    fn windows<'a>(path: &'a str, pathext: Option<&'a str>) -> Rules<'a> {
        Rules {
            windows: true,
            path: Some(path),
            pathext,
        }
    }

    /// A file system that holds exactly these commands.
    fn only(present: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |candidate| present.contains(&candidate)
    }

    #[test]
    fn unix_returns_the_first_match_in_path_order() {
        let fs = only(&["/usr/bin/claude", "/opt/bin/claude", "/opt/bin/codex"]);
        let rules = unix("/usr/local/bin:/usr/bin:/opt/bin");
        assert_eq!(find("claude", &rules, &fs).unwrap(), "/usr/bin/claude");
        assert_eq!(find("codex", &rules, &fs).unwrap(), "/opt/bin/codex");
        assert_eq!(
            find("gemini", &rules, &fs),
            Err(Refusal::NotFound("gemini".into()))
        );
        // A trailing separator on the entry does not double it.
        assert_eq!(
            find("claude", &unix("/usr/bin/"), &fs).unwrap(),
            "/usr/bin/claude"
        );
    }

    #[test]
    fn relative_and_empty_path_entries_are_never_searched() {
        // Everything "exists": only the absolute entry may answer.
        let everything = |_: &str| true;
        let rules = unix(":.:bin:./bin:../bin:/abs/bin:");
        assert_eq!(find("tool", &rules, &everything).unwrap(), "/abs/bin/tool");
        let rules = unix(":.:bin");
        assert_eq!(
            find("tool", &rules, &everything),
            Err(Refusal::NotFound("tool".into()))
        );
        let rules = windows(r";.;bin;.\bin;\bin;C:bin;C:\abs\bin", None);
        assert_eq!(
            find("tool", &rules, &everything).unwrap(),
            r"C:\abs\bin\tool.COM"
        );
        // Windows searches the current directory first; this word never does.
        let rules = windows(r";.;\bin;C:bin", None);
        assert_eq!(
            find("tool", &rules, &everything),
            Err(Refusal::NotFound("tool".into()))
        );
    }

    #[test]
    fn only_a_bare_name_is_searched() {
        let everything = |_: &str| true;
        for name in [
            "", ".", "..", "a/b", "/bin/sh", r"a\b", "./tool", "a\0b", "a\nb",
        ] {
            for rules in [unix("/bin"), windows(r"C:\bin", None)] {
                assert_eq!(
                    find(name, &rules, &everything),
                    Err(Refusal::BareName),
                    "{name:?}"
                );
            }
        }
        // A drive or stream colon makes it a path on Windows only.
        assert_eq!(
            find("C:tool", &windows(r"C:\bin", None), &everything),
            Err(Refusal::BareName)
        );
        assert_eq!(find("a:b", &unix("/bin"), &everything).unwrap(), "/bin/a:b");
        let long = "x".repeat(256);
        assert_eq!(
            find(&long, &unix("/bin"), &everything),
            Err(Refusal::BareName)
        );
    }

    #[test]
    fn a_missing_path_has_its_own_reason() {
        let everything = |_: &str| true;
        let rules = Rules {
            windows: false,
            path: None,
            pathext: None,
        };
        assert_eq!(find("tool", &rules, &everything), Err(Refusal::NoPath));
    }

    #[test]
    fn windows_tries_pathext_in_order_with_a_default() {
        let fs = only(&[
            r"C:\bin\claude.CMD",
            r"C:\bin\claude.EXE",
            r"C:\tools\codex.CMD",
        ]);
        let rules = windows(r"C:\empty;C:\bin;C:\tools", None);
        assert_eq!(find("claude", &rules, &fs).unwrap(), r"C:\bin\claude.EXE");
        assert_eq!(find("codex", &rules, &fs).unwrap(), r"C:\tools\codex.CMD");
        // An empty PATHEXT is the default list too.
        let rules = windows(r"C:\bin", Some(""));
        assert_eq!(find("claude", &rules, &fs).unwrap(), r"C:\bin\claude.EXE");
        // The caller's PATHEXT decides the order; entries without a dot are skipped.
        let rules = windows(r"C:\bin", Some("junk;.CMD;.EXE"));
        assert_eq!(find("claude", &rules, &fs).unwrap(), r"C:\bin\claude.CMD");
        // The bare name alone is not a command on Windows.
        let fs = only(&[r"C:\bin\claude"]);
        assert_eq!(
            find("claude", &windows(r"C:\bin", None), &fs),
            Err(Refusal::NotFound("claude".into()))
        );
    }

    #[test]
    fn windows_accepts_a_name_that_already_has_a_pathext_extension() {
        let fs = only(&[r"C:\bin\claude.cmd", r"C:\bin\notes.txt"]);
        let rules = windows(r"C:\bin", None);
        // Matched without regard to case, returned as the caller wrote it.
        assert_eq!(
            find("claude.cmd", &rules, &fs).unwrap(),
            r"C:\bin\claude.cmd"
        );
        // .txt is not an executable extension.
        assert_eq!(
            find("notes.txt", &rules, &fs),
            Err(Refusal::NotFound("notes.txt".into()))
        );
        // A forward slash or a UNC share is absolute too.
        let fs = only(&["C:/bin/claude.EXE", r"\\server\share\codex.EXE"]);
        let rules = windows(r"C:/bin;\\server\share", None);
        assert_eq!(find("claude", &rules, &fs).unwrap(), "C:/bin/claude.EXE");
        assert_eq!(
            find("codex", &rules, &fs).unwrap(),
            r"\\server\share\codex.EXE"
        );
    }

    #[test]
    fn every_reason_is_one_line_with_a_next_step() {
        for refusal in [
            Refusal::BareName,
            Refusal::NoPath,
            Refusal::NotFound("x".into()),
        ] {
            let text = refusal.to_string();
            assert!(text.contains("Next: ") && !text.contains('\n'), "{text}");
        }
    }
}
