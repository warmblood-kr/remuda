#[cfg(target_os = "macos")]
use std::io::Write;

#[cfg(target_os = "macos")]
#[allow(clippy::zombie_processes)] // The guardian must clean this intentionally orphaned fixture.
fn main() {
    if let Some(path) = std::env::var_os("REMUDA_PROBE_MARKER") {
        std::fs::write(path, "started").unwrap();
    }
    match std::env::var("REMUDA_PROBE_MODE").unwrap().as_str() {
        "good" => println!("build.1"),
        "extra" => print!("build.1\nextra\n"),
        "no-newline" => print!("build.1"),
        "stdout" => print!("{}", "x".repeat(10000)),
        "stderr" => eprint!("{}", "x".repeat(10000)),
        "nonzero" => std::process::exit(7),
        "hang" => std::thread::sleep(std::time::Duration::from_secs(60)),
        "good-descendant" | "bad-descendant" => {
            use std::io::Read;
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .env("REMUDA_PROBE_MODE", "socket")
                .env("REMUDA_PROBE_CHILD_READY", "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            child.stdout.take().unwrap().read_exact(&mut [0]).unwrap();
            if std::env::var("REMUDA_PROBE_MODE").unwrap() == "good-descendant" {
                println!("build.1");
            } else {
                println!("wrong");
            }
            std::mem::forget(child);
        }
        "socket" | "socket-parent" => {
            let mut stream = std::os::unix::net::UnixStream::connect(
                std::env::var_os("REMUDA_PROBE_SOCKET").unwrap(),
            )
            .unwrap();
            stream.write_all(b"x").unwrap();
            if std::env::var_os("REMUDA_PROBE_CHILD_READY").is_some() {
                std::io::stdout().write_all(b"r").unwrap();
                std::io::stdout().flush().unwrap();
            }
            let mut descendant = if std::env::var("REMUDA_PROBE_MODE").unwrap() == "socket-parent" {
                Some(
                    std::process::Command::new(std::env::current_exe().unwrap())
                        .env("REMUDA_PROBE_MODE", "socket")
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .unwrap(),
                )
            } else {
                None
            };
            std::thread::sleep(std::time::Duration::from_secs(60));
            if let Some(child) = &mut descendant {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        _ => std::process::exit(2),
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
