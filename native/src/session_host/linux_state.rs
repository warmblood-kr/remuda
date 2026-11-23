use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub(super) fn default_state_base() -> io::Result<PathBuf> {
    crate::storage::base_dir_for(crate::storage::Kind::State, false, &|name| {
        std::env::var_os(name)
    })
    .map(|base| base.join("remuda/session-hosts"))
    .map_err(io::Error::other)
}

pub(super) fn boot_state_root(root: &Path, create: bool) -> io::Result<PathBuf> {
    let directory = root.join(current_boot()?);
    let identity = pid_namespace()?;
    let marker = directory.join("pid-namespace");
    if create {
        let handle = super::create_socket_root(&directory)?;
        if super::metadata_if_present(&directory.join("inventory"))?.is_some()
            && super::metadata_if_present(&marker)?.is_none()
        {
            return Err(io::Error::other(
                "session host inventory has no PID namespace identity",
            ));
        }
        match std::os::unix::fs::symlink(&identity, &marker) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        handle.sync_all()?;
        super::super::unix::directory(root)?.sync_all()?;
    }
    match super::read_symlink(&marker)? {
        Some(value) if value == Path::new(&identity) => {}
        None if !create && super::metadata_if_present(&directory)?.is_none() => {}
        _ => {
            return Err(io::Error::other(
                "session host state belongs to a different or unknown Linux PID namespace",
            ));
        }
    }
    Ok(directory)
}

pub(super) fn owner_namespace() -> io::Result<String> {
    Ok(format!("{}:{}", current_boot()?, pid_namespace()?))
}

fn current_boot() -> io::Result<String> {
    let boot = read_proc("/proc/sys/kernel/random/boot_id")?;
    boot_name(boot.trim())
}

fn read_proc(path: &str) -> io::Result<String> {
    const LIMIT: u64 = 64 * 1024;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(io::Error::other("Linux process identity record too large"));
    }
    // Process names may contain non-UTF-8 bytes. Identity fields are ASCII.
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn boot_name(id: &str) -> io::Result<String> {
    if id.len() != 36
        || id.bytes().enumerate().any(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte != b'-'
            } else {
                !byte.is_ascii_hexdigit()
            }
        })
    {
        return Err(io::Error::other("invalid Linux boot ID"));
    }
    Ok(format!("boot-{}", id.to_ascii_lowercase()))
}

fn pid_namespace() -> io::Result<String> {
    let me = read_proc("/proc/self/status")?;
    if !is_local_proc(&me, std::process::id()) {
        return Err(io::Error::other(
            "Linux procfs does not match the current PID namespace",
        ));
    }
    let namespace = std::fs::metadata("/proc/self/ns/pid")?;
    // The namespace inode alone could be reused after its last process exits.
    let init = read_proc("/proc/1/stat")?;
    let started = init_start_time(&init)?;
    Ok(format!(
        "{}-{}-{}",
        namespace.dev(),
        namespace.ino(),
        started
    ))
}

fn is_local_proc(status: &str, pid: u32) -> bool {
    let Some(coordinates) = status.lines().find_map(|line| line.strip_prefix("NSpid:")) else {
        return false;
    };
    let mut coordinates = coordinates.split_ascii_whitespace();
    coordinates.next().and_then(|field| field.parse().ok()) == Some(pid)
        && coordinates.next().is_none()
}

fn init_start_time(stat: &str) -> io::Result<u64> {
    stat.strip_prefix("1 (")
        .and_then(|stat| stat.rsplit_once(')'))
        .and_then(|(_, fields)| fields.split_ascii_whitespace().nth(19))
        .and_then(|ticks| ticks.parse::<u64>().ok())
        .ok_or_else(|| io::Error::other("invalid Linux PID 1 start time"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_names_are_distinct_and_cannot_contain_paths() {
        let first = boot_name("12345678-1234-1234-1234-123456789ABC").unwrap();
        let second = boot_name("12345678-1234-1234-1234-123456789ABD").unwrap();
        assert_ne!(first, second);
        assert!(boot_name("../../foreign-boot").is_err());
    }

    #[test]
    fn init_start_time_handles_spaces_and_parentheses_in_comm() {
        let stat = format!("1 (weird ) name) S {}123 0", "0 ".repeat(18));
        assert_eq!(init_start_time(&stat).unwrap(), 123);
        assert!(init_start_time("7 (init) S 0").is_err());
    }

    #[test]
    fn proc_mount_from_outer_pid_namespace_is_rejected() {
        assert!(is_local_proc("Name:\tremuda\nNSpid:\t42\n", 42));
        assert!(!is_local_proc("NSpid:\t42\t42\n", 42));
        assert!(!is_local_proc("NSpid:\t42\n", 7));
    }
}
