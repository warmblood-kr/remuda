//! Installed Lua mods.
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
pub const LUA_API_VERSION: &str = "remuda-lua-v1";
pub const MOD_LIFECYCLE_API: &str = "remuda-module-v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub api: String,
    pub entry: String,
    pub command: Option<String>,
    pub lifecycle: Option<String>,
    pub source: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSource {
    pub source: String,
    pub chunk_name: String,
    pub lifecycle: Option<String>,
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
pub struct UpdateFailure {
    pub name: String,
    pub error: String,
}

/// Per-mod results from a batch update. The first failed update stops the
/// batch; later names are reported as not attempted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAllReport {
    pub updated: Vec<InstallReport>,
    pub failed: Option<UpdateFailure>,
    pub not_attempted: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveReport {
    pub manifest: Manifest,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModSpec {
    pub name: String,
    pub version: String,
    pub api: String,
    pub entry: String,
    pub command: Option<String>,
    pub lifecycle: Option<String>,
    /// `(mod, constraint)` pairs from `requires`, sorted by mod name.
    pub requires: Vec<(String, String)>,
}

/// Return the installed mod name that owns a CLI command. Commands are
/// discovered from manifests; no mod command is compiled into Remuda.
pub fn subcommand(name: &str) -> Result<Option<String>, String> {
    Ok(installed_specs()?
        .into_iter()
        .find(|spec| spec.command.as_deref() == Some(name))
        .map(|spec| spec.name))
}

pub fn has_subcommand(name: &str) -> bool {
    subcommand(name).ok().flatten().is_some()
}

/// `mods/NAME/` exists but its manifest is missing or unreadable, typically
/// mid-update (#134). The message names the mod and where it is, instead of
/// the CLI falling through to its generic usage text.
pub fn half_installed(name: &str) -> Option<String> {
    let dir = mods_dir().ok()?.join(name);
    if !dir.is_dir() {
        return None;
    }
    let reason = read_manifest(&dir.join("extension.toml")).err()?;
    Some(format!(
        "mod '{name}' at {} has no manifest (partially installed or mid-update?): {reason}",
        dir.display()
    ))
}

/// Resolve only installed modules. Mods are deliberately independent from the
/// Remuda binary; there is no embedded compatibility copy.
pub fn resolve(name: &str) -> Result<Option<PackageSource>, String> {
    installed_source(name)
}

pub fn manifests() -> Result<Vec<Manifest>, String> {
    let installed = installed_specs()?;
    let mut entries = Vec::new();
    for spec in installed {
        entries.push(Manifest {
            name: spec.name,
            version: spec.version,
            api: spec.api,
            entry: spec.entry,
            command: spec.command,
            lifecycle: spec.lifecycle,
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
    Ok(None)
}

fn manifest_from_spec(spec: ModSpec, source: &str) -> Manifest {
    Manifest {
        name: spec.name,
        version: spec.version,
        api: spec.api,
        entry: spec.entry,
        command: spec.command,
        lifecycle: spec.lifecycle,
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
    let github = github_repository(repository)?;
    if let Some(reference) = reference {
        validate_reference(reference)?;
    }
    let checkout = temporary_path("checkout")?;
    let result = install_from_checkout(
        &checkout,
        &github,
        reference.map(str::to_string),
        force,
        None,
        false,
    );
    cleanup_tree(&checkout);
    result
}

/// Update one installed mod from the repository and reference recorded at
/// install time. The clone is fully validated before the existing directory
/// is moved aside, so a failed fetch or validation leaves the old mod intact.
pub fn update(name: &str) -> Result<InstallReport, String> {
    update_inner(name, false)
}

/// Update a mod only when the fetched candidate still opts into the in-process
/// lifecycle. Candidate validation happens before the installed directory is
/// changed.
pub fn update_lifecycle(name: &str) -> Result<InstallReport, String> {
    update_inner(name, true)
}

fn update_inner(name: &str, require_lifecycle: bool) -> Result<InstallReport, String> {
    if !valid_component(name) {
        return Err(format!("invalid installed mod name {name:?}"));
    }
    let root = mods_dir()?.join(name);
    let metadata = fs::symlink_metadata(&root)
        .map_err(|error| format!("cannot inspect installed mod {name}: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("installed mod {name} is not a real directory"));
    }
    let spec = read_manifest(&root.join("extension.toml"))?;
    if spec.name != name {
        return Err(format!(
            "installed mod directory {name} disagrees with manifest name {}",
            spec.name
        ));
    }
    let (repository, reference) = read_source_metadata(&root.join("source"))?;
    let github = github_repository(&repository)?;
    let checkout = temporary_path("update")?;
    let result = install_from_checkout(
        &checkout,
        &github,
        reference,
        true,
        Some(name),
        require_lifecycle,
    );
    cleanup_tree(&checkout);
    result
}

/// Update every installed mod, preserving each mod's recorded source and
/// retaining outcomes if an update fails partway through.
pub fn update_all() -> Result<UpdateAllReport, String> {
    let mut names: Vec<_> = installed_specs()?
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    names.sort();
    Ok(update_all_with(names, update))
}

fn update_all_with<F>(names: Vec<String>, mut update_one: F) -> UpdateAllReport
where
    F: FnMut(&str) -> Result<InstallReport, String>,
{
    let mut updated = Vec::new();
    for (index, name) in names.iter().enumerate() {
        match update_one(name) {
            Ok(report) => updated.push(report),
            Err(error) => {
                return UpdateAllReport {
                    updated,
                    failed: Some(UpdateFailure {
                        name: name.clone(),
                        error,
                    }),
                    not_attempted: names[index + 1..].to_vec(),
                };
            }
        }
    }
    UpdateAllReport {
        updated,
        failed: None,
        not_attempted: Vec::new(),
    }
}

/// Remove one explicitly installed mod. The manifest is checked before the
/// directory is removed, so a malformed or substituted path is never treated
/// as an extension owned by Remuda.
pub fn remove(name: &str) -> Result<RemoveReport, String> {
    if !valid_component(name) {
        return Err(format!("invalid installed mod name {name:?}"));
    }
    let root = mods_dir()?.join(name);
    let metadata = fs::symlink_metadata(&root)
        .map_err(|error| format!("cannot inspect installed mod {name}: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("installed mod {name} is not a real directory"));
    }
    ensure_safe_extension_target(&root)?;
    let spec = read_manifest(&root.join("extension.toml"))?;
    if spec.name != name {
        return Err(format!(
            "installed mod directory {name} disagrees with manifest name {}",
            spec.name
        ));
    }
    let dependents = dependents(name)?;
    if !dependents.is_empty() {
        let list = dependents.join(", ");
        return Err(format!(
            "mod {name} is required by {list}; remove {list} first"
        ));
    }
    remove_tree(&root).map_err(|error| format!("cannot remove mod {name}: {error}"))?;
    Ok(RemoveReport {
        manifest: manifest_from_spec(spec, "removed"),
        path: root,
    })
}

/// Validate a local mod checkout without installing or mutating a daemon:
/// manifest, paths, symlinks, Lua syntax, and host API version. The
/// deterministic half of the extension harness, run before an install test.
pub fn test_path(path: &Path) -> Result<ModSpec, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect mod checkout {}: {error}", path.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "mod checkout {} must be a real directory",
            path.display()
        ));
    }
    let spec = read_manifest(&path.join("extension.toml"))?;
    let entry = safe_relative_path(&spec.entry, "entry")?;
    let entry_path = path.join(&entry);
    ensure_regular_file(&entry_path, "manifest entry")?;
    let package_root = entry_path
        .parent()
        .ok_or_else(|| "manifest entry has no package directory".to_string())?;
    let (_, package_root) = checked_package_root(path, package_root)?;
    validate_lua_tree(&package_root)?;
    Ok(spec)
}

fn install_from_checkout(
    checkout: &Path,
    (owner, repo, url): &(String, String, String),
    reference: Option<String>,
    force: bool,
    expected_name: Option<&str>,
    require_lifecycle: bool,
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
    for (dependency, constraint) in &spec.requires {
        check_requirement(&spec.name, dependency, constraint)?;
    }
    if let Some(expected_name) = expected_name {
        if spec.name != expected_name {
            return Err(format!(
                "repository manifest {} does not match requested mod {}",
                spec.name, expected_name
            ));
        }
    }
    if require_lifecycle && spec.lifecycle.is_none() {
        return Err(format!(
            "mod {} does not declare lifecycle {}; installed files were not changed",
            spec.name, MOD_LIFECYCLE_API
        ));
    }
    let entry = safe_relative_path(&spec.entry, "entry")?;
    let entry_path = checkout.join(&entry);
    ensure_regular_file(&entry_path, "manifest entry")?;
    let package_root = entry_path
        .parent()
        .ok_or_else(|| "manifest entry has no package directory".to_string())?;
    let (canonical_checkout, package_root) = checked_package_root(checkout, package_root)?;
    validate_lua_tree(&package_root)?;
    let package_relative = package_root
        .strip_prefix(&canonical_checkout)
        .map_err(|_| "manifest entry escaped checkout".to_string())?;
    let data = mods_dir()?;
    let target = data.join(&spec.name);
    let mut staging = stage_package_tree(&data, &package_root, package_relative)?;
    copy_file_with_safe_mode(&manifest_path, &staging.path().join("extension.toml"))?;
    let commit = git_commit(checkout)?;
    let mut source = format!("https://github.com/{owner}/{repo}.git\ncommit={commit}\n");
    if let Some(reference) = &reference {
        source.push_str("ref=");
        source.push_str(reference);
        source.push('\n');
    }
    let source_path = staging.path().join("source");
    fs::write(&source_path, source)
        .map_err(|error| format!("cannot write source metadata: {error}"))?;
    mask_installed_file_mode(&source_path)?;
    if fs::symlink_metadata(&target).is_ok() {
        if !force {
            return Err(format!(
                "mod {} is already installed; pass --force to replace it",
                spec.name
            ));
        }
        ensure_safe_extension_target(&target)?;
        let backup = temporary_path_in(&data, "backup")?;
        fs::remove_dir_all(&backup)
            .map_err(|error| format!("cannot prepare backup {}: {error}", backup.display()))?;
        fs::rename(&target, &backup)
            .map_err(|error| format!("cannot stage existing mod {}: {error}", target.display()))?;
        fs::rename(staging.path(), &target).map_err(|error| {
            let _ = fs::rename(&backup, &target);
            format!("cannot commit mod install: {error}")
        })?;
        staging.disarm();
        remove_tree(&backup).map_err(|error| format!("cannot remove old mod backup: {error}"))?;
    } else {
        fs::rename(staging.path(), &target)
            .map_err(|error| format!("cannot commit mod install: {error}"))?;
        staging.disarm();
    }
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

pub fn parse_manifest(text: &str) -> Result<ModSpec, String> {
    let mut name = None;
    let mut version = None;
    let mut api = None;
    let mut entry = None;
    let mut command = None;
    let mut lifecycle = None;
    let mut requires = None;
    for (line_number, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("extension.toml line {} is not key = value", line_number + 1))?;
        if key.trim() == "requires" {
            if requires.replace(parse_requires(value)?).is_some() {
                return Err("extension.toml repeats key \"requires\"".into());
            }
            continue;
        }
        let value = parse_quoted(value)?;
        let slot = match key.trim() {
            "name" => &mut name,
            "version" => &mut version,
            "api" => &mut api,
            "entry" => &mut entry,
            "command" => &mut command,
            "lifecycle" => &mut lifecycle,
            other => return Err(format!("extension.toml has unknown key {other:?}")),
        };
        if slot.replace(value).is_some() {
            return Err(format!("extension.toml repeats key {:?}", key.trim()));
        }
    }
    let spec = ModSpec {
        name: name.ok_or_else(|| "extension.toml is missing name".to_string())?,
        version: version.unwrap_or_else(|| "0.1.0".into()),
        api: api.ok_or_else(|| "extension.toml is missing api".to_string())?,
        entry: entry.ok_or_else(|| "extension.toml is missing entry".to_string())?,
        command,
        lifecycle,
        requires: requires.unwrap_or_default(),
    };
    validate_spec(&spec)?;
    if spec.requires.iter().any(|(name, _)| *name == spec.name) {
        return Err(format!("mod {} cannot require itself", spec.name));
    }
    Ok(spec)
}

/// `requires = { name = "constraint", ... }` on one line (constraints hold
/// commas, so pairs are read quote by quote, not split on `,`).
fn parse_requires(value: &str) -> Result<Vec<(String, String)>, String> {
    let shape = "requires must be an inline table like { butler = \">=0.4, <0.5\" }";
    let mut rest = value
        .trim()
        .strip_prefix('{')
        .and_then(|inner| inner.strip_suffix('}'))
        .ok_or(shape)?
        .trim();
    let mut requires = std::collections::BTreeMap::new();
    while !rest.is_empty() {
        let (name, after) = rest.split_once('=').ok_or(shape)?;
        let name = name.trim();
        if !valid_component(name) {
            return Err(format!("invalid required mod name {name:?}"));
        }
        let quoted = after.trim_start().strip_prefix('"').ok_or(shape)?;
        let (constraint, after) = quoted.split_once('"').ok_or(shape)?;
        parse_constraint(constraint)?;
        if requires
            .insert(name.to_string(), constraint.to_string())
            .is_some()
        {
            return Err(format!("requires names {name} twice"));
        }
        rest = after.trim_start();
        rest = rest.strip_prefix(',').unwrap_or(rest).trim_start();
    }
    Ok(requires.into_iter().collect())
}

/// Comma-separated comparators (`>=`, `>`, `<=`, `<`, `=`), all of which must
/// hold; empty or `*` means any version.
fn parse_constraint(constraint: &str) -> Result<Vec<(&str, [u64; 3])>, String> {
    let constraint = constraint.trim();
    if constraint.is_empty() || constraint == "*" {
        return Ok(Vec::new());
    }
    constraint
        .split(',')
        .map(|part| {
            let part = part.trim();
            let op = [">=", "<=", ">", "<", "="]
                .into_iter()
                .find(|op| part.starts_with(op))
                .ok_or_else(|| format!("constraint {part:?} needs >=, >, <=, < or ="))?;
            Ok((op, parse_version(&part[op.len()..])?))
        })
        .collect()
}

fn parse_version(text: &str) -> Result<[u64; 3], String> {
    let parts: Vec<&str> = text.trim().split('.').collect();
    let mut version = [0; 3];
    if parts.len() > 3 {
        return Err(format!("version {text:?} has more than three parts"));
    }
    for (slot, part) in version.iter_mut().zip(parts) {
        *slot = part
            .parse()
            .map_err(|_| format!("version {text:?} is not numeric"))?;
    }
    Ok(version)
}

/// `*` accepts any version unparsed; otherwise the numeric core decides, so
/// `0.1.0-nightly.X` or `1.2.3+build` compare as `0.1.0` and `1.2.3`.
pub fn satisfies(version: &str, constraint: &str) -> Result<bool, String> {
    let comparators = parse_constraint(constraint)?;
    if comparators.is_empty() {
        return Ok(true);
    }
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let installed = parse_version(core)?;
    Ok(comparators.iter().all(|(op, wanted)| match *op {
        ">=" => installed >= *wanted,
        "<=" => installed <= *wanted,
        ">" => installed > *wanted,
        "<" => installed < *wanted,
        _ => installed == *wanted,
    }))
}

fn validate_spec(spec: &ModSpec) -> Result<(), String> {
    if !valid_component(&spec.name) {
        return Err(format!("invalid mod name {:?}", spec.name));
    }
    if spec.api != LUA_API_VERSION {
        return Err(format!("unsupported mod API {:?}", spec.api));
    }
    if spec
        .lifecycle
        .as_deref()
        .is_some_and(|api| api != MOD_LIFECYCLE_API)
    {
        return Err(format!(
            "unsupported mod lifecycle API {:?}",
            spec.lifecycle.as_deref().unwrap()
        ));
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

fn validate_lua_tree(path: &Path) -> Result<(), String> {
    for entry in fs::read_dir(path)
        .map_err(|error| format!("cannot read mod package {}: {error}", path.display()))?
    {
        let entry = entry.map_err(|error| error.to_string())?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child)
            .map_err(|error| format!("cannot inspect {}: {error}", child.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("mod package contains symlink {}", child.display()));
        }
        if metadata.is_dir() {
            validate_lua_tree(&child)?;
        } else if metadata.is_file() {
            if child.extension() != Some(std::ffi::OsStr::new("lua")) {
                return Err(format!(
                    "mod package contains non-Lua file {}",
                    child.display()
                ));
            }
            let source = fs::read_to_string(&child)
                .map_err(|error| format!("cannot read {}: {error}", child.display()))?;
            mlua::Lua::new()
                .load(&source)
                .set_name(child.to_string_lossy())
                .into_function()
                .map_err(|error| format!("Lua syntax error in {}: {error}", child.display()))?;
        } else {
            return Err(format!(
                "mod package contains unsupported file {}",
                child.display()
            ));
        }
    }
    Ok(())
}

/// Resolve the package directory without following any symlinked component
/// between the checkout root and the package root.
fn checked_package_root(
    checkout: &Path,
    package_root: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    let checkout_metadata = fs::symlink_metadata(checkout)
        .map_err(|error| format!("cannot inspect checkout {}: {error}", checkout.display()))?;
    if checkout_metadata.file_type().is_symlink() || !checkout_metadata.is_dir() {
        return Err(format!(
            "checkout {} must be a real directory",
            checkout.display()
        ));
    }
    let relative = package_root
        .strip_prefix(checkout)
        .map_err(|_| "manifest entry escaped checkout".to_string())?;
    let mut current = checkout.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(name) => current.push(name),
            Component::CurDir => continue,
            _ => return Err("manifest entry escaped checkout".into()),
        }
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            format!("cannot inspect package path {}: {error}", current.display())
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "mod package path contains symlink {}",
                current.display()
            ));
        }
    }
    let canonical_checkout = fs::canonicalize(checkout).map_err(|error| {
        format!(
            "cannot canonicalize checkout {}: {error}",
            checkout.display()
        )
    })?;
    let canonical_package = fs::canonicalize(package_root).map_err(|error| {
        format!(
            "cannot canonicalize package root {}: {error}",
            package_root.display()
        )
    })?;
    if !canonical_package.starts_with(&canonical_checkout) {
        return Err("manifest entry escaped checkout".into());
    }
    if !canonical_package.is_dir() {
        return Err(format!(
            "mod package root {} is not a directory",
            canonical_package.display()
        ));
    }
    Ok((canonical_checkout, canonical_package))
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

fn read_manifest(path: &Path) -> Result<ModSpec, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_MANIFEST_BYTES {
        return Err("extension.toml must be a regular file no larger than 64 KiB".into());
    }
    let text =
        fs::read_to_string(path).map_err(|error| format!("cannot read extension.toml: {error}"))?;
    parse_manifest(&text)
}

fn read_source_metadata(path: &Path) -> Result<(String, Option<String>), String> {
    let text = fs::read_to_string(path).map_err(|error| {
        format!(
            "cannot read mod source metadata {}: {error}",
            path.display()
        )
    })?;
    let mut repository = None;
    let mut reference = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("https://github.com/") {
            repository = Some(value.strip_suffix(".git").unwrap_or(value).to_string());
        } else if let Some(value) = line.strip_prefix("ref=") {
            validate_reference(value)?;
            reference = Some(value.to_string());
        }
    }
    let repository =
        repository.ok_or_else(|| "mod source metadata has no GitHub repository".to_string())?;
    github_repository(&repository)?;
    Ok((repository, reference))
}

fn installed_source(name: &str) -> Result<Option<PackageSource>, String> {
    if !valid_package_name(name) {
        return Ok(None);
    }
    let extension = name.split('/').next().unwrap_or(name);
    let root = mods_dir()?.join(extension);
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
    let lifecycle = if name == extension {
        spec.lifecycle.clone()
    } else {
        None
    };
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
        lifecycle,
    }))
}

fn installed_specs() -> Result<Vec<ModSpec>, String> {
    let root = mods_dir()?;
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
        // One half-installed mod (#134) must not hide the rest: skip it and
        // say so once per process. Its own command still gets a named error
        // from `half_installed`.
        match read_manifest(&entry.path().join("extension.toml")) {
            Ok(spec) => specs.push(spec),
            Err(reason) => warn_skipped(&entry.path(), &reason),
        }
    }
    Ok(specs)
}

fn warn_skipped(dir: &Path, reason: &str) {
    static WARNED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
    let mut warned = WARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !warned.iter().any(|seen| seen == dir) {
        eprintln!(
            "remuda: skipping mod at {}: no readable manifest ({reason})",
            dir.display()
        );
        warned.push(dir.to_path_buf());
    }
}

/// Installed mods whose `requires` names `name`. A manifest that no longer
/// parses cannot hold a dependency, so it is skipped rather than fatal.
fn dependents(name: &str) -> Result<Vec<String>, String> {
    let Ok(entries) = fs::read_dir(mods_dir()?) else {
        return Ok(Vec::new());
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| read_manifest(&entry.path().join("extension.toml")).ok())
        .filter(|spec| spec.requires.iter().any(|(required, _)| required == name))
        .map(|spec| spec.name)
        .collect();
    names.sort();
    Ok(names)
}

/// `owner` needs `dependency` installed at a version meeting `constraint`.
fn check_requirement(owner: &str, dependency: &str, constraint: &str) -> Result<ModSpec, String> {
    let path = mods_dir()?.join(dependency).join("extension.toml");
    if !path.is_file() {
        return Err(format!(
            "mod {owner} requires {dependency}, which is not installed"
        ));
    }
    let spec = read_manifest(&path).map_err(|error| {
        format!(
            "mod {owner} requires {dependency}, but its manifest at {} is unreadable (partially installed or mid-update?): {error}",
            path.display()
        )
    })?;
    let met = satisfies(&spec.version, constraint)
        .map_err(|error| format!("mod {owner} requires {dependency} {constraint}, but installed {dependency} {} is unreadable: {error}", spec.version))?;
    if !met {
        return Err(format!(
            "mod {owner} requires {dependency} {constraint}, but {dependency} {} is installed",
            spec.version
        ));
    }
    Ok(spec)
}

/// Every mod `name` needs, transitively and checked, hosts first (ties by
/// name); `name` itself is not included. A cycle is refused with its path.
pub fn requirement_order(name: &str) -> Result<Vec<String>, String> {
    fn visit(
        name: &str,
        requires: &[(String, String)],
        path: &mut Vec<String>,
        done: &mut Vec<String>,
    ) -> Result<(), String> {
        path.push(name.to_string());
        for (dependency, constraint) in requires {
            if done.contains(dependency) {
                continue;
            }
            if let Some(at) = path.iter().position(|seen| seen == dependency) {
                let mut cycle = path[at..].to_vec();
                cycle.push(dependency.clone());
                return Err(format!("mod requires form a cycle: {}", cycle.join(" -> ")));
            }
            let spec = check_requirement(name, dependency, constraint)?;
            visit(dependency, &spec.requires, path, done)?;
        }
        path.pop();
        done.push(name.to_string());
        Ok(())
    }
    let root = mods_dir()?.join(name).join("extension.toml");
    let Ok(spec) = read_manifest(&root) else {
        return Ok(Vec::new());
    };
    let mut done = Vec::new();
    visit(name, &spec.requires, &mut Vec::new(), &mut done)?;
    done.pop();
    Ok(done)
}

fn mods_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or_else(|| "HOME or XDG_DATA_HOME is required for mods".to_string())?;
    let remuda = base.join("remuda");
    let mods = remuda.join("mods");
    let legacy = remuda.join("extensions");
    if !mods.exists() && legacy.is_dir() {
        fs::rename(&legacy, &mods).map_err(|error| {
            format!(
                "cannot migrate legacy mod directory {} to {}: {error}",
                legacy.display(),
                mods.display()
            )
        })?;
    }
    resolve_mods_root(&mods)
}

fn resolve_mods_root(path: &Path) -> Result<PathBuf, String> {
    if let Some(data_dir) = path.parent() {
        if data_dir.exists() {
            validate_data_dir(data_dir)?;
        }
    }
    if !path.exists() {
        return Ok(path.to_path_buf());
    }
    let Some(resolved) = canonicalize_mods_root(path)? else {
        return Ok(path.to_path_buf());
    };
    validate_mods_root(&resolved)?;
    Ok(resolved)
}

fn canonicalize_mods_root(path: &Path) -> Result<Option<PathBuf>, String> {
    match fs::canonicalize(path) {
        Ok(resolved) => Ok(Some(resolved)),
        // The directory may disappear after resolve_mods_root's exists check.
        // Treat that the same as a directory that was already absent.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "cannot resolve mods directory {}: {error}",
            path.display()
        )),
    }
}

fn validate_data_dir(path: &Path) -> Result<(), String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        // Its caller checks existence first, so a missing path here means it
        // disappeared in between and should be treated as absent.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "cannot inspect Remuda data directory {}: {error}",
                path.display()
            ));
        }
    };
    if !metadata.is_dir() {
        return Err(format!(
            "Remuda data path {} is not a directory",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions and returns the current effective uid.
        let euid = unsafe { libc::geteuid() };
        if metadata.uid() != euid {
            return Err(format!(
                "Remuda data directory {} must be owned by the current user",
                path.display()
            ));
        }
        if metadata.permissions().mode() & 0o002 != 0 {
            return Err(format!(
                "Remuda data directory {} must not be world-writable",
                path.display()
            ));
        }
    }
    Ok(())
}

fn validate_mods_root(path: &Path) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("cannot inspect mods directory {}: {error}", path.display()))?;
    if !metadata.is_dir() {
        return Err(format!(
            "mods directory {} is not a directory",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions and returns the current effective uid.
        let euid = unsafe { libc::geteuid() };
        if metadata.uid() != euid {
            return Err(format!(
                "mods directory {} must be owned by the current user",
                path.display()
            ));
        }
        if metadata.permissions().mode() & 0o002 != 0 {
            return Err(format!(
                "mods directory {} must not be world-writable",
                path.display()
            ));
        }
    }
    Ok(())
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
    create_dir_secure(&path)
        .map_err(|error| format!("cannot create temporary directory: {error}"))?;
    Ok(path)
}

fn stage_package_tree(
    data: &Path,
    package_root: &Path,
    package_relative: &Path,
) -> Result<StagingDir, String> {
    create_dir_all_secure(data)
        .map_err(|error| format!("cannot create {}: {error}", data.display()))?;
    validate_mods_root(data)?;
    if let Some(data_dir) = data.parent() {
        validate_data_dir(data_dir)?;
    }
    let staging = StagingDir::create(temporary_path_in(data, "install")?);
    let package_parent_relative = package_relative.parent().unwrap_or_else(|| Path::new(""));
    let staging_package_parent = staging.path().join(package_parent_relative);
    create_dir_all_secure(&staging_package_parent).map_err(|error| {
        format!(
            "cannot create {}: {error}",
            staging_package_parent.display()
        )
    })?;
    copy_tree(package_root, &staging.path().join(package_relative))?;
    set_installed_directory_mode(&staging_package_parent)?;
    Ok(staging)
}

struct StagingDir {
    path: PathBuf,
    cleanup: bool,
}

impl StagingDir {
    fn create(path: PathBuf) -> Self {
        Self {
            path,
            cleanup: true,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.cleanup = false;
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if self.cleanup {
            cleanup_tree(&self.path);
        }
    }
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
        create_dir_all_secure(target)
            .map_err(|error| format!("cannot create {}: {error}", target.display()))?;
        for entry in fs::read_dir(source)
            .map_err(|error| format!("cannot read {}: {error}", source.display()))?
        {
            let entry = entry.map_err(|error| error.to_string())?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
        set_installed_directory_mode(target)?;
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
        copy_file_with_safe_mode(source, target)?;
    } else {
        return Err(format!(
            "mod package contains unsupported file {}",
            source.display()
        ));
    }
    Ok(())
}

fn create_dir_secure(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o755).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

fn create_dir_all_secure(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o755).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

fn set_owner_writable_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        let mode = metadata.permissions().mode() & 0o7777;
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700))
            .map_err(|error| format!("cannot make {} writable: {error}", path.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn cleanup_tree(path: &Path) {
    let _ = remove_tree(path);
}

fn remove_tree(path: &Path) -> std::io::Result<()> {
    make_directories_writable(path);
    fs::remove_dir_all(path)
}

fn make_directories_writable(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return;
    }
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            make_directories_writable(&entry.path());
        }
    }
    set_owner_writable_directory(path).ok();
}

fn copy_file_with_safe_mode(source: &Path, target: &Path) -> Result<(), String> {
    let permissions = fs::metadata(source)
        .map_err(|error| format!("cannot inspect {}: {error}", source.display()))?
        .permissions();
    fs::copy(source, target)
        .map_err(|error| format!("cannot copy {}: {error}", source.display()))?;
    set_safe_installed_permissions(target, &permissions)
}

fn mask_installed_file_mode(path: &Path) -> Result<(), String> {
    let permissions = fs::metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?
        .permissions();
    set_safe_installed_permissions(path, &permissions)
}

fn set_safe_installed_permissions(target: &Path, source: &fs::Permissions) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = source.mode() & 0o755;
        fs::set_permissions(target, fs::Permissions::from_mode(mode))
            .map_err(|error| format!("cannot set safe mode for {}: {error}", target.display()))
    }
    #[cfg(not(unix))]
    {
        fs::set_permissions(target, source.clone())
            .map_err(|error| format!("cannot preserve mode for {}: {error}", target.display()))
    }
}

fn set_installed_directory_mode(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|error| {
            format!(
                "cannot set installed directory mode for {}: {error}",
                path.display()
            )
        })
    }
    #[cfg(not(unix))]
    {
        let mut permissions = fs::metadata(path)
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?
            .permissions();
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions).map_err(|error| {
            format!(
                "cannot make installed directory writable {}: {error}",
                path.display()
            )
        })
    }
}

fn ensure_safe_extension_target(path: &Path) -> Result<(), String> {
    let root = mods_dir()?;
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
    use super::{
        canonicalize_mods_root, parse_manifest, parse_repository, satisfies, stage_package_tree,
        update_all_with, validate_data_dir, validate_reference, InstallReport, Manifest,
        MOD_LIFECYCLE_API,
    };
    #[cfg(unix)]
    use super::{
        cleanup_tree, copy_tree, create_dir_all_secure, remove_tree, resolve_mods_root, test_path,
    };
    use std::path::Path;

    fn with_requires(line: &str) -> Result<super::ModSpec, String> {
        parse_manifest(&format!(
            "name = \"guest\"\nentry = \"packages/guest/init.lua\"\napi = \"remuda-lua-v1\"\n{line}\n"
        ))
    }

    #[test]
    fn vanished_data_directory_is_tolerated() {
        let path =
            std::env::temp_dir().join(format!("remuda-data-not-found-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        assert!(validate_data_dir(&path).is_ok());
    }

    #[test]
    fn mods_directory_removed_before_canonicalize_is_tolerated() {
        let path =
            std::env::temp_dir().join(format!("remuda-mods-not-found-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        assert_eq!(canonicalize_mods_root(&path).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn local_mod_validation_rejects_a_symlinked_packages_ancestor() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("remuda-mod-ancestor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let checkout = root.join("checkout");
        let outside = root.join("outside/packages/guest");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("init.lua"), "return {}\n").unwrap();
        std::fs::write(
            checkout.join("extension.toml"),
            "name = \"guest\"\nentry = \"packages/guest/init.lua\"\napi = \"remuda-lua-v1\"\n",
        )
        .unwrap();
        symlink(root.join("outside/packages"), checkout.join("packages")).unwrap();

        let error = test_path(&checkout).expect_err("symlinked package ancestor must be rejected");
        assert!(error.contains("symlink"), "{error}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn copied_lua_file_mode_is_masked_to_0755() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("remuda-mod-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let source = root.join("source/guest");
        let target = root.join("installed/guest");
        std::fs::create_dir_all(&source).unwrap();
        let init = source.join("init.lua");
        std::fs::write(&init, "return {}\n").unwrap();
        std::fs::set_permissions(&init, std::fs::Permissions::from_mode(0o4777)).unwrap();

        copy_tree(&source, &target).unwrap();
        assert_eq!(
            std::fs::metadata(target.join("init.lua"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn copied_directory_modes_are_masked_to_0755() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("remuda-mod-dirmode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let source = root.join("source/packages/guest/nested");
        let data = root.join("installed");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(source.join("init.lua"), "return {}\n").unwrap();

        let staging = stage_package_tree(
            &data,
            &root.join("source/packages/guest"),
            Path::new("packages/guest"),
        )
        .unwrap();
        for directory in [
            data.clone(),
            staging.path().to_path_buf(),
            staging.path().join("packages"),
            staging.path().join("packages/guest"),
            staging.path().join("packages/guest/nested"),
        ] {
            assert_eq!(
                std::fs::metadata(directory).unwrap().permissions().mode() & 0o7777,
                0o755
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_handles_read_only_source_directories() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("remuda-mod-readonly-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let source = root.join("source/packages/guest/sub");
        let target = root.join("target/packages/guest");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("x.lua"), "return {}\n").unwrap();
        std::fs::set_permissions(
            root.join("source/packages/guest"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        std::fs::set_permissions(source, std::fs::Permissions::from_mode(0o555)).unwrap();

        copy_tree(&root.join("source/packages/guest"), &target).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("sub/x.lua")).unwrap(),
            "return {}\n"
        );
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let _ = std::fs::set_permissions(
            root.join("source/packages/guest"),
            std::fs::Permissions::from_mode(0o755),
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_mods_root_resolves_but_world_writable_target_is_refused() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = std::env::temp_dir().join(format!("remuda-mod-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let target = root.join("real-mods");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.join("mods");
        symlink(&target, &link).unwrap();
        assert_eq!(
            resolve_mods_root(&link).unwrap(),
            std::fs::canonicalize(&target).unwrap()
        );
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );

        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert_eq!(
            resolve_mods_root(&target).unwrap(),
            std::fs::canonicalize(&target).unwrap()
        );
        assert_eq!(
            resolve_mods_root(&link).unwrap(),
            std::fs::canonicalize(&target).unwrap()
        );
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o775,
            "an existing group-writable mods root must not be chmodded"
        );

        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o777)).unwrap();
        let error = resolve_mods_root(&link).unwrap_err();
        assert!(error.contains("must not be world-writable"), "{error}");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn world_writable_remuda_parent_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("remuda-mod-parent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let remuda = root.join("remuda");
        let mods = remuda.join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&remuda, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::set_permissions(&mods, std::fs::Permissions::from_mode(0o755)).unwrap();

        let error = resolve_mods_root(&mods).unwrap_err();
        assert!(error.contains("Remuda data directory"), "{error}");
        assert!(error.contains("must not be world-writable"), "{error}");
        let _ = std::fs::set_permissions(&remuda, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn readonly_installed_tree_can_be_replaced_and_removed() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("remuda-mod-force-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let source = root.join("checkout/packages/guest/sub");
        let data = root.join("mods");
        let target = data.join("guest");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("init.lua"), "return {}\n").unwrap();
        std::fs::set_permissions(
            root.join("checkout/packages/guest"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        std::fs::set_permissions(source, std::fs::Permissions::from_mode(0o555)).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();

        let initial = stage_package_tree(
            &data,
            &root.join("checkout/packages/guest"),
            Path::new("packages/guest"),
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(initial.path().join("packages/guest"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        std::fs::rename(initial.path(), &target).unwrap();

        let replacement = stage_package_tree(
            &data,
            &root.join("checkout/packages/guest"),
            Path::new("packages/guest"),
        )
        .unwrap();
        let backup = data.join("remuda-mod-backup-test");
        std::fs::rename(&target, &backup).unwrap();
        std::fs::rename(replacement.path(), &target).unwrap();
        remove_tree(&backup).unwrap();
        assert!(!backup.exists(), "force install leaves no backup directory");
        std::fs::set_permissions(
            target.join("packages/guest"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o555)).unwrap();
        remove_tree(&target).unwrap();
        assert!(
            !target.exists(),
            "mod remove handles historical 0555 directories"
        );

        let _ = std::fs::set_permissions(
            root.join("checkout/packages/guest"),
            std::fs::Permissions::from_mode(0o755),
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn fresh_data_root_and_mods_are_created_with_safe_modes() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("remuda-mod-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data = root.join("remuda/mods");
        create_dir_all_secure(&data).unwrap();
        for directory in [root.join("remuda"), data] {
            assert_eq!(
                std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_tree_removes_read_only_checkout_directories() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("remuda-mod-cleanup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let readonly = root.join("sub");
        std::fs::create_dir_all(&readonly).unwrap();
        std::fs::write(readonly.join("x.lua"), "return {}\n").unwrap();
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o555)).unwrap();
        cleanup_tree(&root);
        assert!(!root.exists());
    }

    #[test]
    fn failed_package_copy_removes_its_staging_directory() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("remuda-mod-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data = root.join("mods");
        let package = root.join("checkout/packages/guest");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&package).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(package.join("not-lua.txt"), "not allowed\n").unwrap();

        assert!(stage_package_tree(&data, &package, Path::new("packages/guest")).is_err());
        assert_eq!(std::fs::read_dir(&data).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn requires_is_an_inline_table_of_name_to_constraint_sorted_by_name() {
        let spec =
            with_requires(r#"requires = { zeta = "*", butler = ">=0.4, <0.5" }"#).expect("spec");
        assert_eq!(
            spec.requires,
            vec![
                ("butler".to_string(), ">=0.4, <0.5".to_string()),
                ("zeta".into(), "*".into())
            ]
        );
        assert!(with_requires("").expect("no requires").requires.is_empty());
        assert!(with_requires("requires = {}")
            .expect("empty")
            .requires
            .is_empty());
        for bad in [
            r#"requires = "butler""#,
            r#"requires = { butler = ">=x" }"#,
            r#"requires = { "bad name" = "*" }"#,
            r#"requires = { butler = "*", butler = "*" }"#,
            r#"requires = { guest = "*" }"#,
            r#"requires = { butler = "~0.4" }"#,
        ] {
            assert!(with_requires(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn a_prerelease_or_build_suffix_compares_on_its_numeric_core() {
        for (version, constraint, expected) in [
            ("0.1.0-nightly.20260927234736.a3e951c", "*", true),
            ("0.1.0-nightly.20260927234736.a3e951c", "", true),
            ("0.1.0-nightly.20260927234736.a3e951c", ">=0.1, <0.2", true),
            ("0.2.0-rc.1", "<0.2", false),
            ("1.2.3+build.5", "=1.2.3", true),
            ("nightly", "*", true),
        ] {
            assert_eq!(
                satisfies(version, constraint),
                Ok(expected),
                "{version} {constraint}"
            );
        }
        assert!(
            satisfies("nightly", ">=1").is_err(),
            "a non-numeric core under a real constraint"
        );
    }

    #[test]
    fn a_version_satisfies_every_comparator_in_a_constraint() {
        for (version, constraint, expected) in [
            ("0.4.2", ">=0.4, <0.5", true),
            ("0.5.0", ">=0.4, <0.5", false),
            ("0.3.9", ">=0.4", false),
            ("1.2.0", "=1.2", true),
            ("1.0.1", ">1", true),
            ("1.0.0", ">1", false),
            ("2.0", "<=2.0.0", true),
            ("0.1.0", "*", true),
            ("0.1.0", "", true),
        ] {
            assert_eq!(
                satisfies(version, constraint),
                Ok(expected),
                "{version} {constraint}"
            );
        }
        assert!(
            satisfies("x.1", ">=0.1").is_err(),
            "a non-numeric installed version is an error"
        );
    }

    fn report(name: &str) -> InstallReport {
        InstallReport {
            manifest: Manifest {
                name: name.into(),
                version: "1".into(),
                api: "remuda-lua-v1".into(),
                entry: "init.lua".into(),
                command: None,
                lifecycle: None,
                source: "disk".into(),
                status: "installed".into(),
            },
            path: std::path::PathBuf::from(name),
            repository: "owner/repo".into(),
            reference: None,
            commit: "abc".into(),
        }
    }

    #[test]
    fn batch_update_keeps_success_before_failure_and_remaining_names() {
        let result = update_all_with(
            vec!["alpha".into(), "bravo".into(), "charlie".into()],
            |name| match name {
                "alpha" => Ok(report(name)),
                "bravo" => Err("fetch failed".into()),
                _ => panic!("charlie must not be attempted"),
            },
        );
        assert_eq!(result.updated, vec![report("alpha")]);
        assert_eq!(result.failed.unwrap().name, "bravo");
        assert_eq!(result.not_attempted, vec!["charlie"]);
    }

    #[test]
    fn parses_a_manifest_with_a_declared_command() {
        let manifest = parse_manifest(
            r#"
            name = "example"
            entry = "packages/example/init.lua"
            api = "remuda-lua-v1"
            command = "example"
            "#,
        )
        .expect("manifest");
        assert_eq!(manifest.name, "example");
        assert_eq!(manifest.version, "0.1.0");
        assert_eq!(manifest.lifecycle, None);
    }

    #[test]
    fn lifecycle_manifest_is_opt_in_and_versioned() {
        let manifest = parse_manifest(
            r#"
            name = "sample"
            entry = "packages/sample/init.lua"
            api = "remuda-lua-v1"
            lifecycle = "remuda-module-v1"
            "#,
        )
        .expect("lifecycle manifest");
        assert_eq!(manifest.lifecycle.as_deref(), Some(MOD_LIFECYCLE_API));
        assert!(parse_manifest(
            r#"name = "sample"
entry = "packages/sample/init.lua"
api = "remuda-lua-v1"
lifecycle = "remuda-module-v2""#
        )
        .is_err());
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
