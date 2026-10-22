//! Random SQL run on KvSQL and on SQLite (STRICT tables), answers compared.
//!
//! Most expressions keep types consistent, and a share of them compare text
//! with numbers on purpose, so SQLite's comparison affinity is checked too.
//! Everything the generator produces must give the same rows, the same
//! changes, and the same success or failure on both engines.

mod common;

use common::{env_u64, seed};
use kvsql_engine::sim::{Rng, SimStorage};
use kvsql_engine::sql::Value;
use kvsql_engine::{Db, Options};
use rusqlite::types::ValueRef;

const SCHEMA: &str = "
CREATE TABLE t1 (id INTEGER PRIMARY KEY, a INTEGER, b TEXT, c REAL, d INTEGER NOT NULL) STRICT;
CREATE TABLE t2 (k TEXT PRIMARY KEY, x INTEGER, y TEXT) STRICT;
CREATE INDEX t1_a ON t1(a);
CREATE INDEX t1_b ON t1(b);
CREATE INDEX t2_x ON t2(x);
CREATE TABLE t3 (id INTEGER PRIMARY KEY, n ANY, r REAL) STRICT;
CREATE INDEX t3_n ON t3(n);
CREATE INDEX t3_r ON t3(r);
";

/// Values for the ANY and REAL columns of t3, and for comparing across types.
const MIXED: [&str; 14] = [
    "1", "1.0", "2", "-0.0", "0.0", "0", "'1'", "' 2'", "'1.5'", "'abc'", "'1e1'", "1.5", "NULL",
    "x'01'",
];
const NUMERIC_TEXTS: [&str; 8] = [
    "'1'", "'2'", "' 3 '", "'1.5'", "'1e1'", "'-4'", "'x'", "'10'",
];

const TEXTS: [&str; 9] = ["", "a", "ab", "abc", "B", "b", "x y", "Zed", "a%b"];
const KEYS: [&str; 10] = ["k1", "k2", "k3", "kk", "m", "n", "z", "Q", "2", "10"];
const REALS: [f64; 6] = [-1.5, 0.0, 0.5, 2.0, 3.25, 100.0];
const PATTERNS: [&str; 7] = ["a%", "%b%", "_", "%", "A_", "x%y", "%z%"];

struct Gen {
    rng: Rng,
}

/// Which columns an expression may use.
#[derive(Clone, Copy)]
enum Cols {
    None,
    T1,
    T2,
    Both,
}

impl Gen {
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.rng.below(xs.len() as u64) as usize]
    }

    fn int_lit(&mut self) -> String {
        if self.rng.chance(8) {
            "NULL".into()
        } else {
            (self.rng.below(41) as i64 - 20).to_string()
        }
    }

    fn text_lit(&mut self) -> String {
        if self.rng.chance(8) {
            "NULL".into()
        } else {
            format!("'{}'", self.pick(&TEXTS))
        }
    }

    fn real_lit(&mut self) -> String {
        if self.rng.chance(8) {
            "NULL".into()
        } else {
            let r = *self.pick(&REALS);
            format!("{r:?}")
        }
    }

    fn int_col(&mut self, cols: Cols) -> Option<String> {
        let opts: &[&str] = match cols {
            Cols::None => return None,
            Cols::T1 => &["id", "a", "d", "t1.a"],
            Cols::T2 => &["x", "t2.x"],
            Cols::Both => &["t1.id", "t1.a", "t1.d", "t2.x"],
        };
        Some(self.pick(opts).to_string())
    }

    fn text_col(&mut self, cols: Cols) -> Option<String> {
        let opts: &[&str] = match cols {
            Cols::None => return None,
            Cols::T1 => &["b", "t1.b"],
            Cols::T2 => &["k", "y"],
            Cols::Both => &["t1.b", "t2.k", "t2.y"],
        };
        Some(self.pick(opts).to_string())
    }

    fn real_col(&mut self, cols: Cols) -> Option<String> {
        match cols {
            Cols::T1 => Some("c".into()),
            Cols::Both => Some("t1.c".into()),
            _ => None,
        }
    }

    fn int_expr(&mut self, cols: Cols, depth: u32) -> String {
        if depth == 0 || self.rng.chance(35) {
            return match self.int_col(cols) {
                Some(c) if self.rng.chance(70) => c,
                _ => self.int_lit(),
            };
        }
        let d = depth - 1;
        match self.rng.below(9) {
            0 => format!("({} + {})", self.int_expr(cols, d), self.int_expr(cols, d)),
            1 => format!("({} - {})", self.int_expr(cols, d), self.int_expr(cols, d)),
            2 => format!("({} * {})", self.int_expr(cols, d), self.int_expr(cols, d)),
            3 => format!("({} / {})", self.int_expr(cols, d), self.int_expr(cols, d)),
            4 => format!("({} % {})", self.int_expr(cols, d), self.int_expr(cols, d)),
            5 => format!("abs({})", self.int_expr(cols, d)),
            6 => format!("length({})", self.text_expr(cols, d)),
            7 => format!(
                "coalesce({}, {})",
                self.int_expr(cols, d),
                self.int_expr(cols, d)
            ),
            _ => format!(
                "CASE WHEN {} THEN {} ELSE {} END",
                self.bool_expr(cols, d),
                self.int_expr(cols, d),
                self.int_expr(cols, d)
            ),
        }
    }

    fn text_expr(&mut self, cols: Cols, depth: u32) -> String {
        if depth == 0 || self.rng.chance(40) {
            return match self.text_col(cols) {
                Some(c) if self.rng.chance(70) => c,
                _ => self.text_lit(),
            };
        }
        let d = depth - 1;
        match self.rng.below(6) {
            0 => format!("lower({})", self.text_expr(cols, d)),
            1 => format!("upper({})", self.text_expr(cols, d)),
            2 => format!(
                "({} || {})",
                self.text_expr(cols, d),
                self.text_expr(cols, d)
            ),
            3 => format!(
                "substr({}, {}, {})",
                self.text_expr(cols, d),
                self.int_expr(Cols::None, 0),
                self.int_expr(Cols::None, 0)
            ),
            4 => format!("trim({})", self.text_expr(cols, d)),
            _ => format!(
                "coalesce({}, {})",
                self.text_expr(cols, d),
                self.text_expr(cols, d)
            ),
        }
    }

    fn real_expr(&mut self, cols: Cols, depth: u32) -> String {
        if depth == 0 || self.rng.chance(40) {
            return match self.real_col(cols) {
                Some(c) if self.rng.chance(70) => c,
                _ => self.real_lit(),
            };
        }
        let d = depth - 1;
        match self.rng.below(3) {
            0 => format!("({} + {})", self.real_expr(cols, d), self.int_expr(cols, d)),
            1 => format!(
                "({} * {})",
                self.real_expr(cols, d),
                self.real_expr(cols, d)
            ),
            _ => format!("abs({})", self.real_expr(cols, d)),
        }
    }

    fn bool_expr(&mut self, cols: Cols, depth: u32) -> String {
        let d = depth.saturating_sub(1);
        let cmp = *self.pick(&["=", "<>", "<", "<=", ">", ">="]);
        if self.rng.chance(12) {
            // Text against numbers: comparison affinity decides.
            return match self.rng.below(4) {
                0 => format!(
                    "{} {cmp} {}",
                    self.int_expr(cols, d),
                    self.pick(&NUMERIC_TEXTS)
                ),
                1 => format!("{} {cmp} {}", self.text_expr(cols, d), self.int_lit()),
                2 => format!(
                    "{} IN ({}, {})",
                    self.int_expr(cols, d),
                    self.pick(&NUMERIC_TEXTS),
                    self.int_lit()
                ),
                _ => format!(
                    "{} BETWEEN {} AND {}",
                    self.int_expr(cols, d),
                    self.pick(&NUMERIC_TEXTS),
                    self.pick(&NUMERIC_TEXTS)
                ),
            };
        }
        match self.rng.below(if depth == 0 { 5 } else { 9 }) {
            0 => format!(
                "{} {cmp} {}",
                self.int_expr(cols, d),
                self.int_expr(cols, d)
            ),
            1 => format!(
                "{} {cmp} {}",
                self.text_expr(cols, d),
                self.text_expr(cols, d)
            ),
            2 => {
                let e = if self.rng.chance(50) {
                    self.int_expr(cols, d)
                } else {
                    self.text_expr(cols, d)
                };
                let not = if self.rng.chance(50) { "NOT " } else { "" };
                format!("{e} IS {not}NULL")
            }
            3 => {
                let e = self.int_expr(cols, d);
                let list: Vec<String> =
                    (0..1 + self.rng.below(4)).map(|_| self.int_lit()).collect();
                let not = if self.rng.chance(30) { "NOT " } else { "" };
                format!("{e} {not}IN ({})", list.join(", "))
            }
            4 => {
                let p = self.pick(&PATTERNS).to_string();
                let not = if self.rng.chance(30) { "NOT " } else { "" };
                format!("{} {not}LIKE '{p}'", self.text_expr(cols, d))
            }
            5 => format!(
                "{} BETWEEN {} AND {}",
                self.int_expr(cols, d),
                self.int_expr(cols, d),
                self.int_expr(cols, d)
            ),
            6 => format!(
                "({} {} {})",
                self.bool_expr(cols, d),
                if self.rng.chance(50) { "AND" } else { "OR" },
                self.bool_expr(cols, d)
            ),
            7 => format!("NOT ({})", self.bool_expr(cols, d)),
            _ => format!(
                "{} {cmp} {}",
                self.real_expr(cols, d),
                self.real_expr(cols, d)
            ),
        }
    }

    /// A condition the planner can turn into a key or index lookup,
    /// usually ANDed with something random.
    fn access_where(&mut self, cols: Cols, depth: u32) -> String {
        let rest = self.bool_expr(cols, depth);
        if self.rng.chance(40) {
            return rest;
        }
        let n = |g: &mut Gen| (g.rng.below(44) as i64 - 2).to_string();
        let cond = match cols {
            Cols::T2 => match self.rng.below(6) {
                4 => format!("k = {}", self.int_lit()),
                5 => format!("x = {}", self.pick(&NUMERIC_TEXTS)),
                0 => format!("k = '{}'", self.pick(&KEYS)),
                1 => format!("k >= '{}'", self.pick(&KEYS)),
                2 => format!("x = {}", self.int_lit()),
                _ => format!("k IN ('{}', '{}')", self.pick(&KEYS), self.pick(&KEYS)),
            },
            _ => match self.rng.below(12) {
                9 => format!("id = {}", self.pick(&NUMERIC_TEXTS)),
                10 => format!("id < {}", self.pick(&NUMERIC_TEXTS)),
                11 => format!("b = {}", self.int_lit()),
                0 => format!("id = {}", n(self)),
                1 => format!("id < {}", n(self)),
                2 => format!("id <= {}", n(self)),
                3 => format!("id > {}", n(self)),
                4 => format!("{} >= id", n(self)),
                5 => format!("id BETWEEN {} AND {}", n(self), n(self)),
                6 => format!("id IN ({}, {}, {})", n(self), n(self), self.int_lit()),
                7 => format!("a = {}", self.int_lit()),
                _ => format!("b = {}", self.text_lit()),
            },
        };
        if self.rng.chance(50) {
            cond
        } else {
            format!("{cond} AND {rest}")
        }
    }

    fn insert(&mut self) -> String {
        if self.rng.chance(15) {
            let rows: Vec<String> = (0..1 + self.rng.below(3))
                .map(|_| {
                    let r = *self.pick(&["1", "1.0", "-0.0", "0.0", "0", "2.5", "NULL", "'3'"]);
                    format!("({}, {}, {r})", 1 + self.rng.below(30), self.pick(&MIXED))
                })
                .collect();
            return format!("INSERT INTO t3 VALUES {}", rows.join(", "));
        }
        if self.rng.chance(60) {
            let rows: Vec<String> = (0..1 + self.rng.below(3))
                .map(|_| {
                    let id = if self.rng.chance(10) {
                        "NULL".to_string()
                    } else {
                        (1 + self.rng.below(40)).to_string()
                    };
                    let d = if self.rng.chance(3) {
                        "NULL".to_string()
                    } else {
                        (self.rng.below(41) as i64 - 20).to_string()
                    };
                    format!(
                        "({id}, {}, {}, {}, {d})",
                        self.int_lit(),
                        self.text_lit(),
                        self.real_lit()
                    )
                })
                .collect();
            format!("INSERT INTO t1 VALUES {}", rows.join(", "))
        } else {
            let rows: Vec<String> = (0..1 + self.rng.below(3))
                .map(|_| {
                    format!(
                        "('{}', {}, {})",
                        self.pick(&KEYS),
                        self.int_lit(),
                        self.text_lit()
                    )
                })
                .collect();
            format!("INSERT INTO t2 VALUES {}", rows.join(", "))
        }
    }

    fn write(&mut self) -> String {
        match self.rng.below(10) {
            0..=4 => self.insert(),
            5..=7 => {
                if self.rng.chance(70) {
                    let mut sets = vec![];
                    if self.rng.chance(60) {
                        sets.push(format!("a = {}", self.int_expr(Cols::T1, 2)));
                    }
                    if self.rng.chance(50) {
                        sets.push(format!("b = {}", self.text_expr(Cols::T1, 2)));
                    }
                    if self.rng.chance(30) {
                        sets.push(format!("c = {}", self.real_expr(Cols::T1, 1)));
                    }
                    if sets.is_empty() || self.rng.chance(20) {
                        sets.push(format!("d = {}", self.int_expr(Cols::T1, 1)));
                    }
                    format!(
                        "UPDATE t1 SET {} WHERE {}",
                        sets.join(", "),
                        self.access_where(Cols::T1, 2)
                    )
                } else {
                    format!(
                        "UPDATE t2 SET x = {}, y = {} WHERE {}",
                        self.int_expr(Cols::T2, 2),
                        self.text_expr(Cols::T2, 1),
                        self.access_where(Cols::T2, 2)
                    )
                }
            }
            _ => {
                if self.rng.chance(70) {
                    format!("DELETE FROM t1 WHERE {}", self.access_where(Cols::T1, 2))
                } else {
                    format!("DELETE FROM t2 WHERE {}", self.access_where(Cols::T2, 2))
                }
            }
        }
    }

    /// A query and whether its row order is fully determined.
    fn query(&mut self) -> (String, bool) {
        if self.rng.chance(12) {
            let col = *self.pick(&["n", "r", "id"]);
            let cmp = *self.pick(&["=", "=", "<", ">=", "<>"]);
            let q = match self.rng.below(3) {
                0 => format!(
                    "SELECT id, n, r FROM t3 WHERE {col} {cmp} {} ORDER BY id",
                    self.pick(&MIXED)
                ),
                1 => format!(
                    "SELECT id FROM t3 WHERE {col} IN ({}, {}) ORDER BY id",
                    self.pick(&MIXED),
                    self.pick(&MIXED)
                ),
                _ => "SELECT typeof(n), count(*) FROM t3 GROUP BY 1 ORDER BY 1".to_string(),
            };
            return (q, true);
        }
        if self.rng.chance(6) {
            let q = format!(
                "SELECT a % 3, count(*), sum(d) FROM t1 WHERE {} GROUP BY 1 ORDER BY 1",
                self.bool_expr(Cols::T1, 1)
            );
            return (q, true);
        }
        match self.rng.below(6) {
            0 | 1 => {
                let items: Vec<String> = (0..1 + self.rng.below(3))
                    .map(|_| match self.rng.below(3) {
                        0 => self.int_expr(Cols::T1, 2),
                        1 => self.text_expr(Cols::T1, 2),
                        _ => self.real_expr(Cols::T1, 1),
                    })
                    .collect();
                let mut q = format!(
                    "SELECT id, {} FROM t1 WHERE {}",
                    items.join(", "),
                    self.access_where(Cols::T1, 3)
                );
                let ordered = self.rng.chance(60);
                if ordered {
                    let mut key = match self.rng.below(3) {
                        0 => self.int_expr(Cols::T1, 1),
                        1 => self.text_expr(Cols::T1, 1),
                        _ => "c".into(),
                    };
                    // A bare integer would mean a column position.
                    if key.trim_start_matches('-').parse::<i64>().is_ok() {
                        key = "a".into();
                    }
                    let dir = if self.rng.chance(50) { "DESC" } else { "ASC" };
                    q += &format!(" ORDER BY {key} {dir}, id");
                    if self.rng.chance(50) {
                        q += &format!(" LIMIT {} OFFSET {}", self.rng.below(6), self.rng.below(3));
                    }
                }
                (q, ordered)
            }
            2 => {
                let (group, items) = match self.rng.below(3) {
                    0 => ("", String::new()),
                    1 => (" GROUP BY a", "a, ".to_string()),
                    _ => (" GROUP BY b", "b, ".to_string()),
                };
                let aggs = [
                    "count(*)",
                    "count(a)",
                    "sum(a)",
                    "sum(d)",
                    "min(b)",
                    "max(b)",
                    "min(c)",
                    "max(a)",
                    "avg(d)",
                    "total(c)",
                    "count(DISTINCT b)",
                ];
                let chosen: Vec<&str> = (0..1 + self.rng.below(3))
                    .map(|_| *self.pick(&aggs))
                    .collect();
                let mut q = format!(
                    "SELECT {items}{} FROM t1 WHERE {}{group}",
                    chosen.join(", "),
                    self.access_where(Cols::T1, 2)
                );
                if !group.is_empty() && self.rng.chance(40) {
                    q += " HAVING count(*) > 1";
                }
                (q, false)
            }
            3 | 4 => {
                let join = if self.rng.chance(50) {
                    "JOIN"
                } else {
                    "LEFT JOIN"
                };
                let on = match self.rng.below(3) {
                    0 => "t1.a = t2.x".to_string(),
                    1 => "t2.k = t1.b".to_string(),
                    _ => self.bool_expr(Cols::Both, 1),
                };
                let item = if self.rng.chance(50) {
                    self.int_expr(Cols::Both, 1)
                } else {
                    self.text_expr(Cols::Both, 1)
                };
                (
                    format!(
                        "SELECT t1.id, t2.k, {item} FROM t1 {join} t2 ON {on} WHERE {}",
                        self.access_where(Cols::T1, 1)
                    ),
                    false,
                )
            }
            _ => (
                format!(
                    "SELECT DISTINCT {} FROM t1 WHERE {}",
                    self.text_expr(Cols::T1, 1),
                    self.bool_expr(Cols::T1, 2)
                ),
                false,
            ),
        }
    }
}

fn from_sqlite(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Int(i),
        ValueRef::Real(r) => Value::Real(r),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Blob(b.to_vec()),
    }
}

fn sqlite_rows(conn: &rusqlite::Connection, sql: &str) -> rusqlite::Result<Vec<Vec<Value>>> {
    let mut st = conn.prepare(sql)?;
    let n = st.column_count();
    let mut out = Vec::new();
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        out.push((0..n).map(|i| from_sqlite(r.get_ref(i).unwrap())).collect());
    }
    Ok(out)
}

fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Real(x), Value::Real(y)) => (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0),
        _ => a == b,
    }
}

fn same_rows(a: &[Vec<Value>], b: &[Vec<Value>]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same_value(p, q)))
}

fn sorted(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    rows
}

#[test]
fn random_sql_matches_sqlite() {
    let seed = seed("sqlite_diff");
    let stmts = env_u64("KVSQL_SQL_STMTS", 3000);
    let mut g = Gen {
        rng: Rng::new(seed),
    };
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    let db = Db::open_with(Box::new(SimStorage::new()), Options { cache_pages: 128 }).unwrap();
    db.execute(SCHEMA, &[]).unwrap();

    let (mut queries, mut writes, mut errors, mut query_errors) = (0u64, 0u64, 0u64, 0u64);
    for n in 0..stmts {
        let write = g.rng.chance(45);
        let (sql, ordered) = if write { (g.write(), true) } else { g.query() };
        let ctx = format!("seed {seed} statement {n}: {sql}");
        if write {
            let theirs = conn.execute(&sql, []);
            let ours = db.execute(&sql, &[]);
            match (&theirs, &ours) {
                (Ok(t), Ok(o)) => assert_eq!(*t as u64, o.changes, "{ctx}: change count"),
                (Err(_), Err(_)) => errors += 1,
                _ => panic!("{ctx}\n  sqlite: {theirs:?}\n  kvsql: {ours:?}"),
            }
            writes += 1;
            for table in [
                "SELECT * FROM t1 ORDER BY id",
                "SELECT * FROM t2 ORDER BY k",
                "SELECT * FROM t3 ORDER BY id",
            ] {
                let t = sqlite_rows(&conn, table).unwrap();
                let o = db.execute(table, &[]).unwrap().rows;
                assert!(
                    same_rows(&t, &o),
                    "{ctx}: table differs\n  sqlite: {t:?}\n  kvsql: {o:?}"
                );
            }
        } else {
            let theirs = sqlite_rows(&conn, &sql);
            let ours = db.execute(&sql, &[]).map(|r| r.rows);
            match (theirs, ours) {
                (Ok(t), Ok(o)) => {
                    let (t, o) = if ordered {
                        (t, o)
                    } else {
                        (sorted(t), sorted(o))
                    };
                    assert!(same_rows(&t, &o), "{ctx}\n  sqlite: {t:?}\n  kvsql: {o:?}");
                }
                (Err(e), Err(_)) => {
                    errors += 1;
                    query_errors += 1;
                    if std::env::var("KVSQL_SHOW_ERRORS").is_ok() {
                        eprintln!("both failed: {sql}: {e}");
                    }
                }
                (t, o) => panic!("{ctx}\n  sqlite: {t:?}\n  kvsql: {o:?}"),
            }
            queries += 1;
        }
        if n % 500 == 0 {
            db.verify().unwrap_or_else(|e| panic!("{ctx}: verify: {e}"));
        }
    }
    db.verify().unwrap();
    eprintln!("sqlite_diff: {stmts} statements ({writes} writes, {queries} queries, {errors} failing on both), all answers match SQLite");
    assert!(
        query_errors * 20 < queries,
        "too many queries fail on both engines: {query_errors}"
    );
}
