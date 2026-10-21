use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "remuda-dblua-{}-{}",
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

/// A Lua state with `open(owner, name, opts)` bound to a scratch root.
fn lua_at(root: &Path) -> Lua {
    let lua = Lua::new();
    let root = root.to_path_buf();
    let open = lua
        .create_function(
            move |lua, (owner, name, opts): (String, String, Option<Table>)| {
                open_answer(lua, &root, &owner, &name, limits_from(opts)?)
            },
        )
        .unwrap();
    lua.globals().set("open", open).unwrap();
    lua
}

fn run(lua: &Lua, code: &str) -> String {
    lua.load(code).eval::<String>().unwrap()
}

#[test]
fn values_round_trip_and_null_is_nil() {
    let scratch = Scratch::new();
    let lua = lua_at(&scratch.0);
    let got = run(
        &lua,
        r#"
        local db = assert(open("butler", "notes"))
        assert(db:batch("CREATE TABLE t (a, b, c, d, e, f)"))
        assert(db:exec("INSERT INTO t VALUES (?, ?, ?, ?, ?, ?)", {nil, true, 42, 1.5, "hi", "\0\255"}) == 1)
        local rows = assert(db:query("SELECT * FROM t WHERE c = ?", {42}))
        local r = rows[1]
        return table.concat({#rows, tostring(r.a), r.b, math.type(r.c), r.d, r.e, #r.f}, ",")
    "#,
    );
    assert_eq!(got, "1,nil,1,integer,1.5,hi,2");
}

#[test]
fn each_mod_gets_its_own_directory_and_files() {
    let scratch = Scratch::new();
    let lua = lua_at(&scratch.0);
    run(
        &lua,
        r#"
        assert(assert(open("butler", "notes")):batch("CREATE TABLE b (x)"))
        assert(assert(open("other-mod", "notes")):batch("CREATE TABLE o (x)"))
        local rows = assert(assert(open("butler", "notes")):query("SELECT name FROM sqlite_master"))
        assert(#rows == 1 and rows[1].name == "b", "a mod saw another mod's table")
        return ""
    "#,
    );
    assert!(scratch.0.join("butler/notes.db").is_file());
    assert!(scratch.0.join("other-mod/notes.db").is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&scratch.0.join("butler")), 0o700);
        for file in crate::sqlite::files(&scratch.0.join("butler/notes.db")) {
            assert_eq!(mode(&file), 0o600, "{}", file.display());
        }
    }
}

#[test]
fn a_name_that_is_not_a_plain_identifier_raises() {
    let scratch = Scratch::new();
    let lua = lua_at(&scratch.0);
    for (owner, name) in [
        ("butler", "../x"),
        ("butler", "a/b"),
        ("butler", ""),
        ("butler", "notes.db"),
        ("../up", "x"),
        ("a/b", "x"),
    ] {
        let code = format!("return open({owner:?}, {name:?})");
        assert!(
            lua.load(&code).exec().is_err(),
            "{owner}/{name} was accepted"
        );
    }
    assert!(!scratch.0.exists() || std::fs::read_dir(&scratch.0).unwrap().next().is_none());
}

#[test]
fn a_write_past_max_bytes_fails_too_large() {
    let scratch = Scratch::new();
    let lua = lua_at(&scratch.0);
    let got = run(
        &lua,
        r#"
        local db = assert(open("m", "big", {max_bytes = 65536}))
        assert(db:batch("CREATE TABLE t (b)"))
        for _ = 1, 100 do
          local ok, err = db:exec("INSERT INTO t VALUES (randomblob(4096))")
          if not ok then return err end
        end
        return "no limit"
    "#,
    );
    assert!(got.starts_with("too_large: "), "{got}");
}

#[test]
fn a_statement_past_timeout_ms_fails_timeout() {
    let scratch = Scratch::new();
    let lua = lua_at(&scratch.0);
    let started = Instant::now();
    let got = run(
        &lua,
        r#"
        local db = assert(open("m", "slow", {timeout_ms = 50}))
        local _, err = db:query("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) SELECT count(*) AS c FROM n")
        local ok = db:query("SELECT 1 AS one")
        return tostring(err) .. "|" .. tostring(ok ~= nil)
    "#,
    );
    assert!(got.starts_with("timeout: "), "{got}");
    assert!(got.ends_with("|true"), "the handle stayed broken: {got}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_query_past_max_rows_fails_too_many_rows() {
    let scratch = Scratch::new();
    let lua = lua_at(&scratch.0);
    let got = run(
        &lua,
        r#"
        local db = assert(open("m", "rows", {max_rows = 5}))
        assert(#assert(db:query("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5) SELECT i FROM n")) == 5)
        local _, err = db:query("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 6) SELECT i FROM n")
        return err
    "#,
    );
    assert!(got.starts_with("too_many_rows: "), "{got}");
}

#[test]
fn a_handle_cannot_reach_other_files_or_undo_its_limits() {
    let scratch = Scratch::new();
    let lua = lua_at(&scratch.0);
    let other = scratch.0.join("victim.db");
    let code = format!(
        r#"
        local db = assert(open("m", "reach"))
        local out = {{}}
        for _, sql in ipairs({{
          "ATTACH DATABASE '{victim}' AS v",
          "VACUUM INTO '{victim}'",
          "PRAGMA journal_mode = DELETE",
          "PRAGMA max_page_count = 999999999",
          "PRAGMA writable_schema = ON",
        }}) do
          local ok, err = db:exec(sql)
          out[#out + 1] = ok and ("allowed " .. sql) or err:match("^(%a+):")
        end
        assert(db:exec("PRAGMA user_version = 3"))
        out[#out + 1] = db:query("PRAGMA user_version")[1].user_version
        return table.concat(out, ",")
    "#,
        victim = other.display().to_string()
    );
    let got = run(&lua, &code);
    assert_eq!(got, "denied,denied,denied,denied,denied,3");
    assert!(!other.exists(), "a handle wrote {}", other.display());
}

#[test]
fn a_closed_handle_answers_closed_and_limits_have_ceilings() {
    let scratch = Scratch::new();
    let lua = lua_at(&scratch.0);
    let got = run(
        &lua,
        r#"
        local db = assert(open("m", "c"))
        local limits = db:limits()
        db:close()
        local _, err = db:query("SELECT 1")
        return err .. "|" .. limits.max_bytes .. "|" .. limits.timeout_ms .. "|" .. limits.max_rows
    "#,
    );
    assert_eq!(
        got,
        "closed: this database handle was closed|67108864|1000|10000"
    );
    assert!(lua
        .load(r#"open("m", "c", {timeout_ms = 999999})"#)
        .exec()
        .is_err());
    assert!(lua
        .load(r#"open("m", "c", {max_rows = 0})"#)
        .exec()
        .is_err());
}
