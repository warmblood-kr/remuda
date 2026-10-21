//! The VFS name index in SQLite (#778): `vfs/index.db`, one row per path.
//!
//! It replaces `index.json`, which every write parsed and rewrote whole. A
//! write now touches only its own rows, in one transaction. Merge rules and
//! caps stay in `VfsIndex`; this file only stores rows. The wire format and
//! the digest are unchanged, since peers still see the same `VfsIndex`.
//!
//! The schema is `STEPS`, run by `sqlite_migrate`. On first open, a format-1
//! `index.json` is imported. The JSON file is left untouched (an older binary
//! reads only it); `export_json` writes it again before a downgrade. If it
//! changes later, say an older binary wrote it after a downgrade, the next
//! open merges it again: per path, the higher version wins, as between peers.
//!
//! `u64` fields are stored bit-cast to SQLite's `i64` and cast back, so every
//! value a peer can send round-trips exactly.

use super::storage;
use super::vfs::{IndexCounts, VfsError, VfsErrorKind, VfsIndex, VfsResult};
use remuda_core::vfs::VfsEntry;
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;

pub(super) const DB_FILE: &str = "index.db";
pub(super) const LEGACY_FILE: &str = "index.json";
const LEGACY_FORMAT: u32 = 1;
const COLUMNS: &str = "path, sha256, size, replicas, version, writer, deleted, mtime";

#[derive(Debug, Default, Serialize, Deserialize)]
struct LegacyFile {
    format: u32,
    entries: Vec<VfsEntry>,
}

/// Open `root/index.db`: run the schema steps, then merge `index.json` if it
/// changed. A database from before the runner (#791) is adopted at step 1.
pub(super) fn open(root: &Path) -> VfsResult<Connection> {
    let mut conn = crate::sqlite::open(root, DB_FILE)?;
    let known = STEPS.len() as i64;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > known {
        return Err(VfsError::new(
            VfsErrorKind::Corrupt,
            format!(
                "{DB_FILE} schema {version} is newer than this remuda ({known}); upgrade remuda"
            ),
        ));
    }
    crate::sqlite_migrate::upgrade(&mut conn, DB_FILE, &steps(), true)
        .map_err(|error| VfsError::new(VfsErrorKind::Corrupt, error))?;
    let tx = immediate(&mut conn)?;
    // `meta` is not a schema step on purpose: #794 builds create it the same
    // way, so it needs no version bump and a downgrade may leave it behind.
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL) WITHOUT ROWID;",
    )?;
    if may_write(&tx)? {
        merge_legacy(&tx, root)?;
    }
    tx.commit()?;
    if serving() && claimed(&conn)?.is_none() {
        claim(&mut conn)?;
    }
    Ok(conn)
}

/// This daemon's `(socket, token)`, or `None` outside a daemon.
static WRITER: std::sync::Mutex<Option<(String, String)>> = std::sync::Mutex::new(None);

/// Name this process as a writer of the index for `socket`. It may write
/// only while `meta.writer` is its own, or names another socket, or is unset.
pub(super) fn set_writer(socket: &str, token: &str) {
    *WRITER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((socket.to_owned(), token.to_owned()));
    SERVING.store(false, std::sync::atomic::Ordering::SeqCst);
}

/// Set once this daemon serves; a store first opened then is claimed.
static SERVING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(super) fn mark_serving() {
    SERVING.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn serving() -> bool {
    SERVING.load(std::sync::atomic::Ordering::SeqCst)
}

fn claimed(conn: &Connection) -> VfsResult<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = 'writer'", [], |row| {
            row.get(0)
        })
        .optional()?)
}

fn writer() -> Option<(String, String)> {
    WRITER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// False when a daemon that replaced this one on the same socket claimed.
pub(super) fn may_write(conn: &Connection) -> VfsResult<bool> {
    let Some((socket, token)) = writer() else {
        return Ok(true);
    };
    let claimed = claimed(conn)?;
    Ok(
        match claimed.as_deref().and_then(|value| value.split_once('\t')) {
            Some((claimed_socket, claimed_token)) => {
                claimed_socket != socket || claimed_token == token
            }
            None => true,
        },
    )
}

/// `immediate`, refused with `superseded` if this daemon may not write.
pub(super) fn write_tx(conn: &mut Connection) -> VfsResult<Transaction<'_>> {
    let tx = immediate(conn)?;
    if !may_write(&tx)? {
        return Err(VfsError::new(
            VfsErrorKind::Superseded,
            "this daemon was replaced by `remuda reload` and no longer writes the index; run the command again to reach the new daemon",
        ));
    }
    Ok(tx)
}

/// Record this daemon as the index writer for its socket.
pub(super) fn claim(conn: &mut Connection) -> VfsResult<()> {
    let Some((socket, token)) = writer() else {
        return Ok(());
    };
    let tx = immediate(conn)?;
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('writer', ?1)",
        [format!("{socket}\t{token}")],
    )?;
    tx.commit()?;
    Ok(())
}

/// The schema, one `(name, up, down)` per step. Never edit a shipped step;
/// add one. Step 1 has no down: dropping the index loses every name.
const STEPS: [(&str, &str, Option<&str>); 1] = [(
    "entries",
    "CREATE TABLE entries (
         path     TEXT PRIMARY KEY NOT NULL,
         sha256   TEXT NOT NULL,
         size     INTEGER NOT NULL,
         replicas INTEGER NOT NULL,
         version  INTEGER NOT NULL,
         writer   TEXT NOT NULL,
         deleted  INTEGER NOT NULL,
         mtime    INTEGER NOT NULL
     ) WITHOUT ROWID;",
    None,
)];

fn steps() -> Vec<crate::sqlite_migrate::Step> {
    STEPS
        .iter()
        .map(|(name, up, down)| crate::sqlite_migrate::Step {
            name: (*name).to_owned(),
            up: (*up).to_owned(),
            down: down.map(str::to_owned),
        })
        .collect()
}

/// A write transaction that takes SQLite's write lock up front.
pub(super) fn immediate(conn: &mut Connection) -> VfsResult<Transaction<'_>> {
    Ok(conn.transaction_with_behavior(TransactionBehavior::Immediate)?)
}

/// Merge `index.json` when its size or mtime differs from the last merge or
/// export. Entries a row already supersedes are skipped.
fn merge_legacy(tx: &Transaction<'_>, root: &Path) -> VfsResult<()> {
    let Some(stamp) = legacy_stamp(root)? else {
        return Ok(());
    };
    let seen: Option<String> = tx
        .query_row(
            "SELECT value FROM meta WHERE key = 'legacy_stamp'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if seen.as_deref() == Some(stamp.as_str()) {
        return Ok(());
    }
    for entry in read_legacy(root)?.unwrap_or_default().entries.into_values() {
        if get(tx, &entry.path)?.is_none_or(|current| entry.supersedes(&current)) {
            upsert(tx, &entry)?;
        }
    }
    record_stamp(tx, &stamp)
}

fn legacy_stamp(root: &Path) -> VfsResult<Option<String>> {
    match fs::metadata(root.join(LEGACY_FILE)) {
        Ok(meta) => {
            let mtime = meta
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            Ok(Some(format!("{}:{}", meta.len(), mtime.as_nanos())))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn record_stamp(conn: &Connection, stamp: &str) -> VfsResult<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('legacy_stamp', ?1)",
        [stamp],
    )?;
    Ok(())
}

/// The format-1 `index.json`, merged entry by entry as before; `None` if absent.
fn read_legacy(root: &Path) -> VfsResult<Option<VfsIndex>> {
    let bytes = match fs::read(root.join(LEGACY_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let file: LegacyFile = serde_json::from_slice(&bytes)
        .map_err(|error| VfsError::new(VfsErrorKind::Corrupt, format!("{LEGACY_FILE}: {error}")))?;
    if file.format != LEGACY_FORMAT {
        return Err(VfsError::new(
            VfsErrorKind::Corrupt,
            format!("{LEGACY_FILE} format {} is not supported", file.format),
        ));
    }
    let mut index = VfsIndex::default();
    for entry in file.entries {
        index.merge(entry);
    }
    Ok(Some(index))
}

/// Write `root/index.json` (format 1) from the database, for an older binary.
pub(super) fn export_json(conn: &Connection, root: &Path) -> VfsResult<usize> {
    let index = load(conn)?;
    let file = LegacyFile {
        format: LEGACY_FORMAT,
        entries: index.entries.into_values().collect(),
    };
    let bytes = serde_json::to_vec(&file).map_err(io::Error::other)?;
    storage::atomic_write(&root.join(LEGACY_FILE), &bytes)?;
    if let Some(stamp) = legacy_stamp(root)? {
        record_stamp(conn, &stamp)?;
    }
    Ok(file.entries.len())
}

pub(super) fn load(conn: &Connection) -> VfsResult<VfsIndex> {
    let mut statement = conn.prepare(&format!("SELECT {COLUMNS} FROM entries"))?;
    let mut index = VfsIndex::default();
    for entry in statement.query_map([], entry_from_row)? {
        let entry = entry?;
        index.entries.insert(entry.path.clone(), entry);
    }
    Ok(index)
}

pub(super) fn get(conn: &Connection, path: &str) -> VfsResult<Option<VfsEntry>> {
    let sql = format!("SELECT {COLUMNS} FROM entries WHERE path = ?1");
    Ok(conn.query_row(&sql, [path], entry_from_row).optional()?)
}

pub(super) fn counts(conn: &Connection) -> VfsResult<IndexCounts> {
    let (total, live): (i64, i64) = conn.query_row(
        "SELECT count(*), coalesce(sum(deleted = 0), 0) FROM entries",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(IndexCounts {
        live: usize::try_from(live).unwrap_or(usize::MAX),
        total: usize::try_from(total).unwrap_or(usize::MAX),
    })
}

/// One past the highest version, as `VfsIndex::next_version`. A negative
/// stored value is a `u64` of 2^63 or more, so it outranks every other.
pub(super) fn next_version(conn: &Connection) -> VfsResult<u64> {
    let max: Option<i64> = conn.query_row(
        "SELECT coalesce((SELECT max(version) FROM entries WHERE version < 0),
                         (SELECT max(version) FROM entries))",
        [],
        |row| row.get(0),
    )?;
    Ok(max.map_or(0, |value| value as u64).saturating_add(1))
}

/// Hashes that some live entry names.
pub(super) fn live_hashes(conn: &Connection) -> VfsResult<std::collections::HashSet<String>> {
    let mut statement = conn.prepare("SELECT DISTINCT sha256 FROM entries WHERE deleted = 0")?;
    let hashes = statement.query_map([], |row| row.get(0))?;
    Ok(hashes.collect::<Result<_, _>>()?)
}

pub(super) fn upsert(tx: &Transaction<'_>, entry: &VfsEntry) -> VfsResult<()> {
    tx.execute(
        &format!(
            "INSERT OR REPLACE INTO entries ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
        ),
        params![
            entry.path,
            entry.sha256,
            entry.size as i64,
            entry.replicas,
            entry.version as i64,
            entry.writer,
            entry.deleted,
            entry.mtime as i64,
        ],
    )?;
    Ok(())
}

fn entry_from_row(row: &Row<'_>) -> rusqlite::Result<VfsEntry> {
    Ok(VfsEntry {
        path: row.get(0)?,
        sha256: row.get(1)?,
        size: row.get::<_, i64>(2)? as u64,
        replicas: row.get(3)?,
        version: row.get::<_, i64>(4)? as u64,
        writer: row.get(5)?,
        deleted: row.get(6)?,
        mtime: row.get::<_, i64>(7)? as u64,
    })
}

impl From<rusqlite::Error> for VfsError {
    fn from(error: rusqlite::Error) -> Self {
        VfsError::new(VfsErrorKind::Io, format!("{DB_FILE}: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::super::vfs::VfsStore;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "remuda-vfsdb-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }
        fn root(&self) -> std::path::PathBuf {
            self.0.join("vfs")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry(i: usize, deleted: bool) -> VfsEntry {
        VfsEntry {
            path: format!("dir{}/file{i:05}.txt", i % 37),
            sha256: if deleted && i.is_multiple_of(2) {
                String::new()
            } else {
                format!("{:064x}", i)
            },
            size: i as u64,
            replicas: 2,
            version: 1 + i as u64,
            writer: format!("SHA256:node{}", i % 3),
            deleted,
            mtime: 1_700_000_000 + i as u64,
        }
    }

    fn write_legacy(root: &Path, entries: Vec<VfsEntry>) -> Vec<u8> {
        storage::private_directory(root).unwrap();
        let bytes = serde_json::to_vec(&LegacyFile {
            format: LEGACY_FORMAT,
            entries,
        })
        .unwrap();
        fs::write(root.join(LEGACY_FILE), &bytes).unwrap();
        bytes
    }

    #[test]
    fn a_full_format_1_index_imports_with_the_same_listing_and_digest() {
        let scratch = Scratch::new();
        let entries: Vec<VfsEntry> = (0..10_000).map(|i| entry(i, i % 4 == 0)).collect();
        let legacy = write_legacy(&scratch.root(), entries);
        let before = read_legacy(&scratch.root()).unwrap().unwrap();
        assert_eq!(before.counts().total, 10_000);

        let store = VfsStore::open_at(&scratch.root()).unwrap();
        let after = store.load_index().unwrap();
        assert_eq!(after, before);
        assert_eq!(after.digest(), before.digest());
        assert_eq!(after.list("dir5/"), before.list("dir5/"));
        assert_eq!(after.list(""), before.list(""));
        assert_eq!(after.next_version(), before.next_version());
        let conn = open(&scratch.root()).unwrap();
        assert_eq!(next_version(&conn).unwrap(), before.next_version());
        assert_eq!(counts(&conn).unwrap(), before.counts());
        assert_eq!(fs::read(scratch.root().join(LEGACY_FILE)).unwrap(), legacy);
    }

    #[test]
    fn a_json_index_written_after_a_downgrade_merges_and_older_entries_lose() {
        let scratch = Scratch::new();
        let root = scratch.root();
        write_legacy(&root, vec![entry(1, false)]);
        let store = VfsStore::open_at(&root).unwrap();
        store.put("new.txt", b"new", 2, "SHA256:me").unwrap();
        assert_eq!(store.export_legacy_index().unwrap(), 2);
        // The exported file is not merged back; a newer row is not undone.
        store.delete("new.txt", "SHA256:me").unwrap();
        assert!(store.load_index().unwrap().entries["new.txt"].deleted);
        // An older binary then writes a file after the downgrade.
        let mut after = read_legacy(&root).unwrap().unwrap();
        let mut late = entry(2, false);
        late.version = 1_000;
        after.merge(late.clone());
        write_legacy(&root, after.entries.into_values().collect());
        let index = store.load_index().unwrap();
        assert_eq!(index.entries[&late.path], late);
        assert!(index.entries["new.txt"].deleted, "a stale live entry won");
    }

    #[test]
    fn a_bad_json_index_fails_the_open_and_is_retried_later() {
        let scratch = Scratch::new();
        storage::private_directory(&scratch.root()).unwrap();
        fs::write(scratch.root().join(LEGACY_FILE), b"{not json").unwrap();
        let store = VfsStore::open_at(&scratch.root()).unwrap();
        assert_eq!(store.load_index().unwrap_err().kind, VfsErrorKind::Corrupt);
        write_legacy(&scratch.root(), vec![entry(7, false)]);
        assert_eq!(store.load_index().unwrap().entries.len(), 1);
    }

    #[test]
    fn an_index_from_before_the_runner_is_adopted_at_step_1() {
        let scratch = Scratch::new();
        let root = scratch.root();
        storage::private_directory(&root).unwrap();
        let conn = crate::sqlite::open(&root, DB_FILE).unwrap();
        conn.execute_batch(STEPS[0].1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        upsert(&tx, &entry(3, false)).unwrap();
        tx.commit().unwrap();
        drop(conn);
        let conn = open(&root).unwrap();
        assert_eq!(load(&conn).unwrap().entries.len(), 1);
        let applied = crate::sqlite_migrate::status(&conn).unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].name, "entries");
        assert!(!applied[0].reversible);
    }

    #[test]
    fn only_the_last_claimant_on_a_socket_writes_the_index() {
        let scratch = Scratch::new();
        let store = VfsStore::open_at(&scratch.root()).unwrap();
        store.put("a.txt", b"a", 1, "SHA256:me").unwrap();
        set_writer("/s.sock", "old");
        store.claim_writer().unwrap();
        store.put("b.txt", b"b", 1, "SHA256:me").unwrap();
        // A reload candidate on the same socket claims: the old one is fenced.
        set_writer("/s.sock", "new");
        store.claim_writer().unwrap();
        set_writer("/s.sock", "old");
        let error = store.put("c.txt", b"c", 1, "SHA256:me").unwrap_err();
        assert_eq!(error.kind, VfsErrorKind::Superseded);
        assert_eq!(
            store.delete("a.txt", "SHA256:me").unwrap_err().kind,
            VfsErrorKind::Superseded
        );
        assert_eq!(
            store
                .collect_garbage_older_than(std::time::Duration::ZERO)
                .unwrap(),
            0
        );
        // A daemon on another socket is not this socket's successor.
        set_writer("/other.sock", "x");
        store.put("d.txt", b"d", 1, "SHA256:me").unwrap();
        // Rollback: the old daemon claims again and writes.
        set_writer("/s.sock", "old");
        store.claim_writer().unwrap();
        store.put("e.txt", b"e", 1, "SHA256:me").unwrap();
        *WRITER.lock().unwrap() = None;
        let names: Vec<String> = store.load_index().unwrap().entries.into_keys().collect();
        assert_eq!(names, ["a.txt", "b.txt", "d.txt", "e.txt"]);
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let scratch = Scratch::new();
        drop(open(&scratch.root()).unwrap());
        let conn = crate::sqlite::open(&scratch.root(), DB_FILE).unwrap();
        conn.pragma_update(None, "user_version", STEPS.len() as i64 + 1)
            .unwrap();
        let error = open(&scratch.root()).unwrap_err();
        assert!(error.detail.contains("newer than this remuda"), "{error}");
    }

    #[test]
    fn every_u64_round_trips_and_the_highest_version_wins() {
        let scratch = Scratch::new();
        let mut conn = open(&scratch.root()).unwrap();
        let mut huge = entry(1, false);
        huge.version = u64::MAX - 1;
        huge.size = u64::MAX;
        huge.mtime = 1 << 63;
        let tx = immediate(&mut conn).unwrap();
        upsert(&tx, &entry(2, false)).unwrap();
        upsert(&tx, &huge).unwrap();
        tx.commit().unwrap();
        assert_eq!(get(&conn, &huge.path).unwrap().unwrap(), huge);
        assert_eq!(next_version(&conn).unwrap(), u64::MAX);
    }

    #[test]
    fn export_writes_a_json_index_an_older_remuda_reads() {
        let scratch = Scratch::new();
        let store = VfsStore::open_at(&scratch.root()).unwrap();
        store.put("a.txt", b"one", 2, "SHA256:me").unwrap();
        store.put("b.txt", b"two", 2, "SHA256:me").unwrap();
        store.delete("a.txt", "SHA256:me").unwrap();
        assert_eq!(store.export_legacy_index().unwrap(), 2);
        let exported = read_legacy(&scratch.root()).unwrap().unwrap();
        assert_eq!(exported, store.load_index().unwrap());
    }

    const CRASH_ROOT: &str = "REMUDA_VFSDB_CRASH_ROOT";

    /// Child half of the crash test: write until killed, one line per commit.
    #[test]
    fn crash_writer_child() {
        let Some(root) = std::env::var_os(CRASH_ROOT) else {
            return;
        };
        let store = VfsStore::open_at(Path::new(&root)).unwrap();
        for i in 0..u64::MAX {
            let path = format!("crash/{i}");
            store.put(&path, path.as_bytes(), 1, "SHA256:me").unwrap();
            println!("committed {path}");
        }
    }

    #[test]
    fn a_kill_mid_write_loses_no_committed_write() {
        use std::io::BufRead;
        let scratch = Scratch::new();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cluster::vfs_index_db::tests::crash_writer_child",
            ])
            .args(["--nocapture", "--test-threads=1"])
            .env(CRASH_ROOT, scratch.root())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut committed = Vec::new();
        for line in io::BufReader::new(child.stdout.take().unwrap()).lines() {
            if let Some(path) = line.unwrap().strip_prefix("committed ") {
                committed.push(path.to_owned());
            }
            if committed.len() == 200 {
                break;
            }
        }
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(committed.len(), 200, "the writer stopped early");
        let conn = open(&scratch.root()).unwrap();
        let check: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(check, "ok");
        let index = load(&conn).unwrap();
        for path in &committed {
            assert!(index.live(path).is_some(), "{path} was committed but lost");
        }
    }
}
