use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=REMUDA_VERSION");
    println!("cargo:rerun-if-env-changed=REMUDA_BUILD");
    watch_git_head();

    let version = std::env::var("REMUDA_VERSION").unwrap_or_else(|_| {
        std::env::var("CARGO_PKG_VERSION").expect("Cargo sets package version")
    });
    let build = std::env::var("REMUDA_BUILD").unwrap_or_else(|_| git_build());
    println!("cargo:rustc-env=REMUDA_BUILD={build}");
    println!("cargo:rustc-env=REMUDA_BUILD_VERSION={version}+{build}");
    embed_utf8_manifest();
}

/// Windows: `remuda.exe` declares UTF-8 as its ANSI code page, so Lua's C
/// runtime (`os`, `io`) and the Rust-backed words agree on non-ASCII paths.
/// MSVC linker only (no Windows GNU build is shipped); needs Windows 10 1903.
fn embed_utf8_manifest() {
    println!("cargo:rerun-if-changed=remuda.exe.manifest");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" || env != "msvc" {
        return;
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("Cargo sets manifest"))
        .join("remuda.exe.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
}

fn watch_git_head() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("Cargo sets manifest"))
        .parent()
        .expect("native has a workspace parent")
        .to_path_buf();
    let Some(git_dir) = git_path(&root, ["rev-parse", "--git-dir"]) else {
        return;
    };
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        root.join(git_dir)
    };
    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    if let Some(reference) = git_path(&root, ["symbolic-ref", "-q", "HEAD"]) {
        println!(
            "cargo:rerun-if-changed={}",
            git_dir.join(reference).display()
        );
    }
}

fn git_path<const N: usize>(root: &std::path::Path, args: [&str; N]) -> Option<PathBuf> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    (!path.as_os_str().is_empty()).then_some(path)
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
