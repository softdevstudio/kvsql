//! The kvsql shell binary: -c commands, scripts on standard input, file
//! mode and server mode, and exit codes.

use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use kvsql_db::server::{self, Config};
use kvsql_engine::sim::SimStorage;
use kvsql_engine::{Db, Options};

fn temp_path(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("kvsql-shell-{tag}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn kvsql(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kvsql"))
        .args(args)
        .env_remove("KVSQL_URL")
        .env_remove("KVSQL_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn file_mode_commands() {
    let path = temp_path("cmd");
    let f = path.to_str().unwrap();
    let o = kvsql(
        &[
            "--file",
            f,
            "-c",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
            "-c",
            "INSERT INTO t VALUES (1, 'Ada'), (2, 'Grace')",
            "-c",
            "SELECT name FROM t ORDER BY id",
            "-c",
            ".row t 2",
            "-c",
            ".put notes n1 hello there",
            "-c",
            ".get notes n1",
            "-c",
            ".scan notes",
            "-c",
            ".keyspaces",
        ],
        "",
    );
    assert!(o.status.success(), "{}", err(&o));
    let s = out(&o);
    assert!(s.contains("2 rows changed"), "{s}");
    assert!(s.contains("name\n-----\nAda\nGrace\n(2 rows)"), "{s}");
    assert!(s.contains("2 | Grace"), "{s}");
    assert!(s.contains("n1 = hello there"), "{s}");
    assert!(s.contains("notes\nt"), "{s}");
    // A second run sees what the first one committed.
    let o = kvsql(
        &[
            "--file",
            f,
            "-c",
            "SELECT count(*) AS n FROM t",
            "-c",
            ".verify",
        ],
        "",
    );
    assert!(o.status.success());
    assert!(out(&o).contains("n\n-\n2\n(1 row)"), "{}", out(&o));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn stdin_scripts_and_failures() {
    let path = temp_path("stdin");
    let f = path.to_str().unwrap();
    let script = "\
-- a comment
CREATE TABLE t (
  id INTEGER PRIMARY KEY,
  v TEXT
);
INSERT INTO t VALUES (1, 'one');
.put kv a 1
SELECT v FROM t;
.quit
SELECT 'never';
";
    let o = kvsql(&["--file", f], script);
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    let s = out(&o);
    assert!(
        s.contains("1 row changed") && s.contains("one") && !s.contains("never"),
        "{s}"
    );

    // A failing statement makes the exit code non-zero but the rest still runs.
    let o = kvsql(&["--file", f], "SELECT nope FROM t;\nSELECT v FROM t;\n");
    assert!(!o.status.success());
    let s = out(&o);
    assert!(
        s.contains("no such column: nope") && s.contains("one"),
        "{s}"
    );

    let o = kvsql(&["--file", f, "-c", ".bogus"], "");
    assert!(!o.status.success());
    assert!(err(&o).contains("unknown command .bogus"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn server_mode_and_flags() {
    let db = Arc::new(Db::open_with(Box::new(SimStorage::new()), Options::default()).unwrap());
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    server::spawn(
        l,
        db.clone(),
        Config {
            token: Some("t0k".into()),
            ..Config::default()
        },
    );
    let o = kvsql(&["--url", &url, "-c", "SELECT 1"], "");
    assert!(!o.status.success());
    assert!(err(&o).contains("error (401)"), "{}", err(&o));

    let o = kvsql(
        &[
            "--url",
            &url,
            "--token",
            "t0k",
            "-c",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, b BLOB)",
            "-c",
            "INSERT INTO t VALUES (7, x'00ff')",
            "-c",
            ".row t 7",
            "-c",
            ".put raw k\u{1} spaced value",
            "-c",
            ".get raw k\u{1}",
            "-c",
            ".del raw k\u{1}",
            "-c",
            ".get raw k\u{1}",
            "-c",
            ".verify",
        ],
        "",
    );
    assert!(o.status.success(), "{}", err(&o));
    let s = out(&o);
    assert!(
        s.contains("7 | x'00ff'") || s.contains("7 | X'00FF'"),
        "{s}"
    );
    assert!(s.contains("spaced value\ndeleted\n(not found)"), "{s}");
    // The server's database holds what the shell wrote.
    assert!(db
        .get_row("t", &kvsql_engine::sql::Value::Int(7))
        .unwrap()
        .is_some());

    let o = kvsql(&["--version"], "");
    assert!(out(&o).starts_with("kvsql "));
    let o = kvsql(&["--wat"], "");
    assert_eq!(o.status.code(), Some(2));
    let o = kvsql(&["--url", "http://127.0.0.1:1", "-c", "SELECT 1"], "");
    assert!(!o.status.success());
    assert!(err(&o).contains("cannot reach server"), "{}", err(&o));
}

#[test]
fn kvsqld_flags() {
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_kvsqld"))
            .args(args)
            .output()
            .unwrap()
    };
    assert!(String::from_utf8_lossy(&run(&["--version"]).stdout).starts_with("kvsqld "));
    assert!(String::from_utf8_lossy(&run(&["--help"]).stdout).contains("--db PATH"));
    assert_eq!(run(&[]).status.code(), Some(2));
    assert_eq!(
        run(&["--db", "x", "--max-connections", "0"]).status.code(),
        Some(2)
    );
}
