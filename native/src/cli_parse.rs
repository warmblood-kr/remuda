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

/// How repeats of one option within a scope are handled (strict specs only).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum RepeatPolicy {
    #[default]
    Reject,
    Append,
    Last,
    Coalesce,
}

#[derive(Clone, Debug)]
pub struct OptionSpec {
    pub long: String,
    pub short: Option<char>,
    pub value: Option<String>,
    pub help: String,
    pub global: bool,
    pub repeat_policy: RepeatPolicy,
}

#[derive(Clone, Debug)]
pub struct ArgSpec {
    pub name: String,
    pub help: String,
    pub multiple: bool,
    pub required: bool,
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
    pub origins: Map<String, Value>,
    pub boundaries: Vec<Value>,
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
            origins: Map::new(),
            boundaries: Vec::new(),
        }
    }

    pub fn failure(kind: &str, text: String, code: i32) -> Self {
        Self {
            ok: false,
            verb: None,
            values: Map::new(),
            kind: Some(kind.to_owned()),
            text,
            code,
            origins: Map::new(),
            boundaries: Vec::new(),
        }
    }
}

/// Versions and features this parser supports, for `remuda.cli.capabilities()`.
pub const SPEC_VERSIONS: &[u32] = &[1, 2];
pub const REPORT_VERSIONS: &[u32] = &[1, 2];
pub const FEATURES: &[&str] = &[
    "stable_report",
    "strict_v2",
    "repeat_policy",
    "token_origins",
];

/// Shape a report as the v2 envelope; vectors are always arrays.
/// Error text is the legacy diagnostic and may quote input until G7b safe_errors;
/// no sensitive-command consumer may migrate to report v2 before then.
pub fn report_v2(spec: &Spec, report: &Report) -> Value {
    let verb = report
        .verb
        .as_deref()
        .and_then(|name| spec.verbs.iter().find(|verb| verb.name == name));
    let mut values = report.values.clone();
    for arg in verb
        .iter()
        .flat_map(|verb| &verb.args)
        .filter(|a| a.multiple)
    {
        let items = match values.remove(&arg.name) {
            Some(Value::Array(items)) => items,
            Some(one) => vec![one],
            None if report.ok => Vec::new(),
            None => continue,
        };
        values.insert(arg.name.clone(), Value::Array(items));
    }
    let kind = report.kind.as_deref().unwrap_or("success");
    let mut out = serde_json::json!({
        "ok": report.ok, "kind": kind, "code": report.code, "text": report.text,
        "values": values, "path": [], "handler": "", "shape": "",
        "origins": report.origins, "boundaries": report.boundaries, "bodies": {},
    });
    if let Some(name) = &report.verb {
        out["verb"] = Value::String(name.clone());
    }
    out
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
            let mut report =
                Report::success(verb_name, collect_values(spec, verb, &matches, submatches));
            let (origins, boundaries) = token_origins(spec, verb, verb_index, argv);
            report.origins = origins;
            report.boundaries = boundaries;
            report
        }
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            let mut text = error.to_string();
            append_next(&mut text, &verb.next);
            Report::failure("help", text, 0)
        }
        Err(error) => error_report(spec, Some(verb), Some(&error)),
    }
}

/// Keep provenance tied to token positions and byte spans. This scanner only
/// records syntax already accepted by clap; it never decides whether argv is valid.
fn token_origins(
    spec: &Spec,
    verb: &VerbSpec,
    verb_index: usize,
    argv: &[&str],
) -> (Map<String, Value>, Vec<Value>) {
    let options: Vec<&OptionSpec> = spec.options.iter().chain(&verb.options).collect();
    let mut origins: Map<String, Value> = Map::new();
    let mut boundaries = Vec::new();
    let mut positionals = 0usize;
    let mut after_separator = false;
    let mut i = 0usize;
    while i < argv.len() {
        let word = argv[i];
        let index = i + 1;
        if i == verb_index {
            i += 1;
            continue;
        }
        if word == "--" {
            after_separator = true;
            boundaries.push(serde_json::json!({"argv_index": index, "role": "separator"}));
            i += 1;
            continue;
        }
        if word == "-" {
            boundaries.push(serde_json::json!({"argv_index": index, "role": if after_separator { "escaped_dash" } else { "dash_value" }}));
        }
        let (recognized, consumed_next) = scan_option_origin(
            &options,
            word,
            argv.get(i + 1).copied(),
            index,
            after_separator,
            &mut origins,
        );
        if !recognized && (after_separator || !word.starts_with('-') || word == "-") {
            if let Some(arg) = verb
                .args
                .get(positionals.min(verb.args.len().saturating_sub(1)))
            {
                push_origin(
                    &mut origins,
                    &arg.name,
                    index,
                    0..word.len(),
                    after_separator,
                );
                if !arg.multiple {
                    positionals += 1;
                }
            }
        }
        i += 1 + usize::from(consumed_next);
    }
    for option in options
        .iter()
        .filter(|option| option.repeat_policy == RepeatPolicy::Last)
    {
        if let Some(Value::Array(items)) = origins.get_mut(&option.long) {
            if let Some(last) = items.pop() {
                items.clear();
                items.push(last);
            }
        }
    }
    (origins, boundaries)
}

fn scan_option_origin(
    options: &[&OptionSpec],
    word: &str,
    next: Option<&str>,
    index: usize,
    after_separator: bool,
    origins: &mut Map<String, Value>,
) -> (bool, bool) {
    if after_separator {
        return (false, false);
    }
    if let Some(body) = word.strip_prefix("--").filter(|_| word.len() > 2) {
        let (name, inline) = body
            .split_once('=')
            .map_or((body, None), |(name, value)| (name, Some(value)));
        let Some(option) = options.iter().find(|option| option.long == name) else {
            return (false, false);
        };
        if option.value.is_some() {
            let (value_index, range, consumed_next) = match inline {
                Some(value) => (index, word.len() - value.len()..word.len(), false),
                None if next.is_some() => {
                    let value = next.unwrap_or_default();
                    (index + 1, 0..value.len(), true)
                }
                None => (index, 0..0, false),
            };
            push_origin(origins, &option.long, value_index, range, after_separator);
            return (true, consumed_next);
        }
        push_origin(origins, &option.long, index, 0..word.len(), after_separator);
        return (true, false);
    }
    if !word.starts_with('-') || word.len() == 1 {
        return (false, false);
    }
    let chars: Vec<(usize, char)> = word.char_indices().skip(1).collect();
    let mut found = false;
    for (n, (start, short)) in chars.iter().enumerate() {
        let Some(option) = options.iter().find(|option| option.short == Some(*short)) else {
            continue;
        };
        found = true;
        let next_start = chars.get(n + 1).map(|(at, _)| *at).unwrap_or(word.len());
        if option.value.is_some() {
            if next_start < word.len() {
                push_origin(
                    origins,
                    &option.long,
                    index,
                    next_start..word.len(),
                    after_separator,
                );
                return (true, false);
            }
            if let Some(value) = next {
                push_origin(
                    origins,
                    &option.long,
                    index + 1,
                    0..value.len(),
                    after_separator,
                );
                return (true, true);
            }
            return (true, false);
        }
        push_origin(
            origins,
            &option.long,
            index,
            *start..start + short.len_utf8(),
            after_separator,
        );
    }
    (found, false)
}

fn push_origin(
    origins: &mut Map<String, Value>,
    id: &str,
    argv_index: usize,
    span: std::ops::Range<usize>,
    after_separator: bool,
) {
    let list = origins
        .entry(id.to_owned())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Value::Array(items) = list {
        items.push(serde_json::json!({
            "argv_index": argv_index,
            "byte_range": [span.start, span.end],
            "after_separator": after_separator,
            "synthetic": false,
        }));
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
            .required(arg.required);
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
        let action = match option.repeat_policy {
            RepeatPolicy::Append => ArgAction::Append,
            _ => ArgAction::Set,
        };
        arg = arg.value_name(value.clone()).action(action);
    } else {
        arg = arg.action(ArgAction::SetTrue);
    }
    if matches!(
        option.repeat_policy,
        RepeatPolicy::Last | RepeatPolicy::Coalesce
    ) {
        arg = arg.overrides_with(option.long.clone());
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
    parts.extend(verb.args.iter().map(|arg| {
        if arg.required {
            arg.name.clone()
        } else {
            format!("[{}]", arg.name)
        }
    }));
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
        if word.strip_prefix('-').is_some_and(|rest| !rest.is_empty()) {
            let cluster = word
                .strip_prefix('-')
                .unwrap_or_default()
                .chars()
                .collect::<Vec<_>>();
            let value_option_position = cluster.iter().position(|short| {
                options
                    .clone()
                    .any(|option| option.short == Some(*short) && option.value.is_some())
            });
            let consumes_next =
                value_option_position.is_some_and(|position| position + 1 == cluster.len());
            index += 1 + usize::from(consumes_next);
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
    let value = if option.repeat_policy == RepeatPolicy::Append {
        let items = matches.and_then(|m| m.get_many::<String>(&option.long));
        Some(Value::Array(
            items
                .into_iter()
                .flatten()
                .cloned()
                .map(Value::String)
                .collect(),
        ))
    } else if option.value.is_some() {
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
    use super::{parse, Arg, ArgAction, ArgSpec, Command, OptionSpec, Spec, VerbSpec};
    use serde_json::Value;

    #[test]
    fn token_adapter_grammar_matches_locked_clap_cases() {
        let command = Command::new("go")
            .disable_help_flag(true)
            .arg(Arg::new("x").long("x").short('x').num_args(1))
            .arg(Arg::new("a").short('a').action(ArgAction::SetTrue))
            .arg(Arg::new("b").short('b').action(ArgAction::SetTrue))
            .arg(Arg::new("c").short('c').action(ArgAction::SetTrue))
            .arg(Arg::new("flag").long("flag").action(ArgAction::SetTrue))
            .arg(Arg::new("ITEM").num_args(0..).trailing_var_arg(true));
        let cases: &[(&[&str], bool, &str)] = &[
            (&["-"], true, "dash value"),
            (&["--", "-"], true, "escaped dash"),
            (&["--x=-"], true, "inline dash value"),
            (&["-abc"], true, "short cluster"),
            (&["--flag", "--flag"], false, "repeated flag"),
            (&["same", "same"], true, "repeated identical words"),
        ];
        for (argv, expected, label) in cases {
            let actual = command
                .clone()
                .try_get_matches_from(std::iter::once("go").chain(argv.iter().copied()))
                .is_ok();
            assert_eq!(actual, *expected, "locked clap case: {label}");
        }
    }

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
                    repeat_policy: Default::default(),
                },
                OptionSpec {
                    long: "json".into(),
                    short: None,
                    value: None,
                    help: "Print JSON".into(),
                    global: true,
                    repeat_policy: Default::default(),
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
                        required: true,
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
                        required: true,
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
                    repeat_policy: Default::default(),
                },
                OptionSpec {
                    long: "verbose".into(),
                    short: Some('v'),
                    value: Some("LEVEL".into()),
                    help: "Verbosity".into(),
                    global: true,
                    repeat_policy: Default::default(),
                },
                OptionSpec {
                    long: "json".into(),
                    short: Some('j'),
                    value: None,
                    help: "Print JSON".into(),
                    global: true,
                    repeat_policy: Default::default(),
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
                        required: true,
                    },
                    ArgSpec {
                        name: "TEXT".into(),
                        help: "Message text".into(),
                        multiple: true,
                        required: true,
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

    #[test]
    fn short_cluster_value_does_not_start_the_message_body() {
        let report = parse(
            &reply_spec(),
            &["reply", "-jr", "R", "e", "hello", "--help", "x"],
        );
        assert!(report.ok, "{report:?}");
        assert_eq!(report.values["room"], "R");
        assert_eq!(report.values["json"], true);
        assert_eq!(report.values["EVENT_ID"], "e");
        assert_eq!(
            report.values["TEXT"],
            Value::Array(vec![
                Value::String("hello".into()),
                Value::String("--help".into()),
                Value::String("x".into()),
            ])
        );
    }
}
