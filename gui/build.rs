use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=remuda-gui.exe.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }

    // GPUI imports TaskDialogIndirect, which requires Common Controls v6.
    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
            .join("remuda-gui.exe.manifest");
    println!("cargo:rustc-link-arg-bin=remuda-gui=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bin=remuda-gui=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
