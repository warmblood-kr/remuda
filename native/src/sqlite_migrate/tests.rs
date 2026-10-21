use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "remuda-migrate-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        Self(path)
    }
    fn open(&self) -> Connection {
        crate::sqlite::open(&self.0, "t.db").unwrap()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn step(name: &str, up: &str, down: Option<&str>) -> Step {
    Step {
        name: name.into(),
        up: up.into(),
        down: down.map(Into::into),
    }
}

fn notes() -> Vec<Step> {
    vec![
        step(
            "create",
            "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL);",
            Some("DROP TABLE notes;"),
        ),
        step(
            "tags",
            "CREATE TABLE tags (note INTEGER NOT NULL REFERENCES notes(id), tag TEXT NOT NULL);",
            Some("DROP TABLE tags;"),
        ),
    ]
}

fn tables(conn: &Connection) -> Vec<String> {
    let mut statement = conn
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE '\\_remuda%' ESCAPE '\\' ORDER BY name")
        .unwrap();
    let names = statement.query_map([], |row| row.get(0)).unwrap();
    names.collect::<Result<_, _>>().unwrap()
}

#[test]
fn upgrade_applies_pending_steps_once_and_records_them() {
    let scratch = Scratch::new();
    let mut conn = scratch.open();
    let steps = notes();
    assert_eq!(
        upgrade(&mut conn, "t.db", &steps[..1], false).unwrap(),
        Moved { from: 0, to: 1 }
    );
    assert_eq!(
        upgrade(&mut conn, "t.db", &steps, false).unwrap(),
        Moved { from: 1, to: 2 }
    );
    assert_eq!(
        upgrade(&mut conn, "t.db", &steps, false).unwrap(),
        Moved { from: 2, to: 2 }
    );
    assert_eq!(tables(&conn), ["notes", "tags"]);
    let applied = status(&conn).unwrap();
    assert_eq!(applied.len(), 2);
    assert_eq!(applied[1].up_sha256, sha256_hex(&steps[1].up));
    assert!(applied.iter().all(|a| a.reversible));
}

#[test]
fn older_code_meets_a_newer_schema_and_is_refused() {
    let scratch = Scratch::new();
    let mut conn = scratch.open();
    upgrade(&mut conn, "t.db", &notes(), false).unwrap();
    let error = upgrade(&mut conn, "t.db", &notes()[..1], false).unwrap_err();
    assert!(
        error.starts_with("too_new: t.db is at version 2, this code knows 1"),
        "{error}"
    );
    let error = upgrade(&mut conn, "t.db", &[], false).unwrap_err();
    assert!(error.starts_with("too_new:"), "{error}");
}

#[test]
fn an_edited_applied_step_is_drift() {
    let scratch = Scratch::new();
    let mut conn = scratch.open();
    upgrade(&mut conn, "t.db", &notes(), false).unwrap();
    let mut edited = notes();
    edited[0].up.push_str(" -- tweak");
    let error = upgrade(&mut conn, "t.db", &edited, false).unwrap_err();
    assert!(error.starts_with("drift: t.db step 1 (create)"), "{error}");
}

#[test]
fn a_failing_step_rolls_the_whole_run_back() {
    let scratch = Scratch::new();
    let mut conn = scratch.open();
    let mut steps = notes();
    steps.push(step("broken", "CREATE TABLE nope (;", None));
    let error = upgrade(&mut conn, "t.db", &steps, false).unwrap_err();
    assert!(error.contains("step 3, broken"), "{error}");
    assert_eq!(version(&conn).unwrap(), 0);
    assert!(tables(&conn).is_empty());
    assert!(status(&conn).unwrap().is_empty());
}

#[test]
fn a_foreign_key_violation_rolls_back_and_keys_stay_on() {
    let scratch = Scratch::new();
    let mut conn = scratch.open();
    let mut steps = notes();
    steps.push(step("orphan", "INSERT INTO tags VALUES (42, 'x');", None));
    let error = upgrade(&mut conn, "t.db", &steps, false).unwrap_err();
    assert!(error.starts_with("foreign_key:"), "{error}");
    assert_eq!(version(&conn).unwrap(), 0);
    let keys: bool = conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert!(keys);
}

#[test]
fn steps_may_not_attach_begin_pragma_or_touch_the_record() {
    for bad in [
        "ATTACH DATABASE 'x.db' AS x;",
        "BEGIN; CREATE TABLE a (x); COMMIT;",
        "SAVEPOINT s; RELEASE s;",
        "PRAGMA user_version = 9;",
        "PRAGMA journal_mode = DELETE;",
        "DROP TABLE _remuda_migrations;",
        "DELETE FROM _remuda_migrations;",
    ] {
        let scratch = Scratch::new();
        let mut conn = scratch.open();
        let mut steps = notes();
        steps.push(step("bad", bad, None));
        let error = upgrade(&mut conn, "t.db", &steps, false).unwrap_err();
        assert!(error.starts_with("denied:"), "{bad}: {error}");
        assert_eq!(version(&conn).unwrap(), 0, "{bad}");
    }
}

#[test]
fn a_step_may_rebuild_a_table_the_12_step_way() {
    let scratch = Scratch::new();
    let mut conn = scratch.open();
    let mut steps = notes();
    steps.push(step(
        "rebuild",
        "INSERT INTO notes VALUES (1, 'a');
         INSERT INTO tags VALUES (1, 't');
         CREATE TABLE new_notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL, pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)));
         INSERT INTO new_notes (id, body) SELECT id, body FROM notes;
         DROP TABLE notes;
         ALTER TABLE new_notes RENAME TO notes;
         PRAGMA foreign_key_check;",
        None,
    ));
    upgrade(&mut conn, "t.db", &steps, false).unwrap();
    let pinned: i64 = conn
        .query_row("SELECT pinned FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(pinned, 0);
}

#[test]
fn downgrade_uses_the_stored_down_sql_without_the_steps() {
    let scratch = Scratch::new();
    let mut conn = scratch.open();
    upgrade(&mut conn, "t.db", &notes(), false).unwrap();
    // No step list is passed: the database itself holds the downs.
    assert_eq!(
        downgrade(&mut conn, "t.db", 0).unwrap(),
        Moved { from: 2, to: 0 }
    );
    assert!(tables(&conn).is_empty());
    assert!(status(&conn).unwrap().is_empty());
    assert_eq!(version(&conn).unwrap(), 0);
    upgrade(&mut conn, "t.db", &notes(), false).unwrap();
    assert_eq!(version(&conn).unwrap(), 2);
}

#[test]
fn an_irreversible_step_blocks_the_downgrade_before_anything_runs() {
    let scratch = Scratch::new();
    let mut conn = scratch.open();
    let mut steps = notes();
    steps[0].down = None;
    upgrade(&mut conn, "t.db", &steps, false).unwrap();
    let error = downgrade(&mut conn, "t.db", 0).unwrap_err();
    assert!(
        error.starts_with("irreversible: t.db step 1 (create)"),
        "{error}"
    );
    assert_eq!(tables(&conn), ["notes", "tags"]);
    assert_eq!(
        downgrade(&mut conn, "t.db", 1).unwrap(),
        Moved { from: 2, to: 1 }
    );
    assert_eq!(tables(&conn), ["notes"]);
}

#[test]
fn a_user_version_set_outside_the_runner_is_refused_unless_adopted() {
    let scratch = Scratch::new();
    let mut conn = scratch.open();
    conn.execute_batch(
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL); PRAGMA user_version = 1;",
    )
    .unwrap();
    let error = upgrade(&mut conn, "t.db", &notes(), false).unwrap_err();
    assert!(
        error.contains("changed outside the migration runner"),
        "{error}"
    );
    assert_eq!(
        upgrade(&mut conn, "t.db", &notes(), true).unwrap(),
        Moved { from: 1, to: 2 }
    );
    assert_eq!(status(&conn).unwrap().len(), 2);
}

#[test]
fn backup_writes_a_private_copy_next_to_the_database() {
    let scratch = Scratch::new();
    let mut conn = scratch.open();
    upgrade(&mut conn, "t.db", &notes(), false).unwrap();
    conn.execute("INSERT INTO notes VALUES (1, 'kept')", [])
        .unwrap();
    let copy = backup(&conn, &scratch.0.join("t.db"), 2).unwrap();
    assert!(copy
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("t.db.v2-"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let restored = Connection::open(&copy).unwrap();
    let body: String = restored
        .query_row("SELECT body FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(body, "kept");
}

#[test]
fn load_dir_reads_pairs_and_rejects_gaps_duplicates_and_empty_files() {
    let scratch = Scratch::new();
    let dir = scratch.0.join("m");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("0001_create.up.sql"), "CREATE TABLE a (x);").unwrap();
    fs::write(dir.join("0001_create.down.sql"), "DROP TABLE a;").unwrap();
    fs::write(dir.join("0002_more.up.sql"), "CREATE TABLE b (x);").unwrap();
    let steps = load_dir(&dir).unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].down.as_deref(), Some("DROP TABLE a;"));
    assert_eq!(steps[1].down, None);

    fs::write(dir.join("0004_gap.up.sql"), "SELECT 1;").unwrap();
    assert_eq!(load_dir(&dir).unwrap_err(), "migration: step 3 is missing");
    fs::remove_file(dir.join("0004_gap.up.sql")).unwrap();
    fs::write(dir.join("02_again.up.sql"), "SELECT 1;").unwrap();
    assert_eq!(
        load_dir(&dir).unwrap_err(),
        "migration: step 2 appears twice"
    );
    fs::remove_file(dir.join("02_again.up.sql")).unwrap();
    fs::write(dir.join("0003_empty.up.sql"), "  \n").unwrap();
    assert!(load_dir(&dir).unwrap_err().contains("empty file"));
    fs::write(dir.join("0003_empty.up.sql"), "-- intentionally nothing\n").unwrap();
    assert_eq!(load_dir(&dir).unwrap().len(), 3);
    fs::write(dir.join("notes.txt"), "x").unwrap();
    assert!(load_dir(&dir).unwrap_err().contains("notes.txt"));
}

const CRASH_ROOT: &str = "REMUDA_MIGRATE_CRASH_ROOT";

/// Three steps; the last writes 8 MiB and then calls `midway()`, which the
/// crash child defines and the next run replaces with a no-op.
fn crash_steps() -> Vec<Step> {
    let mut steps = notes();
    steps.push(step(
        "big",
        "CREATE TABLE big (x BLOB NOT NULL);
         WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 128)
         INSERT INTO big SELECT randomblob(65536) FROM c;
         SELECT midway();",
        Some("DROP TABLE big;"),
    ));
    steps
}

fn define_midway(conn: &Connection, hold: bool) {
    use rusqlite::functions::FunctionFlags;
    conn.create_scalar_function("midway", 0, FunctionFlags::SQLITE_UTF8, move |_| {
        if hold {
            println!("midway");
            std::thread::sleep(std::time::Duration::from_secs(120));
        }
        Ok(0)
    })
    .unwrap();
}

/// Child half of the crash test: stop inside the last step until killed.
#[test]
fn crash_migrator_child() {
    let Some(root) = std::env::var_os(CRASH_ROOT) else {
        return;
    };
    let mut conn = crate::sqlite::open(Path::new(&root), "t.db").unwrap();
    define_midway(&conn, true);
    upgrade(&mut conn, "t.db", &crash_steps(), false).unwrap();
    println!("finished");
}

#[test]
fn a_kill_midway_leaves_the_old_schema_and_the_next_run_applies_all() {
    use std::io::BufRead;
    let scratch = Scratch::new();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "sqlite_migrate::tests::crash_migrator_child"])
        .args(["--nocapture", "--test-threads=1"])
        .env(CRASH_ROOT, &scratch.0)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let reached = lines.any(|line| line.is_ok_and(|line| line.ends_with("midway")));
    // The run's uncommitted pages are already in the WAL when it dies.
    let spilled = fs::metadata(scratch.0.join("t.db-wal")).map_or(0, |m| m.len());
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(reached, "the child never reached the middle of the run");
    assert!(
        spilled > 4 * 1024 * 1024,
        "only {spilled} WAL bytes at the kill"
    );

    let mut conn = scratch.open();
    let check: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(check, "ok");
    assert_eq!(version(&conn).unwrap(), 0);
    assert!(
        tables(&conn).is_empty(),
        "a killed run left {:?}",
        tables(&conn)
    );
    assert!(status(&conn).unwrap().is_empty());
    define_midway(&conn, false);
    assert_eq!(
        upgrade(&mut conn, "t.db", &crash_steps(), false).unwrap(),
        Moved { from: 0, to: 3 }
    );
    assert_eq!(tables(&conn), ["big", "notes", "tags"]);
}
