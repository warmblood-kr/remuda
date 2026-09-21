use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=REMUDA_VERSION");
    println!("cargo:rerun-if-env-changed=REMUDA_BUILD");
    println!("cargo:rerun-if-changed=../.git/HEAD");

    let version = std::env::var("REMUDA_VERSION").unwrap_or_else(|_| {
        std::env::var("CARGO_PKG_VERSION").expect("Cargo sets package version")
    });
    let build = std::env::var("REMUDA_BUILD").unwrap_or_else(|_| git_build());
    println!("cargo:rustc-env=REMUDA_BUILD={build}");
    println!("cargo:rustc-env=REMUDA_BUILD_VERSION={version}+{build}");
}

fn git_build() -> String {
    let output = Command::new("git")
        .args(["describe", "--always", "--dirty", "--abbrev=12"])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            format!("git.{}", String::from_utf8_lossy(&output.stdout).trim())
        }
        _ => "local".into(),
    }
}
