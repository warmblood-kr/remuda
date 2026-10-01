const INSTALL_SH: &str = include_str!("../../docs/install.sh");
const INSTALL_PS1: &str = include_str!("../../docs/install.ps1");

#[test]
fn both_installers_offer_the_gated_butler_setup_and_next_steps() {
    for (name, source, gate) in [
        (
            "install.sh",
            INSTALL_SH,
            r#"if [ "${REMUDA_INSTALL_BUTLER:-}" = 1 ]; then"#,
        ),
        (
            "install.ps1",
            INSTALL_PS1,
            "if ($env:REMUDA_INSTALL_BUTLER -eq '1') {",
        ),
    ] {
        assert!(
            source.contains(gate),
            "{name} is missing its Butler opt-in gate"
        );
        assert!(
            source.contains("mod install warmblood-kr/remuda-butler --force"),
            "{name} is missing its Butler mod install command"
        );
        assert!(
            source.contains("Next: remuda butler doctor"),
            "{name} is missing the successful-install next step"
        );
        assert!(
            source.contains("Next: remuda mod install warmblood-kr/remuda-butler --force"),
            "{name} is missing the failed-install next step"
        );
    }
}

// The only place the installer itself runs: on the Windows runner, under both
// shells a user can have. Each script explains what it asserts.
#[cfg(windows)]
fn passes_under_both_shells(check: &str) {
    let check = format!("{}/../scripts/{check}", env!("CARGO_MANIFEST_DIR"));
    for shell in ["powershell", "pwsh"] {
        let output = std::process::Command::new(shell)
            // CI's step shell is pwsh. Windows PowerShell started under its
            // PSModulePath did not find Get-FileHash; a user opens it directly,
            // so give it the same clean start.
            .env_remove("PSModulePath")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", &check])
            .output()
            .unwrap_or_else(|error| panic!("cannot run {shell}: {error}"));
        assert!(
            output.status.success(),
            "{shell}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(windows)]
#[test]
fn the_windows_installer_puts_its_install_dir_on_path() {
    passes_under_both_shells("check-install-path.ps1");
}

#[cfg(windows)]
#[test]
fn a_failed_windows_install_leaves_the_session_open() {
    passes_under_both_shells("check-install-die.ps1");
}

#[cfg(windows)]
#[test]
fn a_windows_install_without_git_explains_the_butler_step() {
    passes_under_both_shells("check-install-butler.ps1");
}

// `remuda upgrade` typed into PowerShell 7 runs the installer in Windows
// PowerShell with pwsh's PSModulePath, where there is no Get-FileHash.
#[cfg(windows)]
#[test]
fn a_windows_install_checks_its_download_in_powershell_started_from_pwsh() {
    passes_under_both_shells("check-install-hash.ps1");

    let pwsh = std::process::Command::new("pwsh")
        .args([
            "-NoProfile",
            "-Command",
            "[Console]::OutputEncoding = [Text.Encoding]::UTF8; $env:PSModulePath",
        ])
        .output()
        .expect("cannot run pwsh");
    let module_path = String::from_utf8_lossy(&pwsh.stdout);
    assert!(
        pwsh.status.success() && !module_path.trim().is_empty(),
        "pwsh did not say what its PSModulePath is"
    );
    let check = format!(
        "{}/../scripts/check-install-hash.ps1",
        env!("CARGO_MANIFEST_DIR")
    );
    let output = std::process::Command::new("powershell")
        .env("PSModulePath", module_path.trim())
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", &check])
        .output()
        .expect("cannot run powershell");
    assert!(
        output.status.success(),
        "powershell under pwsh's PSModulePath: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
