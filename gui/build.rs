fn main() {
    println!("cargo:rerun-if-changed=remuda-icon.rc");
    println!("cargo:rerun-if-changed=assets/remuda.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile_for("remuda-icon.rc", ["remuda-gui"], embed_resource::NONE)
            .manifest_required()
            .expect("embed the Windows application icon");
    }
}
