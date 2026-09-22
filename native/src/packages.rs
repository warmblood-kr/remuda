//! Built-in and installed Lua modules.
//!
//! Installed modules are validated and copied atomically below the user's
//! Remuda data directory. The daemon does not preload disk modules: each
//! `exec` resolves the current file, while an already-loaded Lua image keeps
//! its old definitions until the caller explicitly re-execs the module or
//! restarts the daemon.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_REPOSITORY_PART: usize = 128;

pub struct Builtin {
    pub name: &'static str,
    pub source: &'static str,
    pub subcommand: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub api: String,
    pub entry: String,
    pub command: Option<String>,
    pub source: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSource {
    pub source: String,
    pub chunk_name: String,
    pub origin: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub manifest: Manifest,
    pub path: PathBuf,
    pub repository: String,
    pub reference: Option<String>,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionSpec {
    pub name: String,
    pub version: String,
    pub api: String,
    pub entry: String,
    pub command: Option<String>,
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "butler",
        source: include_str!("../../packages/butler/init.lua"),
        subcommand: Some("butler"),
    },
    Builtin {
        name: "butler/telemetry",
        source: include_str!("../../packages/butler/telemetry.lua"),
        subcommand: None,
    },
    Builtin {
        name: "butler/mail",
        source: include_str!("../../packages/butler/mail.lua"),
        subcommand: None,
    },
    Builtin {
        name: "butler/agents/claudecode",
        source: include_str!("../../packages/butler/agents/claudecode.lua"),
        subcommand: None,
    },
    Builtin {
        name: "butler/agents/codex",
        source: include_str!("../../packages/butler/agents/codex.lua"),
        subcommand: None,
    },
];

pub fn builtin(name: &str) -> Option<&'static str> {
    BUILTINS
        .iter()
        .find(|package| package.name == name)
        .map(|package| package.source)
}

pub fn subcommand(name: &str) -> Option<&'static Builtin> {
    BUILTINS
        .iter()
        .find(|package| package.subcommand == Some(name))
}

fn builtin_entry(name: &str) -> String {
    if name.contains('/') {
        format!("packages/{name}.lua")
    } else {
        format!("packages/{name}/init.lua")
    }
}

/// Resolve an installed module first, then the embedded compatibility copy.
pub fn resolve(name: &str) -> Result<Option<PackageSource>, String> {
    if let Some(source) = installed_source(name)? {
        return Ok(Some(source));
    }
    Ok(builtin(name).map(|source| PackageSource {
        source: source.to_string(),
        chunk_name: builtin_entry(name),
        origin: "embedded".into(),
    }))
}

pub fn manifests() -> Result<Vec<Manifest>, String> {
    let installed = installed_specs()?;
    let mut entries = Vec::new();
    for package in BUILTINS {
        if installed.iter().any(|spec| {
            package.name == spec.name || package.name.starts_with(&format!("{}/", spec.name))
        }) {
            continue;
        }
        entries.push(Manifest {
            name: package.name.into(),
            version: crate::dist::BUILD_VERSION.into(),
            api: "embedded".into(),
            entry: builtin_entry(package.name),
            command: package.subcommand.map(str::to_string),
            source: "embedded".into(),
            status: "installed".into(),
        });
    }
    for spec in installed {
        entries.push(Manifest {
            name: spec.name,
            version: spec.version,
            api: spec.api,
            entry: spec.entry,
            command: spec.command,
            source: "disk".into(),
            status: "installed".into(),
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(entries)
}

pub fn manifest(name: &str) -> Result<Option<Manifest>, String> {
    if let Some(spec) = installed_specs()?
        .into_iter()
        .find(|spec| spec.name == name)
    {
        return Ok(Some(manifest_from_spec(spec, "disk")));
    }
    Ok(BUILTINS
        .iter()
        .find(|package| package.name == name)
        .map(|package| Manifest {
            name: package.name.into(),
            version: crate::dist::BUILD_VERSION.into(),
            api: "embedded".into(),
            entry: builtin_entry(package.name),
            command: package.subcommand.map(str::to_string),
            source: "embedded".into(),
            status: "installed".into(),
        }))
}

fn manifest_from_spec(spec: ExtensionSpec, source: &str) -> Manifest {
    Manifest {
        name: spec.name,
        version: spec.version,
        api: spec.api,
        entry: spec.entry,
        command: spec.command,
        source: source.into(),
        status: "installed".into(),
    }
}

/// Clone and install one GitHub extension. A missing reference follows the
/// repository's default branch; a supplied reference is passed as an exact
/// `git clone --branch` argument after rejecting shell/control syntax.
pub fn install(
    repository: &str,
    reference: Option<&str>,
    force: bool,
) -> Result<InstallReport, String> {
    let (owner, repo, url) = github_repository(repository)?;
    if let Some(reference) = reference {
        validate_reference(reference)?;
    }
    let checkout = temporary_path("checkout")?;
    let result = install_from_checkout(
        &checkout,
        &owner,
        &repo,
        &url,
        reference.map(str::to_string),
        force,
    );
    let _ = fs::remove_dir_all(&checkout);
    result
}

fn install_from_checkout(
    checkout: &Path,
    owner: &str,
    repo: &str,
    url: &str,
    reference: Option<String>,
    force: bool,
) -> Result<InstallReport, String> {
    let mut command = Command::new("git");
    command.args(["clone", "--depth", "1", "--no-tags"]);
    if let Some(reference) = &reference {
        command.args(["--branch", reference]);
    }
    command.arg(url).arg(checkout);
    let output = command
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git clone failed for {owner}/{repo}: {}",
            command_error(&output.stderr)
        ));
    }
    let manifest_path = checkout.join("extension.toml");
    let spec = read_manifest(&manifest_path)?;
    let entry = safe_relative_path(&spec.entry, "entry")?;
    let entry_path = checkout.join(&entry);
    ensure_regular_file(&entry_path, "manifest entry")?;
    let package_root = entry_path
        .parent()
        .ok_or_else(|| "manifest entry has no package directory".to_string())?;
    let package_relative = package_root
        .strip_prefix(checkout)
        .map_err(|_| "manifest entry escaped checkout".to_string())?;
    let data = extensions_dir()?;
    fs::create_dir_all(&data)
        .map_err(|error| format!("cannot create {}: {error}", data.display()))?;
    let target = data.join(&spec.name);
    let staging = temporary_path_in(&data, "install")?;
    let staging_package = staging.join(package_relative);
    copy_tree(package_root, &staging_package)?;
    fs::copy(&manifest_path, staging.join("extension.toml"))
        .map_err(|error| format!("cannot copy extension.toml: {error}"))?;
    let commit = git_commit(checkout)?;
    fs::write(
        staging.join("source"),
        format!("https://github.com/{owner}/{repo}.git\ncommit={commit}\n"),
    )
    .map_err(|error| format!("cannot write source metadata: {error}"))?;
    if target.exists() {
        if !force {
            let _ = fs::remove_dir_all(&staging);
            return Err(format!(
                "mod {} is already installed; pass --force to replace it",
                spec.name
            ));
        }
        ensure_safe_extension_target(&target)?;
        fs::remove_dir_all(&target)
            .map_err(|error| format!("cannot replace {}: {error}", target.display()))?;
    }
    fs::rename(&staging, &target).map_err(|error| format!("cannot commit mod install: {error}"))?;
    Ok(InstallReport {
        manifest: manifest_from_spec(spec, "disk"),
        path: target,
        repository: format!("{owner}/{repo}"),
        reference,
        commit,
    })
}

pub fn parse_repository(input: &str) -> Result<(String, String, String), String> {
    github_repository(input)
}

fn github_repository(input: &str) -> Result<(String, String, String), String> {
    let value = input.trim();
    let path = if let Some(path) = value.strip_prefix("https://github.com/") {
        path
    } else if value.starts_with("http://github.com/") {
        return Err("GitHub mod URLs must use https".into());
    } else {
        value
    };
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || !valid_component(owner)
        || !valid_component(repo)
        || owner.len() > MAX_REPOSITORY_PART
        || repo.len() > MAX_REPOSITORY_PART
    {
        return Err(format!(
            "invalid GitHub repository {input:?}; use OWNER/REPO or https://github.com/OWNER/REPO"
        ));
    }
    Ok((
        owner.into(),
        repo.into(),
        format!("https://github.com/{owner}/{repo}.git"),
    ))
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_reference(reference: &str) -> Result<(), String> {
    if reference.is_empty()
        || reference.len() > 256
        || reference.starts_with('-')
        || reference.contains("..")
        || reference.contains('\\')
        || reference
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err("invalid mod ref; use a branch, tag, or commit name".into());
    }
    Ok(())
}

fn parse_quoted(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.len() < 2 || !value.starts_with('"') || !value.ends_with('"') {
        return Err("manifest values must be double-quoted strings".into());
    }
    let inner = &value[1..value.len() - 1];
    let mut output = String::new();
    let mut escaped = false;
    for character in inner.chars() {
        if escaped {
            match character {
                '"' | '\\' => output.push(character),
                _ => return Err("manifest contains an unsupported escape".into()),
            }
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '\n' || character == '\r' {
            return Err("manifest strings cannot contain newlines".into());
        } else {
            output.push(character);
        }
    }
    if escaped {
        return Err("manifest ends with an incomplete escape".into());
    }
    Ok(output)
}

pub fn parse_manifest(text: &str) -> Result<ExtensionSpec, String> {
    let mut name = None;
    let mut version = None;
    let mut api = None;
    let mut entry = None;
    let mut command = None;
    for (line_number, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("extension.toml line {} is not key = value", line_number + 1))?;
        let value = parse_quoted(value)?;
        let slot = match key.trim() {
            "name" => &mut name,
            "version" => &mut version,
            "api" => &mut api,
            "entry" => &mut entry,
            "command" => &mut command,
            other => return Err(format!("extension.toml has unknown key {other:?}")),
        };
        if slot.replace(value).is_some() {
            return Err(format!("extension.toml repeats key {:?}", key.trim()));
        }
    }
    let spec = ExtensionSpec {
        name: name.ok_or_else(|| "extension.toml is missing name".to_string())?,
        version: version.unwrap_or_else(|| "0.1.0".into()),
        api: api.ok_or_else(|| "extension.toml is missing api".to_string())?,
        entry: entry.ok_or_else(|| "extension.toml is missing entry".to_string())?,
        command,
    };
    validate_spec(&spec)?;
    Ok(spec)
}

fn validate_spec(spec: &ExtensionSpec) -> Result<(), String> {
    if !valid_component(&spec.name) {
        return Err(format!("invalid mod name {:?}", spec.name));
    }
    if spec.api != "remuda-lua-v1" {
        return Err(format!("unsupported mod API {:?}", spec.api));
    }
    let expected = format!("packages/{}/init.lua", spec.name);
    if spec.entry != expected {
        return Err(format!("entry must be {expected}"));
    }
    safe_relative_path(&spec.entry, "entry")?;
    if let Some(command) = &spec.command {
        if !valid_component(command) {
            return Err(format!("invalid mod command {:?}", command));
        }
    }
    Ok(())
}

fn safe_relative_path(value: &str, label: &str) -> Result<PathBuf, String> {
    if value.is_empty() || value.contains('\\') {
        return Err(format!("{label} must be a relative forward-slash path"));
    }
    let path = PathBuf::from(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(format!("{label} escapes the mod directory"));
    }
    Ok(path)
}

fn read_manifest(path: &Path) -> Result<ExtensionSpec, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_MANIFEST_BYTES {
        return Err("extension.toml must be a regular file no larger than 64 KiB".into());
    }
    let text =
        fs::read_to_string(path).map_err(|error| format!("cannot read extension.toml: {error}"))?;
    parse_manifest(&text)
}

fn installed_source(name: &str) -> Result<Option<PackageSource>, String> {
    if !valid_package_name(name) {
        return Ok(None);
    }
    let extension = name.split('/').next().unwrap_or(name);
    let root = extensions_dir()?.join(extension);
    let root_type = match fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "cannot inspect installed mod {}: {error}",
                root.display()
            ))
        }
    };
    if !root_type.is_dir() || root_type.file_type().is_symlink() {
        return Err(format!(
            "installed mod path {} is not a real directory",
            root.display()
        ));
    }
    let spec = read_manifest(&root.join("extension.toml"))?;
    if spec.name != extension {
        return Err(format!(
            "installed mod directory {extension} disagrees with manifest name {}",
            spec.name
        ));
    }
    let entry = safe_relative_path(&spec.entry, "entry")?;
    let entry_path = root.join(&entry);
    let package_root = entry_path
        .parent()
        .ok_or_else(|| "installed mod entry has no package directory".to_string())?;
    let path = if name == extension {
        entry_path
    } else {
        let suffix = name
            .strip_prefix(extension)
            .and_then(|suffix| suffix.strip_prefix('/'))
            .ok_or_else(|| "invalid installed package name".to_string())?;
        package_root
            .join(safe_relative_path(suffix, "package name")?)
            .with_extension("lua")
    };
    ensure_regular_file(&path, "installed package")?;
    let chunk_name = path
        .strip_prefix(&root)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| spec.entry.clone());
    Ok(Some(PackageSource {
        source: fs::read_to_string(&path).map_err(|error| {
            format!("cannot read installed package {}: {error}", path.display())
        })?,
        chunk_name,
        origin: format!("disk:{}", root.display()),
    }))
}

fn installed_specs() -> Result<Vec<ExtensionSpec>, String> {
    let root = extensions_dir()?;
    let Ok(entries) = fs::read_dir(&root) else {
        return Ok(Vec::new());
    };
    let mut specs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot inspect {}: {error}", root.display()))?;
        if !entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            continue;
        }
        specs.push(read_manifest(&entry.path().join("extension.toml"))?);
    }
    Ok(specs)
}

fn extensions_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or_else(|| "HOME or XDG_DATA_HOME is required for mods".to_string())?;
    Ok(base.join("remuda").join("extensions"))
}

fn temporary_path(label: &str) -> Result<PathBuf, String> {
    temporary_path_in(&std::env::temp_dir(), label)
}

fn temporary_path_in(parent: &Path, label: &str) -> Result<PathBuf, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let path = parent.join(format!("remuda-mod-{label}-{}-{stamp}", std::process::id()));
    fs::create_dir(&path).map_err(|error| format!("cannot create temporary directory: {error}"))?;
    Ok(path)
}

fn ensure_regular_file(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot access {label} {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!("{label} {} must be a regular file", path.display()));
    }
    Ok(())
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("cannot inspect {}: {error}", source.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("mod package contains symlink {}", source.display()));
    }
    if metadata.is_dir() {
        fs::create_dir_all(target)
            .map_err(|error| format!("cannot create {}: {error}", target.display()))?;
        for entry in fs::read_dir(source)
            .map_err(|error| format!("cannot read {}: {error}", source.display()))?
        {
            let entry = entry.map_err(|error| error.to_string())?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        if source.extension() != Some(std::ffi::OsStr::new("lua")) {
            return Err(format!(
                "mod package contains non-Lua file {}",
                source.display()
            ));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        fs::copy(source, target)
            .map_err(|error| format!("cannot copy {}: {error}", source.display()))?;
    } else {
        return Err(format!(
            "mod package contains unsupported file {}",
            source.display()
        ));
    }
    Ok(())
}

fn ensure_safe_extension_target(path: &Path) -> Result<(), String> {
    let root = extensions_dir()?;
    if path.parent() != Some(root.as_path()) {
        return Err("refusing to replace a mod outside the mod directory".into());
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect existing mod {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "refusing to replace non-directory mod {}",
            path.display()
        ));
    }
    Ok(())
}

fn git_commit(checkout: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|error| format!("cannot read cloned commit: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot read cloned commit: {}",
            command_error(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn command_error(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).trim().to_string();
    if text.is_empty() {
        "unknown error".into()
    } else {
        text
    }
}

fn valid_package_name(name: &str) -> bool {
    !name.is_empty() && name.split('/').all(valid_component)
}

#[cfg(test)]
mod tests {
    use super::{parse_manifest, parse_repository, validate_reference};

    #[test]
    fn parses_the_published_butler_manifest_shape() {
        let manifest = parse_manifest(
            r#"
            name = "butler"
            entry = "packages/butler/init.lua"
            api = "remuda-lua-v1"
            command = "remuda-butler"
            "#,
        )
        .expect("manifest");
        assert_eq!(manifest.name, "butler");
        assert_eq!(manifest.version, "0.1.0");
    }

    #[test]
    fn repository_resolution_only_allows_github_owner_repo() {
        assert_eq!(
            parse_repository("warmblood-kr/remuda-butler").unwrap().2,
            "https://github.com/warmblood-kr/remuda-butler.git"
        );
        assert!(parse_repository("https://example.com/a/b").is_err());
        assert!(parse_repository("warmblood-kr/../private").is_err());
        assert!(parse_repository("http://github.com/a/b").is_err());
    }

    #[test]
    fn manifest_and_refs_reject_escape_and_option_injection() {
        assert!(parse_manifest(
            r#"name = "bad"
entry = "../init.lua"
api = "remuda-lua-v1""#
        )
        .is_err());
        assert!(validate_reference("--upload-pack=sh").is_err());
        assert!(validate_reference("feature\nattack").is_err());
    }
}
