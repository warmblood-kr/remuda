//! Version string, release channel, and the once-a-day update notice.
//!
//! Nothing here opens a socket. `upgrade` re-runs the published install script,
//! so install and upgrade are one tested code path and this binary carries no
//! HTTP client. The update check is a *detached* `curl` that writes a cache file
//! for a later run to read — so a notice can arrive one run late, and no command
//! ever waits on the network. That is the trade: staleness instead of latency.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// CI injects the full version (including a nightly suffix); a local `cargo
/// build` falls back to the manifest.
pub const VERSION: &str = match option_env!("REMUDA_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// `VERSION` determines update ordering; this identifies the exact build.
pub const BUILD: &str = env!("REMUDA_BUILD");
pub const BUILD_VERSION: &str = env!("REMUDA_BUILD_VERSION");

pub const INDEX_URL: &str = "https://warmblood-kr.github.io/remuda/latest.json";

/// The installer this binary re-runs to upgrade itself. Two scripts, one
/// behaviour — see `docs/install.ps1`, which mirrors `docs/install.sh`.
pub const INSTALL_URL: &str = if cfg!(windows) {
    "https://warmblood-kr.github.io/remuda/install.ps1"
} else {
    "https://warmblood-kr.github.io/remuda/install.sh"
};

const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The channel this install follows, as written by the install script.
pub fn channel() -> String {
    std::fs::read_to_string(channel_path())
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| is_channel(s))
        .unwrap_or_else(|| "stable".into())
}

pub fn is_channel(name: &str) -> bool {
    matches!(name, "stable" | "nightly")
}

/// Re-run the published install script with the channel pinned. It replaces the
/// binary by rename, so upgrading from a running `remuda` is safe.
pub fn upgrade(channel: Option<&str>) -> Result<(), String> {
    let channel = channel.map(str::to_string).unwrap_or_else(self::channel);
    if !is_channel(&channel) {
        return Err(format!("unknown channel {channel:?} — stable or nightly"));
    }
    eprintln!("remuda: upgrading on the {channel} channel…");
    let status = installer_command(&channel)
        .status()
        .map_err(|e| format!("cannot run the installer: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("the installer exited with {status}"))
    }
}

/// Fetch the installer to a file and run it from there. Deliberately NOT
/// `curl … | sh`: a pipeline reports the *last* status, so a 404 fed an empty
/// script to a shell that exited 0 — a failed upgrade that looked finished.
#[cfg(unix)]
fn installer_command(channel: &str) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(format!(
        "set -e; t=$(mktemp); trap 'rm -f \"$t\"' EXIT; \
         curl -fsSL --max-time 120 -o \"$t\" {INSTALL_URL}; sh \"$t\""
    ));
    command
        .env("REMUDA_CHANNEL", channel)
        .env_remove("REMUDA_INSTALL_BUTLER");
    command
}

/// The same two rules in PowerShell: land the script, then run it. `-Stop` is
/// what makes `Invoke-WebRequest` raise on a 404 instead of returning an error
/// page for the next line to execute.
#[cfg(windows)]
fn installer_command(channel: &str) -> Command {
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"]);
    command.arg(format!(
        "$ErrorActionPreference='Stop'; \
         $t = Join-Path ([IO.Path]::GetTempPath()) ('remuda-install-' + [guid]::NewGuid() + '.ps1'); \
         try {{ Invoke-WebRequest -UseBasicParsing -Uri '{INSTALL_URL}' -OutFile $t; \
         & powershell -NoProfile -ExecutionPolicy Bypass -File $t; exit $LASTEXITCODE }} \
         finally {{ Remove-Item -Force -ErrorAction SilentlyContinue $t }}"
    ));
    command
        .env("REMUDA_CHANNEL", channel)
        .env_remove("REMUDA_INSTALL_BUTLER");
    command
}

/// One line for stderr when a newer version is published, or `None`. Reads only
/// the cache; refreshing it happens in a detached child.
pub fn update_notice() -> Option<String> {
    if std::env::var_os("REMUDA_NO_UPDATE_CHECK").is_some() {
        return None;
    }
    let cache = cache_path();
    if is_stale(&cache) {
        refresh(&cache);
    }
    let text = std::fs::read_to_string(&cache).ok()?;
    let index: serde_json::Value = serde_json::from_str(&text).ok()?;
    let channel = channel();
    let latest = index.get(&channel)?.as_str()?;
    is_newer(latest, VERSION).then(|| {
        format!(
            "remuda: {latest} is out on the {channel} channel (you have {VERSION}) \
             — `remuda upgrade`, or REMUDA_NO_UPDATE_CHECK=1 to silence this"
        )
    })
}

/// Missing or older than the interval. An unreadable mtime counts as stale.
fn is_stale(cache: &Path) -> bool {
    std::fs::metadata(cache)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|d| d > CHECK_INTERVAL).unwrap_or(true))
        .unwrap_or(true)
}

/// Fetch the index in the background and never wait for it. The trailing stamp
/// is what stops an offline machine from spawning a fetch per command: a failed
/// fetch still touches the cache, so the next attempt is a day away.
fn refresh(cache: &Path) {
    let Some(dir) = cache.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = cache.with_extension("tmp");
    let mut command = fetch_index_command(&tmp, cache);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _ = command.spawn();
}

#[cfg(unix)]
fn fetch_index_command(tmp: &Path, cache: &Path) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(format!(
        "curl -fsSL --max-time 10 -o {tmp} {INDEX_URL} && mv -f {tmp} {cache}; touch {cache}",
        tmp = shell_quote(tmp),
        cache = shell_quote(cache),
    ));
    command
}

/// Paths travel as environment variables rather than inside the script text, so
/// there is no PowerShell quoting to get wrong. `CREATE_NO_WINDOW` keeps this
/// from flashing a console on every single command.
#[cfg(windows)]
fn fetch_index_command(tmp: &Path, cache: &Path) -> Command {
    use std::os::windows::process::CommandExt;
    let mut command = Command::new("powershell");
    command
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"])
        .arg(format!(
            "try {{ Invoke-WebRequest -UseBasicParsing -Uri '{INDEX_URL}' \
             -OutFile $env:REMUDA_TMP -ErrorAction Stop; \
             Move-Item -Force $env:REMUDA_TMP $env:REMUDA_CACHE }} catch {{}}; \
             if (Test-Path $env:REMUDA_CACHE) \
             {{ (Get-Item $env:REMUDA_CACHE).LastWriteTime = Get-Date }} \
             else {{ New-Item -ItemType File -Force $env:REMUDA_CACHE | Out-Null }}"
        ))
        .env("REMUDA_TMP", tmp)
        .env("REMUDA_CACHE", cache)
        .creation_flags(0x0800_0000);
    command
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// Semver-ish ordering: the numeric triple first, then a release outranks any
/// prerelease, then prerelease strings compare lexically — which is why the
/// nightly suffix is `YYYYMMDD.<sha>` and not `<sha>`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    rank(candidate) > rank(current)
}

fn rank(version: &str) -> ([u64; 3], bool, String) {
    let (triple, pre) = version.split_once('-').unwrap_or((version, ""));
    let mut numbers = [0u64; 3];
    for (slot, field) in numbers.iter_mut().zip(triple.split('.')) {
        *slot = field.parse().unwrap_or(0);
    }
    (numbers, pre.is_empty(), pre.to_string())
}

fn channel_path() -> PathBuf {
    data_home()
        .unwrap_or_else(|| PathBuf::from(".local/share"))
        .join("remuda")
        .join("channel")
}

fn cache_path() -> PathBuf {
    cache_path_for(cfg!(windows), &|name| std::env::var_os(name))
}

/// The data home, parent of `remuda/`: `$XDG_DATA_HOME`, else `%LOCALAPPDATA%` on Windows or `$HOME/.local/share`; `None` if unknown.
pub fn data_home() -> Option<PathBuf> {
    data_home_for(cfg!(windows), &|name| std::env::var_os(name))
}

type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

fn set(env: Env, name: &str) -> Option<PathBuf> {
    env(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

// Platform and environment are arguments so every arm runs on any host.
fn data_home_for(windows: bool, env: Env) -> Option<PathBuf> {
    set(env, "XDG_DATA_HOME").or_else(|| home_dir_for(windows, env, ".local/share"))
}

fn cache_path_for(windows: bool, env: Env) -> PathBuf {
    let dir = match set(env, "XDG_CACHE_HOME") {
        Some(dir) => dir.join("remuda"),
        None if windows => home_dir_for(true, env, "")
            .unwrap_or_default()
            .join("remuda")
            .join("cache"),
        None => home_dir_for(false, env, ".cache")
            .unwrap_or_else(|| PathBuf::from(".cache"))
            .join("remuda"),
    };
    dir.join("update-check.json")
}

/// Windows keeps app data in `%LOCALAPPDATA%`; elsewhere `$HOME` (or
/// `%USERPROFILE%`) plus the XDG layout `unix_sub`.
fn home_dir_for(windows: bool, env: Env, unix_sub: &str) -> Option<PathBuf> {
    if windows {
        return set(env, "LOCALAPPDATA")
            .or_else(|| set(env, "USERPROFILE").map(|home| home.join("AppData").join("Local")));
    }
    env("HOME")
        .or_else(|| env("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(unix_sub))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_outranks_its_own_nightlies_and_the_triple_wins_first() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.0"), "0.10 must not lose to 0.9");
        assert!(is_newer("0.1.0", "0.1.0-nightly.20260910.abc1234"));
        assert!(is_newer(
            "0.2.0-nightly.20260911.def5678",
            "0.2.0-nightly.20260910.abc1234"
        ));
        // Two nightlies of ONE day. This used to rank by sha — which has no
        // order — and the binary really did announce `…ce56928` as newer than
        // the `…396eb78` already installed. Hence a stamp to the second.
        assert!(is_newer(
            "0.1.0-nightly.20260910143022.396eb78",
            "0.1.0-nightly.20260910052043.ce56928"
        ));
        assert!(!is_newer(
            "0.1.0-nightly.20260910052043.ce56928",
            "0.1.0-nightly.20260910143022.396eb78"
        ));
        // The stamp got longer mid-flight: whoever installed under the old
        // day-only scheme must still be offered the new one, never the reverse.
        assert!(is_newer(
            "0.1.0-nightly.20260910143022.396eb78",
            "0.1.0-nightly.20260910.396eb78"
        ));
        // Not newer: equal, older, and a nightly of the version you already run.
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.1.0-nightly.20260910.abc1234", "0.1.0"));
    }

    // Unix-only because the FUNCTION is: Windows passes paths as environment
    // variables and never quotes them into a script. Not a test skipped to make
    // a suite green — there is nothing on the other platform to call.
    #[cfg(unix)]
    #[test]
    fn shell_quoting_survives_a_quote_in_the_path() {
        assert_eq!(shell_quote(Path::new("/tmp/a b")), "'/tmp/a b'");
        assert_eq!(shell_quote(Path::new("/tmp/it's")), r"'/tmp/it'\''s'");
    }

    #[test]
    fn the_version_is_never_empty() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn upgrade_installer_does_not_inherit_butler_install_opt_in() {
        // `Command` inherits parent variables unless it explicitly removes
        // them. Inspect the builder instead of running its network installer.
        let command = installer_command("nightly");
        assert!(
            command.get_envs().any(|(name, value)| {
                name == std::ffi::OsStr::new("REMUDA_INSTALL_BUTLER") && value.is_none()
            }),
            "upgrade installer must remove REMUDA_INSTALL_BUTLER from its child environment"
        );
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    const HANGUL: &str = r"C:\Users\박 정수\AppData\Local";

    #[test]
    fn windows_data_home_is_localappdata_and_the_cache_nests_under_it() {
        let env = env_of(&[("LOCALAPPDATA", HANGUL), ("HOME", "/ignored")]);
        assert_eq!(data_home_for(true, &env), Some(PathBuf::from(HANGUL)));
        assert_eq!(
            cache_path_for(true, &env),
            PathBuf::from(HANGUL)
                .join("remuda")
                .join("cache")
                .join("update-check.json")
        );
    }

    #[test]
    fn windows_xdg_wins_and_localappdata_falls_back_to_the_profile() {
        let env = env_of(&[
            ("XDG_DATA_HOME", "/x"),
            ("XDG_CACHE_HOME", "/c"),
            ("LOCALAPPDATA", "L"),
        ]);
        assert_eq!(data_home_for(true, &env), Some(PathBuf::from("/x")));
        assert_eq!(
            cache_path_for(true, &env),
            PathBuf::from("/c/remuda/update-check.json")
        );
        for env in [
            env_of(&[("USERPROFILE", "P")]),
            env_of(&[("LOCALAPPDATA", ""), ("USERPROFILE", "P")]),
        ] {
            assert_eq!(
                data_home_for(true, &env),
                Some(PathBuf::from("P").join("AppData").join("Local"))
            );
        }
        assert_eq!(data_home_for(true, &env_of(&[("HOME", "/h")])), None);
    }

    #[test]
    fn unix_rule_is_unchanged() {
        let env = env_of(&[("HOME", "/h"), ("USERPROFILE", "/p"), ("LOCALAPPDATA", "L")]);
        assert_eq!(
            data_home_for(false, &env),
            Some(PathBuf::from("/h/.local/share"))
        );
        assert_eq!(
            cache_path_for(false, &env),
            PathBuf::from("/h/.cache/remuda/update-check.json")
        );
        let env = env_of(&[("USERPROFILE", "/p")]);
        assert_eq!(
            data_home_for(false, &env),
            Some(PathBuf::from("/p/.local/share"))
        );
        assert_eq!(
            data_home_for(false, &env_of(&[("XDG_DATA_HOME", "")])),
            None
        );
        assert_eq!(
            data_home_for(false, &env_of(&[("XDG_DATA_HOME", "/x")])),
            Some(PathBuf::from("/x"))
        );
        assert_eq!(
            cache_path_for(false, &env_of(&[])),
            PathBuf::from(".cache/remuda/update-check.json")
        );
    }
}
