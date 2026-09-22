use super::{describe, exec_command, fail, with_daemon};
use remuda_core::protocol::{Request, Response};
use std::path::Path;
use std::process::ExitCode;

type Handler = fn(&str, &Path, &[&str]) -> ExitCode;
const HANDLERS: &[(&str, Handler)] = &[("butler", butler_command)];

pub fn extension_command(server: &str, path: &Path, command: &str, args: &[&str]) -> ExitCode {
    if args.is_empty() {
        let package = remuda_native::packages::subcommand(command)
            .expect("subcommand dispatch was checked above");
        return with_daemon(server, path, |path| exec_command(path, package.name));
    }
    match HANDLERS
        .iter()
        .find(|(name, _)| *name == command)
        .map(|(_, handler)| handler)
    {
        Some(handler) => handler(server, path, args),
        None => fail(format!("extension {command} does not accept subcommands")),
    }
}

const USAGE: &str = "\
remuda butler — lightweight coordination for managed agents

  remuda butler sessions
  remuda butler launch <claude|codex> [name]
  remuda butler topic new <name> [--template T] [--agent A]
  remuda butler topic delegate <name> <task...> [--agent A] [--leader L]
  remuda butler send <to> <message...>
  remuda butler send <from> <to> <message...>
  remuda butler send-to-leader <message...>
  remuda butler inbox [name]

Butler is live state in the remuda daemon. Start it with:

  remuda butler
";

fn butler_command(server: &str, path: &Path, args: &[&str]) -> ExitCode {
    match args {
        [] | ["help"] | ["-h"] | ["--help"] => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        ["sessions"] => with_daemon(server, path, |path| {
            eval(path, "return remuda._butler_sessions()")
        }),
        ["launch", kind] => with_daemon(server, path, |path| {
            eval(
                path,
                &format!("return remuda._butler_launch({}, nil)", lua_string(kind)),
            )
        }),
        ["launch", kind, name] => with_daemon(server, path, |path| {
            eval(
                path,
                &format!(
                    "return remuda._butler_launch({}, {})",
                    lua_string(kind),
                    lua_string(name)
                ),
            )
        }),
        ["topic", "new", name] => topic_new(server, path, name, None, None),
        ["topic", "new", name, "--template", template] => {
            topic_new(server, path, name, Some(template), None)
        }
        ["topic", "new", name, "--agent", agent] => {
            topic_new(server, path, name, None, Some(agent))
        }
        ["topic", "new", name, "--template", template, "--agent", agent]
        | ["topic", "new", name, "--agent", agent, "--template", template] => {
            topic_new(server, path, name, Some(template), Some(agent))
        }
        ["topic", "delegate", name, "--leader", leader, "--agent", agent, task @ ..]
        | ["topic", "delegate", name, "--agent", agent, "--leader", leader, task @ ..]
            if !task.is_empty() =>
        {
            topic_delegate(
                server,
                path,
                name,
                &task.join(" "),
                Some(agent),
                Some(leader),
            )
        }
        ["topic", "delegate", name, "--leader", leader, task @ ..] if !task.is_empty() => {
            topic_delegate(server, path, name, &task.join(" "), None, Some(leader))
        }
        ["topic", "delegate", name, "--agent", agent, task @ ..] if !task.is_empty() => {
            topic_delegate(server, path, name, &task.join(" "), Some(agent), None)
        }
        ["topic", "delegate", name, task @ ..] if !task.is_empty() => {
            topic_delegate(server, path, name, &task.join(" "), None, None)
        }
        ["send", from, to, message @ ..] if !message.is_empty() => {
            with_daemon(server, path, |path| {
                eval(
                    path,
                    &format!(
                        "return remuda._butler_send({}, {}, {})",
                        lua_string(from),
                        lua_string(to),
                        lua_string(&message.join(" "))
                    ),
                )
            })
        }
        ["send", to, message @ ..] if !message.is_empty() => {
            let Ok(from) = agent_id() else {
                return fail(
                    "send needs REMUDA_BUTLER_AGENT_ID; run it inside a Butler agent session",
                );
            };
            with_daemon(server, path, |path| {
                eval(
                    path,
                    &format!(
                        "return remuda._butler_send({}, {}, {})",
                        lua_string(&from),
                        lua_string(to),
                        lua_string(&message.join(" "))
                    ),
                )
            })
        }
        ["send-to-leader", message @ ..] if !message.is_empty() => {
            send_to_leader(server, path, message)
        }
        ["inbox", name] => with_daemon(server, path, |path| {
            eval(
                path,
                &format!("return remuda._butler_inbox({})", lua_string(name)),
            )
        }),
        ["inbox"] => {
            let Ok(name) = agent_id() else {
                return fail("inbox needs REMUDA_BUTLER_AGENT_ID; pass a name outside a Butler agent session");
            };
            with_daemon(server, path, |path| {
                eval(
                    path,
                    &format!("return remuda._butler_inbox({})", lua_string(&name)),
                )
            })
        }
        _ => {
            eprint!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn send_to_leader(server: &str, path: &Path, message: &[&str]) -> ExitCode {
    let Ok(from) = agent_id() else {
        return fail(
            "send-to-leader needs REMUDA_BUTLER_AGENT_ID; run it inside a Butler agent session",
        );
    };
    with_daemon(server, path, |path| {
        eval(
            path,
            &format!(
                "return remuda._butler_report({}, {})",
                lua_string(&from),
                lua_string(&message.join(" "))
            ),
        )
    })
}

fn agent_id() -> Result<String, std::env::VarError> {
    std::env::var("REMUDA_BUTLER_AGENT_ID").or_else(|_| std::env::var("REMUDA_BUTLER_SESSION_NAME"))
}

fn topic_delegate(
    server: &str,
    path: &Path,
    name: &str,
    task: &str,
    agent: Option<&str>,
    leader: Option<&str>,
) -> ExitCode {
    with_daemon(server, path, |path| {
        eval(
            path,
            &format!(
                "return remuda._butler_topic_delegate({}, {}, nil, {}, {})",
                lua_string(name),
                lua_string(task),
                agent.map(lua_string).unwrap_or_else(|| "nil".into()),
                leader
                    .map(lua_string)
                    .or_else(|| std::env::var("REMUDA_BUTLER_LEADER_ID")
                        .ok()
                        .filter(|id| !id.is_empty())
                        .map(|id| lua_string(&id)))
                    .unwrap_or_else(|| "\"butler\"".into()),
            ),
        )
    })
}

fn topic_new(
    server: &str,
    path: &Path,
    name: &str,
    template: Option<&str>,
    agent: Option<&str>,
) -> ExitCode {
    with_daemon(server, path, |path| {
        eval(
            path,
            &format!(
                "return remuda._butler_topic_new({}, {}, {})",
                lua_string(name),
                template.map(lua_string).unwrap_or_else(|| "nil".into()),
                agent.map(lua_string).unwrap_or_else(|| "nil".into()),
            ),
        )
    })
}

pub(crate) fn lua_string(value: &str) -> String {
    serde_json::to_string(value).expect("strings always serialize")
}

fn eval(path: &Path, code: &str) -> ExitCode {
    match remuda_native::client::request(
        path,
        &Request::Eval {
            code: code.to_string(),
            name: Some("butler-cli".into()),
        },
    ) {
        Ok(Response::Value(value)) => {
            if !value.is_empty() {
                println!("{value}");
            }
            ExitCode::SUCCESS
        }
        Ok(Response::Error(error)) if error.contains("_butler_") && error.contains("nil value") => {
            eprintln!("remuda: Butler is not running; start it with `remuda butler`");
            ExitCode::FAILURE
        }
        other => fail(describe(other)),
    }
}
