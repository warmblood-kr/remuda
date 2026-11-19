//! CLI wrapper for the read-only OpenCode permission scan.

use std::path::PathBuf;
use std::process::ExitCode;

use remuda_native::native_permissions;

fn help() -> ExitCode {
    println!(
        "remuda _native_permissions [--json] [--root DIR]\n\
         Read-only list of pending OpenCode permission requests from Butler bindings.\n\
         It never replies or writes; credentials and nonces are not printed."
    );
    ExitCode::SUCCESS
}

pub fn run(args: &[&str]) -> ExitCode {
    let mut json_out = false;
    let mut root = None;
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "--help" | "-h" => return help(),
            "--json" => json_out = true,
            "--root" => {
                index += 1;
                match args.get(index) {
                    Some(path) if !path.is_empty() => root = Some(PathBuf::from(path)),
                    _ => {
                        eprintln!("remuda: _native_permissions: --root needs a directory");
                        return ExitCode::FAILURE;
                    }
                }
            }
            other => {
                eprintln!("remuda: _native_permissions: unknown argument {other}");
                return ExitCode::FAILURE;
            }
        }
        index += 1;
    }
    let root = match root.or_else(native_permissions::default_root) {
        Some(root) => root,
        None => {
            eprintln!("remuda: _native_permissions: no bindings directory");
            return ExitCode::FAILURE;
        }
    };
    let scan = native_permissions::scan(&root);
    if json_out {
        println!("{}", scan.to_json(&root));
    } else {
        print!("{}", scan.to_text());
    }
    ExitCode::SUCCESS
}
