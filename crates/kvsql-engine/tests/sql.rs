//! SQL statements, and the two doors meeting on the same rows.

use kvsql_engine::sim::SimStorage;
use kvsql_engine::sql::{decode_row, encode_row, key_for, Value};
use kvsql_engine::{Db, Error, Options};

fn db() -> Db {
    Db::open_with(Box::new(SimStorage::new()), Options::default()).unwrap()
}

fn i(v: i64) -> Value {
    Value::Int(v)
}
fn t(s: &str) -> Value {
    Value::Text(s.into())
}
fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    db.execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows
}

fn people(db: &Db) {
    db.execute(
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INTEGER, city TEXT);
         CREATE INDEX people_city ON people(city);
         INSERT INTO people VALUES
           (1, 'Ada', 36, 'London'), (2, 'Grace', 45, 'New York'),
           (3, 'Alan', 41, 'London'), (4, 'Edsger', NULL, 'Austin'),
           (5, 'Barbara', 29, NULL)",
        &[],
    )
    .unwrap();
}

#[test]
fn create_insert_select() {
    let db = db();
    people(&db);
    assert_eq!(
        rows(&db, "SELECT name FROM people WHERE id = 3"),
        vec![vec![t("Alan")]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT name FROM people WHERE city = 'London' ORDER BY name"
        ),
        vec![vec![t("Ada")], vec![t("Alan")]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT count(*), sum(age), min(age), max(name) FROM people"
        ),
        vec![vec![i(5), i(151), i(29), t("Grace")]]
    );
    assert_eq!(
        rows(&db, "SELECT city, count(*) AS n FROM people GROUP BY city HAVING n >= 1 ORDER BY n DESC, city"),
        vec![
            vec![t("London"), i(2)],
            vec![Value::Null, i(1)],
            vec![t("Austin"), i(1)],
            vec![t("New York"), i(1)],
        ]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT id FROM people WHERE id BETWEEN 2 AND 4 AND age IS NOT NULL ORDER BY id DESC"
        ),
        vec![vec![i(3)], vec![i(2)]]
    );
    assert_eq!(
        rows(&db, "SELECT name FROM people ORDER BY age LIMIT 2 OFFSET 1"),
        vec![vec![t("Barbara")], vec![t("Ada")]]
    );
    let r = db
        .execute(
            "SELECT name AS who, age * 2 FROM people WHERE id = ?",
            &[i(1)],
        )
        .unwrap();
    assert_eq!(r.columns, vec!["who".to_string(), "expr".to_string()]);
    assert_eq!(r.rows, vec![vec![t("Ada"), i(72)]]);
    db.verify().unwrap();
}

#[test]
fn update_delete_and_indexes() {
    let db = db();
    people(&db);
    let r = db
        .execute(
            "UPDATE people SET city = 'Paris', age = age + 1 WHERE city = 'London'",
            &[],
        )
        .unwrap();
    assert_eq!(r.changes, 2);
    assert_eq!(
        rows(&db, "SELECT count(*) FROM people WHERE city = 'London'"),
        vec![vec![i(0)]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT name, age FROM people WHERE city = 'Paris' ORDER BY id"
        ),
        vec![vec![t("Ada"), i(37)], vec![t("Alan"), i(42)]]
    );
    let r = db
        .execute("DELETE FROM people WHERE age > 40", &[])
        .unwrap();
    assert_eq!(r.changes, 2);
    assert_eq!(
        rows(&db, "SELECT id FROM people ORDER BY id"),
        vec![vec![i(1)], vec![i(4)], vec![i(5)]]
    );
    db.verify().unwrap();
    db.execute("DROP INDEX people_city; DROP TABLE people", &[])
        .unwrap();
    assert!(db.execute("SELECT * FROM people", &[]).is_err());
    assert!(db.keyspaces().unwrap().is_empty());
    db.verify().unwrap();
}

#[test]
fn constraints_and_statement_rollback() {
    let db = db();
    people(&db);
    // Duplicate key in the middle of a multi-row insert: nothing from it stays.
    let e = db.execute(
        "INSERT INTO people VALUES (10, 'X', 1, 'Rome'), (1, 'Dup', 1, 'Rome')",
        &[],
    );
    assert!(matches!(e, Err(Error::Constraint(_))), "{e:?}");
    assert_eq!(
        rows(&db, "SELECT count(*) FROM people WHERE city = 'Rome'"),
        vec![vec![i(0)]]
    );
    assert!(matches!(
        db.execute("INSERT INTO people (id, age) VALUES (11, 3)", &[]),
        Err(Error::Constraint(_))
    ));
    assert!(matches!(
        db.execute("INSERT INTO people VALUES (12, 'Y', 'old', NULL)", &[]),
        Err(Error::Constraint(_))
    ));
    // Within one transaction, a failed statement is undone and the rest goes on.
    let mut tx = db.begin_write().unwrap();
    tx.execute("INSERT INTO people VALUES (20, 'Ok', 1, 'Oslo')", &[])
        .unwrap();
    assert!(tx
        .execute("UPDATE people SET id = 2 WHERE id = 20", &[])
        .is_err());
    tx.execute("INSERT INTO people VALUES (21, 'Ok2', 2, 'Oslo')", &[])
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(
        rows(&db, "SELECT id FROM people WHERE city = 'Oslo' ORDER BY id"),
        vec![vec![i(20)], vec![i(21)]]
    );
    db.verify().unwrap();
}

#[test]
fn integer_primary_key_is_assigned_when_missing() {
    let db = db();
    db.execute(
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)",
        &[],
    )
    .unwrap();
    db.execute("INSERT INTO notes (body) VALUES ('a'), ('b')", &[])
        .unwrap();
    db.execute("INSERT INTO notes VALUES (10, 'c'), (NULL, 'd')", &[])
        .unwrap();
    assert_eq!(
        rows(&db, "SELECT id, body FROM notes ORDER BY id"),
        vec![
            vec![i(1), t("a")],
            vec![i(2), t("b")],
            vec![i(10), t("c")],
            vec![i(11), t("d")]
        ]
    );
}

#[test]
fn joins() {
    let db = db();
    people(&db);
    db.execute(
        "CREATE TABLE pets (name TEXT PRIMARY KEY, owner INTEGER);
         INSERT INTO pets VALUES ('Rex', 1), ('Tom', 1), ('Kit', 3), ('Stray', 99)",
        &[],
    )
    .unwrap();
    assert_eq!(
        rows(
            &db,
            "SELECT p.name, x.name FROM people p JOIN pets x ON x.owner = p.id ORDER BY 2"
        ),
        vec![
            vec![t("Alan"), t("Kit")],
            vec![t("Ada"), t("Rex")],
            vec![t("Ada"), t("Tom")]
        ]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT p.name, count(x.name) FROM people p LEFT JOIN pets x ON x.owner = p.id
             GROUP BY p.id ORDER BY p.id"
        ),
        vec![
            vec![t("Ada"), i(2)],
            vec![t("Grace"), i(0)],
            vec![t("Alan"), i(1)],
            vec![t("Edsger"), i(0)],
            vec![t("Barbara"), i(0)]
        ]
    );
    // Primary key lookup on the inner side, written in the other direction.
    assert_eq!(
        rows(
            &db,
            "SELECT x.name, p.name FROM pets x, people p WHERE p.id = x.owner ORDER BY 1"
        ),
        vec![
            vec![t("Kit"), t("Alan")],
            vec![t("Rex"), t("Ada")],
            vec![t("Tom"), t("Ada")]
        ]
    );
    assert!(
        db.execute("SELECT name FROM people, pets", &[]).is_err(),
        "ambiguous column"
    );
}

#[test]
fn expressions_follow_sqlite() {
    let db = db();
    let one = |sql: &str| rows(&db, sql).remove(0).remove(0);
    assert_eq!(one("SELECT 7 / 2"), i(3));
    assert_eq!(one("SELECT 7 / 2.0"), Value::Real(3.5));
    assert_eq!(one("SELECT 7 % 0"), Value::Null);
    assert_eq!(one("SELECT 1 / 0"), Value::Null);
    assert_eq!(one("SELECT NULL = NULL"), Value::Null);
    assert_eq!(one("SELECT NULL IS NULL"), i(1));
    assert_eq!(one("SELECT 1 IN (2, NULL)"), Value::Null);
    assert_eq!(one("SELECT 'ab' || 3"), t("ab3"));
    assert_eq!(one("SELECT 'Hello' LIKE 'h%O'"), i(1));
    assert_eq!(
        one("SELECT 9223372036854775807 + 1"),
        Value::Real(9.223372036854776e18)
    );
    assert_eq!(one("SELECT CASE WHEN 1 > 2 THEN 'a' ELSE 'b' END"), t("b"));
    assert_eq!(one("SELECT coalesce(NULL, NULL, 3)"), i(3));
    assert_eq!(one("SELECT substr('hello', 2, 3)"), t("ell"));
    assert_eq!(one("SELECT CAST('12abc' AS INTEGER)"), i(12));
    assert_eq!(one("SELECT typeof(1.0)"), t("real"));
    assert_eq!(one("SELECT abs(-3) + length('héllo')"), i(8));
    assert!(db.execute("SELECT nosuch(1)", &[]).is_err());
}

/// The KV door and SQL read and write the same rows, and indexes stay right
/// whichever door wrote.
#[test]
fn kv_door_and_sql_share_rows() {
    let db = db();
    people(&db);
    // KV door read of a row SQL wrote.
    assert_eq!(
        db.get_row("people", &i(2)).unwrap(),
        Some(vec![i(2), t("Grace"), i(45), t("New York")])
    );
    // The raw KV value is the encoded row, under the row's key.
    let raw = db.get("people", &key_for(&i(2)).unwrap()).unwrap().unwrap();
    assert_eq!(decode_row(&raw).unwrap()[1], t("Grace"));

    // KV door writes; SQL and the index see them.
    let mut tx = db.begin_write().unwrap();
    tx.put_row("people", &[i(6), t("Linus"), i(30), t("Helsinki")])
        .unwrap();
    tx.put_row("people", &[i(1), t("Ada"), i(36), t("Helsinki")])
        .unwrap(); // moves Ada
    tx.put(
        "people",
        &key_for(&i(7)).unwrap(),
        &encode_row(&[i(7), t("Ken"), i(50), t("Helsinki")]),
    )
    .unwrap();
    assert!(tx.delete_row("people", &i(3)).unwrap());
    tx.commit().unwrap();
    assert_eq!(
        rows(
            &db,
            "SELECT name FROM people WHERE city = 'Helsinki' ORDER BY id"
        ),
        vec![vec![t("Ada")], vec![t("Linus")], vec![t("Ken")]]
    );
    assert_eq!(
        rows(&db, "SELECT count(*) FROM people WHERE city = 'London'"),
        vec![vec![i(0)]]
    );
    db.verify().unwrap();

    // A raw write that does not fit the table is refused.
    let mut tx = db.begin_write().unwrap();
    assert!(tx
        .put("people", &key_for(&i(8)).unwrap(), b"not a row")
        .is_err());
    assert!(tx
        .put(
            "people",
            &key_for(&i(8)).unwrap(),
            &encode_row(&[i(9), t("Wrong key"), i(1), t("x")])
        )
        .is_err());
    assert!(tx
        .put_row("people", &[i(8), Value::Null, i(1), t("x")])
        .is_err());
    assert!(tx.drop_keyspace("people").is_err());
    assert!(tx.put("__kvsql_catalog", b"t:x", b"y").is_err());
    tx.commit().unwrap();
    db.verify().unwrap();
}

#[test]
fn plain_keyspaces_read_as_tables() {
    let db = db();
    db.put("sessions", b"s1", b"alice").unwrap();
    db.put("sessions", b"s2", b"bob").unwrap();
    assert_eq!(
        rows(
            &db,
            "SELECT CAST(key AS TEXT), length(value) FROM sessions ORDER BY key"
        ),
        vec![vec![t("s1"), i(5)], vec![t("s2"), i(3)]]
    );
    db.execute("INSERT INTO sessions VALUES ('s3', 'carol')", &[])
        .unwrap();
    assert_eq!(db.get("sessions", b"s3").unwrap(), Some(b"carol".to_vec()));
    db.execute("DELETE FROM sessions WHERE key = x'7331'", &[])
        .unwrap();
    assert_eq!(db.get("sessions", b"s1").unwrap(), None);
    assert!(db
        .execute("CREATE TABLE sessions (id INTEGER PRIMARY KEY)", &[])
        .is_err());
    assert!(db
        .execute("CREATE INDEX s ON sessions(value)", &[])
        .is_err());
}

#[test]
fn readers_query_snapshots() {
    let db = db();
    people(&db);
    let mut snap = db.begin_read().unwrap();
    db.execute("DELETE FROM people", &[]).unwrap();
    assert_eq!(
        snap.query("SELECT count(*) FROM people", &[]).unwrap().rows,
        vec![vec![i(5)]]
    );
    assert_eq!(rows(&db, "SELECT count(*) FROM people"), vec![vec![i(0)]]);
    assert!(snap.query("DELETE FROM people", &[]).is_err());
}

#[test]
fn errors_name_the_problem() {
    let db = db();
    let msg = |sql: &str| db.execute(sql, &[]).unwrap_err().to_string();
    assert!(msg("SELECT * FROM nope").contains("no such table: nope"));
    people(&db);
    assert!(msg("SELECT nope FROM people").contains("no such column: nope"));
    assert!(msg("CREATE TABLE x (a INTEGER)").contains("PRIMARY KEY"));
    assert!(msg("SELECT count(*) FROM people WHERE count(*) > 1").contains("aggregate"));
    assert!(msg("SELECT 1 FROM people WHERE id = ?").contains("parameters"));
}
