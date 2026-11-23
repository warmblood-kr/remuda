//! Select an existing manual GUI first, then this CLI build's managed GUI.
//! Managed entries must remain immutable for the lifetime of running windows.
use std::path::{Path, PathBuf};

pub(super) const EXECUTABLE: &str = if cfg!(windows) {
    "remuda-gui.exe"
} else {
    "remuda-gui"
};

pub(super) fn choose(exe: &Path, build: &str) -> Result<Option<PathBuf>, String> {
    let sibling = exe.with_file_name(EXECUTABLE);
    match std::fs::symlink_metadata(&sibling) {
        Ok(_) => return Ok(Some(sibling)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot inspect {}: {e}", sibling.display())),
    }
    if build.is_empty()
        || build.len() > 200
        || !build
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
    {
        return Err("invalid build identifier".into());
    }
    let parent = exe.parent().ok_or("CLI has no parent directory")?;
    let mut path = parent
        .canonicalize()
        .map_err(|e| format!("cannot resolve CLI directory: {e}"))?;
    for (component, directory) in [
        (".remuda-gui".to_string(), true),
        ("builds".to_string(), true),
        (format!("build-{build}"), true),
        (EXECUTABLE.to_string(), false),
    ] {
        path.push(component);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot inspect {}: {e}", path.display())),
        };
        let kind = metadata.file_type();
        #[cfg(windows)]
        let reparse_point = {
            use std::os::windows::fs::MetadataExt;
            metadata.file_attributes() & 0x400 != 0
        };
        #[cfg(not(windows))]
        let reparse_point = kind.is_symlink();
        if reparse_point || (directory && !kind.is_dir()) || (!directory && !kind.is_file()) {
            return Err(format!("invalid managed GUI entry: {}", path.display()));
        }
    }
    Ok(Some(path))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn matching_build_rollback_and_invalid_keys() {
        let root =
            std::env::temp_dir().join(format!("remuda-managed-select-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let exe = root.join("remuda");
        let path = |build| {
            root.join(".remuda-gui/builds")
                .join(format!("build-{build}/remuda-gui"))
        };
        for build in ["one", "two"] {
            let file = path(build);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, build).unwrap();
        }
        for build in ["one", "two", "one"] {
            assert_eq!(
                choose(&exe, build).unwrap().unwrap(),
                path(build).canonicalize().unwrap()
            );
        }
        assert!(choose(&exe, "missing").unwrap().is_none());
        for invalid in ["", "../escape", "/absolute", "white space", "é"] {
            assert!(choose(&exe, invalid).is_err());
        }
        assert!(choose(&exe, &"a".repeat(201)).is_err());
        let sibling = root.join("remuda-gui");
        symlink(root.join("not-installed"), &sibling).unwrap();
        assert_eq!(choose(&exe, "../escape").unwrap(), Some(sibling));
        std::fs::remove_dir_all(root).unwrap();
    }
}
