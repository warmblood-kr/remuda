use super::*;
use crate::sqlite_migrate::Step;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "remuda-dbcli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn steps(first_down: Option<&str>) -> Vec<Step> {
    vec![
        Step {
            name: "create".into(),
            up: "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT);".into(),
            down: first_down.map(Into::into),
        },
        Step {
            name: "tag".into(),
            up: "ALTER TABLE notes ADD COLUMN tag TEXT;".into(),
            down: Some("ALTER TABLE notes DROP COLUMN tag;".into()),
        },
    ]
}

fn migrated(root: &Path, first_down: Option<&str>) {
    let mut conn = crate::sqlite::open(&root.join("butler"), "notes.db").unwrap();
    sqlite_migrate::upgrade(&mut conn, "notes.db", &steps(first_down), false).unwrap();
    conn.execute("INSERT INTO notes (body, tag) VALUES ('kept', 't')", [])
        .unwrap();
}

fn db(root: &Path, args: &[&str]) -> (u8, String, String) {
    let mut out = Out::default();
    let code = serve(root, args, &mut out);
    (code, out.stdout, out.stderr)
}

fn version(root: &Path) -> u32 {
    let conn = crate::sqlite::open(&root.join("butler"), "notes.db").unwrap();
    sqlite_migrate::version(&conn).unwrap()
}

#[test]
fn status_lists_each_database_with_its_steps() {
    let scratch = Scratch::new();
    migrated(&scratch.0, None);
    let (code, stdout, _) = db(&scratch.0, &["status"]);
    assert_eq!(code, 0);
    assert!(
        stdout.starts_with("butler/notes.db  version 2, can step back to 1"),
        "{stdout}"
    );
    assert!(
        stdout.contains("     1  create  (irreversible)\n"),
        "{stdout}"
    );
    assert!(stdout.contains("     2  tag\n"), "{stdout}");
    let (_, stdout, _) = db(&scratch.0, &["status", "other"]);
    assert_eq!(stdout, "No mod databases for other.\n");
}

#[test]
fn downgrade_prints_its_plan_and_changes_nothing_without_yes() {
    let scratch = Scratch::new();
    migrated(&scratch.0, Some("DROP TABLE notes;"));
    let (code, stdout, _) = db(&scratch.0, &["downgrade", "butler", "notes", "0"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("undo steps 2 (tag), 1 (create)"),
        "{stdout}"
    );
    assert_eq!(version(&scratch.0), 2);
    assert!(!std::fs::read_dir(scratch.0.join("butler"))
        .unwrap()
        .any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".bak")));
}

#[test]
fn downgrade_with_yes_backs_up_then_steps_back() {
    let scratch = Scratch::new();
    migrated(&scratch.0, Some("DROP TABLE notes;"));
    let (code, stdout, stderr) = db(&scratch.0, &["downgrade", "butler", "notes", "1", "--yes"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("butler/notes.db is now at version 1 (was 2)."),
        "{stdout}"
    );
    assert_eq!(version(&scratch.0), 1);
    let backup = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Backed up to "))
        .and_then(|rest| rest.strip_suffix('.'))
        .map(PathBuf::from)
        .unwrap();
    let copy = rusqlite::Connection::open(&backup).unwrap();
    let tag: String = copy
        .query_row("SELECT tag FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tag, "t", "the backup holds the data the down step dropped");
}

#[test]
fn an_irreversible_step_or_a_missing_database_is_refused() {
    let scratch = Scratch::new();
    migrated(&scratch.0, None);
    let (code, _, stderr) = db(&scratch.0, &["downgrade", "butler", "notes", "0", "--yes"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("step 1 (create) is irreversible; nothing was changed"),
        "{stderr}"
    );
    assert_eq!(version(&scratch.0), 2);
    let (code, _, stderr) = db(&scratch.0, &["downgrade", "butler", "nope", "0"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("no database butler/nope.db"), "{stderr}");
    let (code, _, stderr) = db(&scratch.0, &["downgrade", "../x", "notes", "0"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("not a plain name"), "{stderr}");
}
