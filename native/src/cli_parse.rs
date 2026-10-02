use clap::{
    error::{ContextKind, ContextValue, ErrorKind},
    Arg, ArgAction, Command,
};
use serde_json::{Map, Value};

#[derive(Clone, Debug)]
pub struct Spec {
    pub name: String,
    pub options: Vec<OptionSpec>,
    pub verbs: Vec<VerbSpec>,
}

#[derive(Clone, Debug)]
pub struct OptionSpec {
    pub long: String,
    pub short: Option<char>,
    pub value: Option<String>,
    pub help: String,
    pub global: bool,
}

#[derive(Clone, Debug)]
pub struct ArgSpec {
    pub name: String,
    pub help: String,
    pub multiple: bool,
}

#[derive(Clone, Debug)]
pub struct VerbSpec {
    pub name: String,
    pub about: String,
    pub args: Vec<ArgSpec>,
    pub next: String,
    pub options: Vec<OptionSpec>,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub ok: bool,
    pub verb: Option<String>,
    pub values: Map<String, Value>,
    pub kind: Option<String>,
    pub text: String,
    pub code: i32,
}

impl Report {
    fn success(verb: &str, values: Map<String, Value>) -> Self {
        Self {
            ok: true,
            verb: Some(verb.to_owned()),
            values,
            kind: None,
            text: String::new(),
            code: 0,
        }
    }

    fn failure(kind: &str, text: String, code: i32) -> Self {
        Self {
            ok: false,
            verb: None,
            values: Map::new(),
            kind: Some(kind.to_owned()),
            text,
            code,
        }
    }
}

/// Parse a verb's raw argument words with a runtime-built clap command.
/// This function only returns a report; it never prints or exits.
pub fn parse(spec: &Spec, argv: &[&str]) -> Report {
    let Some((verb_index, verb)) = select_verb(spec, argv) else {
        return error_report(spec, None, None);
    };
    let verb_name = verb.name.as_str();

    let command = command_for(spec, verb);
    let input =
        std::iter::once(command.get_name().to_owned()).chain(argv.iter().map(|s| (*s).to_owned()));
    let input: Vec<String> = preserve_help_in_body(spec, verb, verb_index, argv, input.collect());
    match command.try_get_matches_from(input) {
        Ok(matches) => {
            let submatches = matches.subcommand().map(|(_, matches)| matches);
            Report::success(verb_name, collect_values(spec, verb, &matches, submatches))
        }
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            let mut text = error.to_string();
            append_next(&mut text, &verb.next);
            Report::failure("help", text, 0)
        }
        Err(error) => error_report(spec, Some(verb), Some(&error)),
    }
}

fn select_verb<'a>(spec: &'a Spec, argv: &[&str]) -> Option<(usize, &'a VerbSpec)> {
    let mut index = 0;
    while let Some(word) = argv.get(index) {
        if *word == "--" {
            return None;
        }
        if let Some(long) = word.strip_prefix("--") {
            let option_name = long.split('=').next().unwrap_or_default();
            let takes_value = spec
                .options
                .iter()
                .find(|option| option.long == option_name)
                .is_some_and(|option| option.value.is_some());
            index += 1 + usize::from(takes_value && !long.contains('='));
            continue;
        }
        if let Some(short) = word.strip_prefix('-').and_then(|rest| rest.chars().next()) {
            let takes_value = spec
                .options
                .iter()
                .find(|option| option.short == Some(short))
                .is_some_and(|option| option.value.is_some());
            index += 1 + usize::from(takes_value && word.len() == 2);
            continue;
        }
        return spec
            .verbs
            .iter()
            .find(|verb| verb.name == *word)
            .map(|verb| (index, verb));
    }
    None
}

fn command_for(spec: &Spec, verb: &VerbSpec) -> Command {
    let command_name = spec.name.split_whitespace().last().unwrap_or("remuda");
    let usage = format!("{} {}", spec.name, usage_tail(spec, verb));
    let mut child = Command::new(verb.name.clone())
        .about(verb.about.clone())
        .disable_help_flag(true)
        .override_usage(usage)
        .after_help(format!("Next: {}", verb.next))
        .arg(help_arg());
    for option in &verb.options {
        child = child.arg(make_option(option));
    }
    for arg in &verb.args {
        let mut positional = Arg::new(arg.name.clone())
            .help(arg.help.clone())
            .required(true);
        if arg.multiple {
            positional = positional.num_args(1..).trailing_var_arg(true);
        }
        child = child.arg(positional);
    }

    let mut command = Command::new(command_name.to_owned())
        .subcommand_required(true)
        .disable_help_flag(true)
        .subcommand(child);

    for option in &spec.options {
        command = command.arg(make_option(option));
    }
    command
}

fn make_option(option: &OptionSpec) -> Arg {
    let mut arg = Arg::new(option.long.clone())
        .long(option.long.clone())
        .help(option.help.clone())
        .global(option.global);
    if let Some(short) = option.short {
        arg = arg.short(short);
    }
    if let Some(value) = &option.value {
        arg = arg.value_name(value.clone()).action(ArgAction::Set);
    } else {
        arg = arg.action(ArgAction::SetTrue);
    }
    arg
}

fn help_arg() -> Arg {
    Arg::new("__help")
        .short('h')
        .long("help")
        .help("Print help")
        .action(ArgAction::Help)
}

fn usage_tail(spec: &Spec, verb: &VerbSpec) -> String {
    let mut parts = vec![verb.name.clone()];
    if !spec.options.is_empty() || !verb.options.is_empty() {
        parts.push("[OPTIONS]".into());
    }
    parts.extend(verb.args.iter().map(|arg| arg.name.clone()));
    parts.join(" ")
}

fn preserve_help_in_body(
    spec: &Spec,
    verb: &VerbSpec,
    verb_index: usize,
    argv: &[&str],
    mut input: Vec<String>,
) -> Vec<String> {
    // `trailing_var_arg` still recognizes declared `--help` wherever it appears.
    // `allow_hyphen_values` would also accept unknown `--name` flags, so insert `--` for body text.
    if !verb.args.last().is_some_and(|arg| arg.multiple) {
        return input;
    }
    let Some(help_index) = argv
        .iter()
        .enumerate()
        .skip(verb_index + 1)
        .find_map(|(index, arg)| (*arg == "--help").then_some(index))
    else {
        return input;
    };

    let separator_index = argv
        .iter()
        .enumerate()
        .skip(verb_index + 1)
        .find_map(|(index, arg)| (*arg == "--").then_some(index));
    if let Some(separator_index) = separator_index {
        if let Some(body_start) =
            message_body_start(spec, verb, argv, verb_index + 1, separator_index)
        {
            // Once a trailing positional starts, clap treats a later `--` as
            // body text. Move the existing separator ahead of that positional.
            input.remove(separator_index + 1);
            input.insert(body_start + 1, "--".into());
        }
        return input;
    }

    let Some(body_start) = message_body_start(spec, verb, argv, verb_index + 1, help_index) else {
        return input;
    };
    if help_index + 1 < argv.len() {
        // Place the separator before the body so it cannot leak into text.
        input.insert(body_start + 1, "--".into());
    } else {
        // A final `--help` is a request for help even after a trailing body
        // positional has started. Move it before the body so clap sees the flag.
        input.remove(help_index + 1);
        input.insert(verb_index + 2, "--help".into());
    }
    input
}

fn message_body_start(
    spec: &Spec,
    verb: &VerbSpec,
    argv: &[&str],
    start: usize,
    end: usize,
) -> Option<usize> {
    let options = spec.options.iter().chain(verb.options.iter());
    let positionals_before_body = verb.args.len().saturating_sub(1);
    let mut positionals_seen = 0;
    let mut index = start;
    while index < end {
        let word = argv[index];
        if word == "--" {
            return None;
        }
        if let Some(long) = word.strip_prefix("--") {
            let (name, has_inline_value) = long
                .split_once('=')
                .map_or((long, false), |(name, _)| (name, true));
            let option = options.clone().find(|option| option.long == name);
            if option.is_some_and(|option| option.value.is_some() && !has_inline_value) {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if let Some(short) = word.strip_prefix('-').and_then(|rest| rest.chars().next()) {
            let option = options.clone().find(|option| option.short == Some(short));
            if option.is_some_and(|option| option.value.is_some() && word.len() == 2) {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if positionals_seen < positionals_before_body {
            positionals_seen += 1;
            index += 1;
        } else {
            return Some(index);
        }
    }
    None
}

fn collect_values(
    spec: &Spec,
    verb: &VerbSpec,
    root_matches: &clap::ArgMatches,
    matches: Option<&clap::ArgMatches>,
) -> Map<String, Value> {
    let mut values = Map::new();
    for arg in &verb.args {
        if let Some(items) = matches.and_then(|m| m.get_many::<String>(&arg.name)) {
            let items: Vec<Value> = items.cloned().map(Value::String).collect();
            values.insert(
                arg.name.clone(),
                if arg.multiple && items.len() != 1 {
                    Value::Array(items)
                } else {
                    items.into_iter().next().unwrap_or(Value::Null)
                },
            );
        }
    }
    for option in &spec.options {
        collect_option(&mut values, option, Some(root_matches));
    }
    for option in &verb.options {
        collect_option(&mut values, option, matches);
    }
    values
}

fn collect_option(
    values: &mut Map<String, Value>,
    option: &OptionSpec,
    matches: Option<&clap::ArgMatches>,
) {
    let value = if option.value.is_some() {
        matches
            .and_then(|m| m.get_one::<String>(&option.long))
            .cloned()
            .map(Value::String)
    } else {
        Some(Value::Bool(
            matches.is_some_and(|m| m.get_flag(&option.long)),
        ))
    };
    if let Some(value) = value {
        values.insert(option.long.clone(), value);
    }
}

fn error_report(spec: &Spec, verb: Option<&VerbSpec>, error: Option<&clap::Error>) -> Report {
    let Some(verb) = verb else {
        let usage = spec
            .verbs
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        return Report::failure(
            "error",
            format!("unknown verb. Usage: {} <{}>", spec.name, usage),
            2,
        );
    };

    let usage = format!("Usage: {} {}", spec.name, usage_tail(spec, verb));
    let next = format!("Next: {}", verb.next);
    let detail = error
        .map(|error| error_detail(error, spec, verb))
        .unwrap_or_default();
    let text = format!("{detail}\n{usage}\n{next}");
    Report::failure("error", text, 2)
}

fn error_detail(error: &clap::Error, spec: &Spec, verb: &VerbSpec) -> String {
    let invalid_arg = context_text(error, ContextKind::InvalidArg);
    match error.kind() {
        ErrorKind::UnknownArgument => {
            let argument = invalid_arg.as_deref().unwrap_or("unknown argument");
            if argument.starts_with("--") {
                let suggestion = context_text(error, ContextKind::SuggestedArg)
                    .map(|suggested| format!(" Did you mean '{suggested}'?"))
                    .unwrap_or_default();
                let body_tip = if verb.args.last().is_some_and(|arg| arg.multiple) {
                    " put the text after --"
                } else {
                    ""
                };
                format!(
                    "remuda: {} {}: unknown option '{argument}'.{suggestion}{body_tip}",
                    command_prefix(&spec.name),
                    verb.name
                )
            } else {
                format!("unexpected argument '{argument}'")
            }
        }
        ErrorKind::MissingRequiredArgument => format!(
            "missing required argument{}{}",
            if invalid_arg.as_deref().is_some_and(|arg| arg.contains(',')) {
                "s "
            } else {
                " "
            },
            invalid_arg.unwrap_or_else(|| "".into())
        ),
        ErrorKind::InvalidValue => {
            let value = context_text(error, ContextKind::InvalidValue).unwrap_or_default();
            let suggestion = context_text(error, ContextKind::SuggestedValue)
                .map(|suggested| format!(" Did you mean '{suggested}'?"))
                .unwrap_or_default();
            format!(
                "invalid value '{value}' for {}.{suggestion}",
                invalid_arg.unwrap_or_default()
            )
        }
        ErrorKind::NoEquals => format!(
            "option {} requires '=' before its value",
            invalid_arg.unwrap_or_default()
        ),
        kind => format!(
            "invalid arguments: {}",
            kind.as_str().unwrap_or("parse error")
        ),
    }
}

fn context_text(error: &clap::Error, kind: ContextKind) -> Option<String> {
    match error.get(kind)? {
        ContextValue::String(value) => Some(value.clone()),
        ContextValue::Strings(values) => Some(values.join(", ")),
        other => Some(other.to_string()),
    }
}

fn command_prefix(name: &str) -> &str {
    name.strip_prefix("remuda ").unwrap_or(name)
}

fn append_next(text: &mut String, next: &str) {
    if !text.contains("Next:") {
        text.push_str("\nNext: ");
        text.push_str(next);
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, ArgSpec, OptionSpec, Spec, VerbSpec};
    use serde_json::Value;

    fn spec() -> Spec {
        Spec {
            name: "remuda butler matrix".into(),
            options: vec![
                OptionSpec {
                    long: "room".into(),
                    short: None,
                    value: Some("ROOM".into()),
                    help: "Room ID".into(),
                    global: true,
                },
                OptionSpec {
                    long: "json".into(),
                    short: None,
                    value: None,
                    help: "Print JSON".into(),
                    global: true,
                },
            ],
            verbs: vec![
                VerbSpec {
                    name: "thread".into(),
                    about: "Show every reply in a Matrix thread".into(),
                    args: vec![ArgSpec {
                        name: "EVENT_ID".into(),
                        help: "Event that starts the thread".into(),
                        multiple: false,
                    }],
                    next: "remuda butler matrix reply EVENT_ID TEXT".into(),
                    options: vec![],
                },
                VerbSpec {
                    name: "send-to-leader".into(),
                    about: "Send a message to the team leader".into(),
                    args: vec![ArgSpec {
                        name: "TEXT".into(),
                        help: "Message text".into(),
                        multiple: true,
                    }],
                    next: "remuda butler inbox".into(),
                    options: vec![],
                },
            ],
        }
    }

    fn reply_spec() -> Spec {
        Spec {
            name: "remuda butler matrix".into(),
            options: vec![
                OptionSpec {
                    long: "room".into(),
                    short: Some('r'),
                    value: Some("ROOM".into()),
                    help: "Room ID".into(),
                    global: true,
                },
                OptionSpec {
                    long: "verbose".into(),
                    short: Some('v'),
                    value: Some("LEVEL".into()),
                    help: "Verbosity".into(),
                    global: true,
                },
                OptionSpec {
                    long: "json".into(),
                    short: Some('j'),
                    value: None,
                    help: "Print JSON".into(),
                    global: true,
                },
            ],
            verbs: vec![VerbSpec {
                name: "reply".into(),
                about: "Reply to an event".into(),
                args: vec![
                    ArgSpec {
                        name: "EVENT_ID".into(),
                        help: "Event to reply to".into(),
                        multiple: false,
                    },
                    ArgSpec {
                        name: "TEXT".into(),
                        help: "Message text".into(),
                        multiple: true,
                    },
                ],
                next: "remuda butler matrix inbox".into(),
                options: vec![],
            }],
        }
    }

    #[test]
    fn proven_cli_behaviors_are_table_driven() {
        let cases: &[(&str, &[&str], bool, &str)] = &[
            ("help", &["thread", "--help"], false, "Show every reply"),
            (
                "unknown suggestion",
                &["thread", "--jsno", "$abc"],
                false,
                "Did you mean '--json'",
            ),
            (
                "global option before verb",
                &["--room", "!r", "thread", "$abc"],
                true,
                "",
            ),
            (
                "global option after verb",
                &["thread", "$abc", "--room", "!r"],
                true,
                "",
            ),
            (
                "message list item",
                &["send-to-leader", "-", "item"],
                true,
                "",
            ),
            (
                "unknown message flag",
                &["send-to-leader", "--urgent", "hi"],
                false,
                "put the text after --",
            ),
            (
                "escaped message flag",
                &["send-to-leader", "--", "--urgent", "hi"],
                true,
                "",
            ),
            (
                "help in message body",
                &["send-to-leader", "hello", "--help", "there"],
                true,
                "",
            ),
            ("help before verb", &["--help", "send-to-leader"], false, ""),
            (
                "verb name in message body",
                &["send-to-leader", "thread"],
                true,
                "",
            ),
        ];

        for (name, argv, expected_ok, expected_text) in cases {
            let report = parse(&spec(), argv);
            assert_eq!(report.ok, *expected_ok, "case {name}: {report:?}");
            assert!(
                report.text.contains(expected_text),
                "case {name}: expected {:?} in {:?}",
                expected_text,
                report.text
            );
        }
    }

    #[test]
    fn no_verb_and_missing_option_value_are_errors() {
        for (name, argv) in [
            ("no spec verb", &["not-a-verb"][..]),
            ("missing room value", &["thread", "--room"][..]),
        ] {
            let report = parse(&spec(), argv);
            assert!(!report.ok, "case {name} unexpectedly succeeded: {report:?}");
            assert_eq!(report.kind.as_deref(), Some("error"), "case {name}");
            assert_eq!(report.code, 2, "case {name}");
        }
    }

    #[test]
    fn success_report_has_the_default_shape_and_parsed_values() {
        let report = parse(&spec(), &["thread", "$abc"]);
        assert!(report.ok, "{report:?}");
        assert_eq!(report.verb.as_deref(), Some("thread"));
        assert_eq!(report.values["EVENT_ID"], "$abc");
        assert_eq!(report.values["json"], false);
        assert_eq!(report.code, 0);
        assert!(report.kind.is_none());
        assert!(report.text.is_empty());
    }

    #[test]
    fn non_global_root_options_are_read_from_root_matches() {
        let mut spec = spec();
        spec.options[1].global = false;
        for (argv, expected_json) in [
            (&["thread", "$abc"][..], false),
            (&["--json", "thread", "$abc"][..], true),
        ] {
            let report = parse(&spec, argv);
            assert!(report.ok, "{argv:?}: {report:?}");
            assert_eq!(report.values["json"], expected_json, "{argv:?}");
        }
    }

    #[test]
    fn help_in_message_body_respects_verb_position_and_separator() {
        let spec = spec();
        for argv in [
            &["--room", "x", "send-to-leader", "--help"][..],
            &["--room=x", "send-to-leader", "--help"][..],
            &["send-to-leader", "a", "--help"][..],
        ] {
            let report = parse(&spec, argv);
            assert_eq!(report.kind.as_deref(), Some("help"), "{argv:?}: {report:?}");
        }

        for argv in [
            &["send-to-leader", "--", "--help"][..],
            &["send-to-leader", "a", "--", "--help"][..],
        ] {
            let report = parse(&spec, argv);
            assert!(report.ok, "{argv:?}: {report:?}");
            let expected = if argv.len() == 3 {
                Value::String("--help".into())
            } else {
                Value::Array(vec![
                    Value::String("a".into()),
                    Value::String("--help".into()),
                ])
            };
            assert_eq!(report.values["TEXT"], expected);
        }

        let report = parse(&spec, &["send-to-leader", "hello", "--help", "there"]);
        assert!(report.ok, "{report:?}");
        assert_eq!(
            report.values["TEXT"],
            Value::Array(vec![
                Value::String("hello".into()),
                Value::String("--help".into()),
                Value::String("there".into()),
            ])
        );
    }

    #[test]
    fn help_in_reply_body_keeps_options_after_leading_positional() {
        let spec = reply_spec();
        let cases: &[(&[&str], &str, &str)] = &[
            (
                &["reply", "e", "--room", "R", "hello", "--help", "there"],
                "room",
                "R",
            ),
            (
                &["reply", "e", "-v", "q", "hello", "--help", "there"],
                "verbose",
                "q",
            ),
            (
                &["reply", "e", "--json", "hello", "--help", "there"],
                "json",
                "true",
            ),
        ];
        for (argv, option, expected) in cases {
            let report = parse(&spec, argv);
            assert!(report.ok, "{argv:?}: {report:?}");
            assert_eq!(report.values["EVENT_ID"], "e", "{argv:?}");
            assert_eq!(
                report.values[*option].to_string().trim_matches('"'),
                *expected,
                "{argv:?}"
            );
            assert_eq!(
                report.values["TEXT"],
                Value::Array(vec![
                    Value::String("hello".into()),
                    Value::String("--help".into()),
                    Value::String("there".into()),
                ]),
                "{argv:?}"
            );
        }
    }
}
