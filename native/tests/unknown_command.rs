use std::fs;
use std::process::Command;

#[test]
fn unknown_top_level_name_reports_mod_install_next_step() {
    let dir = std::env::temp_dir().join(format!("remuda-unknown-command-{}", std::process::id()));
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

    assert!(
        !output.status.success(),
        "unknown command succeeded: {output:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "remuda: no command or mod named nosuchmod.\n\
         Next: if nosuchmod is a mod, install it with remuda mod install OWNER/REPO; installed mods: remuda mod list.\n"
    );
}

#[test]
fn unsafe_unknown_name_is_not_echoed() {
    let dir = std::env::temp_dir().join(format!(
        "remuda-unsafe-unknown-command-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    for word in ["NOPE", "a\x1b[31m"] {
        let output = Command::new(env!("CARGO_BIN_EXE_remuda"))
            .arg(word)
            .env("REMUDA_RUNTIME_DIR", &dir)
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("HOME", &dir)
            .output()
            .expect("run remuda");

        assert!(
            !output.status.success(),
            "unknown command succeeded: {output:?}"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "remuda: no command or mod with that name.\n\
             Next: if this is a mod, install it with remuda mod install OWNER/REPO; installed mods: remuda mod list.\n"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn unknown_name_can_suggest_an_installed_mod() {
    let dir = std::env::temp_dir().join(format!(
        "remuda-unknown-installed-mod-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    let mod_dir = dir.join("data/remuda/mods/sample");
    fs::create_dir_all(&mod_dir).unwrap();
    fs::write(
        mod_dir.join("extension.toml"),
        "name = \"sample\"\nentry = \"packages/sample/init.lua\"\napi = \"remuda-lua-v1\"\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .arg("sampel")
        .env("REMUDA_RUNTIME_DIR", &dir)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("HOME", &dir)
        .output()
        .expect("run remuda");

    let _ = fs::remove_dir_all(&dir);

    assert!(
        !output.status.success(),
        "unknown command succeeded: {output:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "remuda: no command or mod named sampel.\n\
         Did you mean: remuda exec sample?\n\
         Next: if sampel is a mod, install it with remuda mod install OWNER/REPO; installed mods: remuda mod list.\n"
    );
}
