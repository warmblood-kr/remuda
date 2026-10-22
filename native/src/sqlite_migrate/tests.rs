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

/// Two first opens race (#778): one read version 0, then waited for the
/// lock while the other applied every step. It must not run them again.
#[test]
fn a_run_that_waited_for_the_lock_plans_again() {
    let scratch = Scratch::new();
    let steps = notes();
    let mut holder = scratch.open();
    let tx = holder
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let waiter = {
        let mut conn = scratch.open();
        let steps = steps.clone();
        std::thread::spawn(move || upgrade(&mut conn, "t.db", &steps, false))
    };
    // Give the waiter time to read version 0 and block on the lock.
    std::thread::sleep(std::time::Duration::from_millis(300));
    create_table(&tx).unwrap();
    for (index, step) in steps.iter().enumerate() {
        tx.execute_batch(&step.up).unwrap();
        tx.execute(
            &format!("INSERT INTO {TABLE} (version, name, up_sha256, down_sql, applied_at) VALUES (?1, ?2, ?3, ?4, 0)"),
            params![index as u32 + 1, step.name, sha256_hex(&step.up), step.down],
        )
        .unwrap();
    }
    set_version(&tx, 2).unwrap();
    tx.commit().unwrap();
    assert_eq!(waiter.join().unwrap().unwrap(), Moved { from: 2, to: 2 });
    assert_eq!(tables(&holder), ["notes", "tags"]);
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
    assert_eq!(steps.steps.len(), 2);
    assert_eq!(steps.steps[0].down.as_deref(), Some("DROP TABLE a;"));
    assert_eq!(steps.steps[1].down, None);

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
    assert_eq!(load_dir(&dir).unwrap().steps.len(), 3);
    fs::write(dir.join("notes.txt"), "x").unwrap();
    assert!(load_dir(&dir).unwrap_err().contains("notes.txt"));
}

fn squash_set(kept_from: u32) -> StepSet {
    let all = [
        step(
            "create",
            "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL);",
            Some("DROP TABLE notes;"),
        ),
        step(
            "tags",
            "CREATE TABLE tags (note INTEGER NOT NULL, tag TEXT NOT NULL);",
            Some("DROP TABLE tags;"),
        ),
        step(
            "pins",
            "CREATE TABLE pins (note INTEGER NOT NULL);",
            Some("DROP TABLE pins;"),
        ),
        step(
            "links",
            "CREATE TABLE links (a INTEGER, b INTEGER);",
            Some("DROP TABLE links;"),
        ),
    ];
    let base = Base {
        version: 3,
        name: "base".into(),
        up: "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL);
             CREATE TABLE tags (note INTEGER NOT NULL, tag TEXT NOT NULL);
             CREATE TABLE pins (note INTEGER NOT NULL);"
            .into(),
    };
    StepSet {
        base: Some(base),
        first: kept_from,
        steps: all[kept_from as usize - 1..].to_vec(),
    }
}

fn recorded_versions(conn: &Connection) -> Vec<(u32, bool)> {
    status(conn)
        .unwrap()
        .iter()
        .map(|a| (a.version, a.reversible))
        .collect()
}

#[test]
fn a_new_database_runs_the_base_then_the_later_steps() {
    let scratch = Scratch::new();
    let mut conn = scratch.open();
    let moved = upgrade_set(&mut conn, "t.db", &squash_set(4), false).unwrap();
    assert_eq!(moved, Moved { from: 0, to: 4 });
    assert_eq!(tables(&conn), ["links", "notes", "pins", "tags"]);
    // The base is recorded at its number and cannot be stepped below.
    assert_eq!(recorded_versions(&conn), [(3, false), (4, true)]);
    assert_eq!(
        upgrade_set(&mut conn, "t.db", &squash_set(4), false)
            .unwrap()
            .from,
        4
    );
    downgrade(&mut conn, "t.db", 3).unwrap();
    let error = downgrade(&mut conn, "t.db", 2).unwrap_err();
    assert!(
        error.starts_with("irreversible: t.db step 3 (base)"),
        "{error}"
    );
    assert_eq!(tables(&conn), ["notes", "pins", "tags"]);

    let mut edited = squash_set(4);
    edited
        .base
        .as_mut()
        .unwrap()
        .up
        .push_str(" CREATE TABLE extra (x);");
    let error = upgrade_set(&mut conn, "t.db", &edited, false).unwrap_err();
    assert!(error.starts_with("drift: t.db step 3 (base)"), "{error}");
}

#[test]
fn an_existing_database_upgrades_past_a_squash_or_is_told_to_step_through_it() {
    let scratch = Scratch::new();
    let mut conn = scratch.open();
    let full = StepSet::plain(squash_set(1).steps);
    upgrade_set(
        &mut conn,
        "t.db",
        &StepSet {
            steps: full.steps[..3].to_vec(),
            ..full.clone()
        },
        false,
    )
    .unwrap();
    // At the base's version: the deleted steps are not checked for drift.
    assert_eq!(
        upgrade_set(&mut conn, "t.db", &squash_set(4), false).unwrap(),
        Moved { from: 3, to: 4 }
    );
    assert_eq!(recorded_versions(&conn).len(), 4);

    let scratch = Scratch::new();
    let mut conn = scratch.open();
    upgrade_set(
        &mut conn,
        "t.db",
        &StepSet {
            steps: full.steps[..1].to_vec(),
            ..full.clone()
        },
        false,
    )
    .unwrap();
    let error = upgrade_set(&mut conn, "t.db", &squash_set(4), false).unwrap_err();
    assert_eq!(
        error,
        "squashed: t.db is at version 1, but steps 1-3 were squashed into 0003_base.base.sql; open it once with an older release of the mod"
    );
    assert_eq!(tables(&conn), ["notes"]);
    // Shipping steps 2..3 next to the base lets it through.
    assert_eq!(
        upgrade_set(&mut conn, "t.db", &squash_set(2), false).unwrap(),
        Moved { from: 1, to: 4 }
    );
}

#[test]
fn the_previous_release_refuses_a_base_created_database_before_any_change() {
    let scratch = Scratch::new();
    let mut conn = scratch.open();
    upgrade_set(&mut conn, "t.db", &squash_set(4), false).unwrap();
    let before = tables(&conn);
    let error = previous_release_plan(&conn, "t.db").unwrap_err();
    assert!(
        error.contains("changed outside the migration runner"),
        "{error}"
    );
    assert_eq!(tables(&conn), before);
    assert_eq!(version(&conn).unwrap(), 4);
}

/// The record check in the previous release's `plan`, copied as released.
/// It runs before any step is read or the write lock is taken.
fn previous_release_plan(conn: &Connection, label: &str) -> Result<u32, String> {
    let from = version(conn)?;
    let recorded = status(conn)?;
    if recorded.len() != from as usize {
        return Err(format!(
            "migration: {label} has user_version {from} but {} recorded steps; it was changed outside the migration runner",
            recorded.len()
        ));
    }
    Ok(from)
}

#[test]
fn load_dir_reads_a_base_and_checks_where_steps_start() {
    let scratch = Scratch::new();
    let dir = scratch.0.join("m");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("0003_base.base.sql"), "CREATE TABLE a (x);").unwrap();
    fs::write(dir.join("0004_more.up.sql"), "CREATE TABLE b (x);").unwrap();
    let set = load_dir(&dir).unwrap();
    assert_eq!(
        (set.base.as_ref().unwrap().version, set.first, set.target()),
        (3, 4, 4)
    );
    fs::write(dir.join("0002_kept.up.sql"), "SELECT 1;").unwrap();
    assert!(load_dir(&dir).unwrap_err().contains("step 3 is missing"));
    fs::write(dir.join("0003_kept.up.sql"), "SELECT 1;").unwrap();
    assert_eq!(load_dir(&dir).unwrap().first, 2);
    fs::write(dir.join("0005_again.base.sql"), "SELECT 1;").unwrap();
    assert!(load_dir(&dir).unwrap_err().contains("at most one base"));
    fs::remove_file(dir.join("0005_again.base.sql")).unwrap();
    for file in ["0002_kept.up.sql", "0003_kept.up.sql", "0004_more.up.sql"] {
        fs::remove_file(dir.join(file)).unwrap();
    }
    assert_eq!(load_dir(&dir).unwrap().target(), 3);
    fs::write(dir.join("0005_late.up.sql"), "SELECT 1;").unwrap();
    assert!(load_dir(&dir).unwrap_err().contains("step 4 is missing"));
    fs::remove_file(dir.join("0005_late.up.sql")).unwrap();
    fs::write(dir.join("0001_first.up.sql"), "SELECT 1;").unwrap();
    assert!(load_dir(&dir).unwrap_err().contains("before the base"));
    fs::remove_file(dir.join("0003_base.base.sql")).unwrap();
    fs::write(dir.join("0001_one.base.sql"), "SELECT 1;").unwrap();
    assert!(load_dir(&dir).unwrap_err().contains("numbered 2 or more"));
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
