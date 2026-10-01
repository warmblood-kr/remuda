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
        .env("XDG_CACHE_HOME", dir.join("cache"))
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
            .env("XDG_CACHE_HOME", dir.join("cache"))
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
fn unsafe_unknown_name_never_looks_up_or_reports_half_installed_mod_paths() {
    let dir = std::env::temp_dir().join(format!("remuda-unsafe-mod-path-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let data = dir.join("data");
    let mods = data.join("remuda/mods");

    // ../x resolves to a deliberately half-installed sibling of mods. The
    // other malformed name is planted under mods to catch raw-word/path leaks.
    let outside_mods = data.join("remuda/x");
    fs::create_dir_all(&outside_mods).unwrap();
    let long_escape_word = format!("{}\x1b[31m", "a".repeat(40));
    // Windows forbids ESC in file names, and the malformed name cannot leak
    // there because it is rejected before the filesystem lookup.
    #[cfg(unix)]
    fs::create_dir_all(mods.join(&long_escape_word)).unwrap();

    for word in ["../x", &long_escape_word] {
        let output = Command::new(env!("CARGO_BIN_EXE_remuda"))
            .arg(word)
            .env("REMUDA_RUNTIME_DIR", &dir)
            .env("XDG_DATA_HOME", &data)
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("HOME", &dir)
            .output()
            .expect("run remuda");

        assert!(!output.status.success(), "unknown command succeeded");
        assert!(
            output.stdout.is_empty(),
            "unexpected stdout: {:?}",
            output.stdout
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            stderr,
            "remuda: no command or mod with that name.\n\
             Next: if this is a mod, install it with remuda mod install OWNER/REPO; installed mods: remuda mod list.\n"
        );
        assert!(!stderr.contains(word), "echoed unsafe word: {stderr:?}");
        assert!(
            !stderr.contains(&dir.display().to_string()),
            "leaked filesystem path: {stderr:?}"
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
    let mod_dir = dir.join("data/remuda/mods/butler");
    fs::create_dir_all(&mod_dir).unwrap();
    fs::write(
        mod_dir.join("extension.toml"),
        "name = \"butler\"\nentry = \"packages/butler/init.lua\"\napi = \"remuda-lua-v1\"\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .arg("butlr")
        .env("REMUDA_RUNTIME_DIR", &dir)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
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
        "remuda: no command or mod named butlr.\n\
         Did you mean: remuda butler?\n\
         Next: if butlr is a mod, install it with remuda mod install OWNER/REPO; installed mods: remuda mod list.\n"
    );
}
