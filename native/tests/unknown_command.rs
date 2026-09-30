use std::fs;
use std::process::Command;

#[test]
fn unknown_top_level_name_reports_mod_install_next_step() {
    let dir = std::path::PathBuf::from(format!(
        "/private/tmp/remuda-unknown-command-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .arg("nosuchmod")
        .env("REMUDA_RUNTIME_DIR", &dir)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("HOME", &dir)
        .output()
        .expect("run remuda");

    let _ = fs::remove_dir_all(&dir);

    assert!(!output.status.success(), "unknown command succeeded: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "remuda: no command or mod named nosuchmod.\n\
         Next: if nosuchmod is a mod, install it with remuda mod install OWNER/REPO; installed mods: remuda mod list.\n"
    );
}
