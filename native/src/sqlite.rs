//! The daemon-owned SQLite capability (#778): the one place that opens a
//! SQLite database file, so its contents stay private to the user.
//!
//! SQLite keeps three files: the database, `-wal` and `-shm`. It creates the
//! last two itself, and nothing here would check their permissions. So `open`
//! makes all three first, private (0600 in a 0700 directory on Unix, the
//! owner-only DACL on Windows), and asks SQLite to keep `-wal`/`-shm` on close
//! (`SQLITE_FCNTL_PERSIST_WAL`), so it never deletes them and later recreates
//! them with default permissions. Temporary tables stay in memory, never in
//! the OS temp directory.
//!
//! The SQLite library is compiled in (`rusqlite` `bundled`); no system
//! libsqlite is used on any target.

use crate::cluster::storage;
use rusqlite::{Connection, OpenFlags};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a statement waits for another connection's lock before failing.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const SIDE_SUFFIXES: [&str; 3] = ["", "-wal", "-shm"];

/// Open (creating if needed) `dir/file_name` in WAL mode with private files.
/// `file_name` is a plain name such as `index.db`, never a path.
pub fn open(dir: &Path, file_name: &str) -> io::Result<Connection> {
    check_file_name(file_name)?;
    storage::private_directory(dir)?;
    let path = dir.join(file_name);
    secure_files(&path)?;
    // No SQLITE_OPEN_NOFOLLOW: it rejects a symlink anywhere in the path
    // (macOS /var, a linked home). `secure_files` refuses a linked file itself.
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(&path, flags).map_err(sql_error)?;
    configure(&conn)?;
    secure_files(&path)?;
    Ok(conn)
}

/// The database file and the two files SQLite keeps next to it.
pub fn files(path: &Path) -> [PathBuf; 3] {
    SIDE_SUFFIXES.map(|suffix| {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    })
}

fn secure_files(path: &Path) -> io::Result<()> {
    files(path)
        .iter()
        .try_for_each(|file| storage::private_file(file))
}

fn configure(conn: &Connection) -> io::Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT).map_err(sql_error)?;
    persist_wal(conn)?;
    let mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .map_err(sql_error)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(io::Error::other(format!(
            "sqlite refused WAL mode (got {mode})"
        )));
    }
    conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA temp_store=MEMORY; PRAGMA foreign_keys=ON;")
        .map_err(sql_error)
}

/// Keep `-wal` and `-shm` when the last connection closes (see module doc).
fn persist_wal(conn: &Connection) -> io::Result<()> {
    let mut on: std::ffi::c_int = 1;
    // SAFETY: the handle is live for `conn`'s lifetime; PERSIST_WAL reads and
    // writes one c_int through the pointer, which outlives the call.
    let rc = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            conn.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_PERSIST_WAL,
            (&mut on as *mut std::ffi::c_int).cast(),
        )
    };
    if rc != rusqlite::ffi::SQLITE_OK {
        return Err(io::Error::other(format!(
            "sqlite cannot keep its WAL file (code {rc})"
        )));
    }
    Ok(())
}

fn check_file_name(name: &str) -> io::Result<()> {
    let plain = !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if plain {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("database name {name:?} must be a plain file name"),
        ))
    }
}

fn sql_error(error: rusqlite::Error) -> io::Error {
    io::Error::other(format!("sqlite: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "remuda-sqlite-{tag}-{}-{nanos}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_rows(conn: &Connection) {
        conn.execute_batch("CREATE TABLE t (k TEXT PRIMARY KEY, v TEXT)")
            .unwrap();
        conn.execute("INSERT INTO t VALUES (?1, ?2)", ["a", "secret"])
            .unwrap();
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn assert_private(dir: &Path, db: &Path) {
        assert_eq!(mode(dir), 0o700, "{}", dir.display());
        for file in files(db) {
            assert_eq!(mode(&file), 0o600, "{}", file.display());
        }
    }

    #[cfg(windows)]
    fn assert_private(_dir: &Path, db: &Path) {
        for file in files(db) {
            let handle = fs::File::open(&file).unwrap();
            assert!(
                crate::cluster::windows_security::is_owner_acl_conforming(&handle).unwrap(),
                "{} lacks the owner-only DACL",
                file.display()
            );
        }
    }

    #[test]
    fn the_database_and_its_wal_and_shm_are_private_after_a_write() {
        let tmp = TempDir::new("private");
        let dir = tmp.0.join("db");
        let conn = open(&dir, "index.db").unwrap();
        write_rows(&conn);
        let db = dir.join("index.db");
        assert_private(&dir, &db);
        drop(conn);
        assert_private(&dir, &db);
    }

    #[test]
    fn wal_and_shm_are_kept_on_close_so_sqlite_never_recreates_them() {
        let tmp = TempDir::new("persist");
        let dir = tmp.0.join("db");
        write_rows(&open(&dir, "index.db").unwrap());
        for file in files(&dir.join("index.db")) {
            assert!(file.exists(), "{} was removed on close", file.display());
        }
        let conn = open(&dir, "index.db").unwrap();
        let value: String = conn
            .query_row("SELECT v FROM t WHERE k = 'a'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "secret");
        assert_private(&dir, &dir.join("index.db"));
    }

    #[test]
    fn a_second_connection_sees_committed_rows_in_wal_mode() {
        let tmp = TempDir::new("wal");
        let dir = tmp.0.join("db");
        let writer = open(&dir, "index.db").unwrap();
        write_rows(&writer);
        let reader = open(&dir, "index.db").unwrap();
        let mode: String = reader
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let count: i64 = reader
            .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_loose_database_file_is_refused_not_used() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("loose");
        let dir = tmp.0.join("db");
        storage::private_directory(&dir).unwrap();
        let db = dir.join("index.db");
        fs::write(&db, b"").unwrap();
        fs::set_permissions(&db, fs::Permissions::from_mode(0o644)).unwrap();
        let error = open(&dir, "index.db").unwrap_err();
        assert!(error.to_string().contains("loose permissions"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_wal_file_is_refused() {
        let tmp = TempDir::new("symlink");
        let dir = tmp.0.join("db");
        storage::private_directory(&dir).unwrap();
        let elsewhere = tmp.0.join("elsewhere");
        fs::write(&elsewhere, b"").unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("index.db-wal")).unwrap();
        assert!(open(&dir, "index.db").is_err());
    }

    #[cfg(unix)]
    const LOCK_PROBE: &str = "REMUDA_SQLITE_LOCK_PROBE";

    /// Child half of the lock test: is SQLite's `-shm` dead-man byte locked?
    #[cfg(unix)]
    #[test]
    fn lock_probe_child() {
        use std::os::fd::AsRawFd;
        let Some(shm) = std::env::var_os(LOCK_PROBE) else {
            return;
        };
        let file = fs::OpenOptions::new().read(true).open(shm).unwrap();
        // SAFETY: plain struct of integers; F_GETLK fills it in.
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = libc::F_WRLCK as _;
        lock.l_whence = libc::SEEK_SET as _;
        lock.l_start = 128; // UNIX_SHM_DMS in SQLite's os_unix.c
        lock.l_len = 1;
        // SAFETY: valid descriptor and a valid flock for the call's duration.
        assert_eq!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) },
            0
        );
        let held = i64::from(lock.l_type) != i64::from(libc::F_UNLCK);
        println!("dms {}", if held { "held" } else { "free" });
    }

    /// A second `open` must not drop the first connection's locks: closing
    /// any descriptor of a file drops all of a process's fcntl locks on it.
    #[cfg(unix)]
    #[test]
    fn a_second_open_keeps_the_first_connections_locks() {
        let tmp = TempDir::new("locks");
        let dir = tmp.0.join("db");
        let first = open(&dir, "index.db").unwrap();
        write_rows(&first);
        let second = open(&dir, "index.db").unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "sqlite::tests::lock_probe_child", "--nocapture"])
            .env(LOCK_PROBE, dir.join("index.db-shm"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("dms held"), "{stdout}");
        drop((first, second));
    }

    #[test]
    fn a_name_with_a_path_is_refused() {
        let tmp = TempDir::new("names");
        for name in ["", "../x.db", "a/b.db", ".hidden", "a\\b.db"] {
            let error = open(&tmp.0, name).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{name}");
        }
        assert!(!tmp.0.exists(), "a refused name created the directory");
    }
}
