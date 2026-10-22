//! Recorded benchmark: KvSQL against SQLite on the same machine and the
//! same data. scripts/record-results.sh runs it and saves the output as
//! test/results/latest/bench.md.
//!
//! KVSQL_BENCH_ROWS sets the row count (default 100000).
//!
//! Both databases are real files in the temp directory. Both get an 8 MiB
//! page cache. Reads are timed warm, in a shuffled key order, and each
//! figure is the median of five rounds. Commits are timed with both sides
//! syncing to disk on every commit.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kvsql_engine::sim::Rng;
use kvsql_engine::sql::Value;
use kvsql_engine::Db;
use rusqlite::{params, Connection};

const ROUNDS: usize = 5;

fn temp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("kvsql-bench-{}-{name}", std::process::id()));
    remove_db(&p);
    p
}

fn remove_db(p: &Path) {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", p.display()));
    }
}

fn file_size(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Median over rounds of the time per operation.
fn per_op(ops: usize, mut round: impl FnMut()) -> Duration {
    let mut times: Vec<Duration> = (0..ROUNDS)
        .map(|_| {
            let t = Instant::now();
            round();
            t.elapsed() / ops as u32
        })
        .collect();
    times.sort();
    times[ROUNDS / 2]
}

fn fmt(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns < 10_000 {
        format!("{:.2} µs", ns as f64 / 1000.0)
    } else if ns < 10_000_000 {
        format!("{:.1} µs", ns as f64 / 1000.0)
    } else {
        format!("{:.2} ms", ns as f64 / 1e6)
    }
}

fn row(label: &str, d: Duration) {
    println!(
        "| {label} | {} | {:.0} |",
        fmt(d),
        1e9 / d.as_nanos().max(1) as f64
    );
}

fn name_of(i: i64) -> String {
    format!("user-{i:08}")
}

fn shuffled(n: i64, seed: u64) -> Vec<i64> {
    let mut v: Vec<i64> = (0..n).collect();
    let mut rng = Rng::new(seed);
    for i in (1..v.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        v.swap(i, j);
    }
    v
}

fn host() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|m| m.trim().to_string())
        })
        .or_else(|| {
            std::process::Command::new("sysctl")
                .args(["-n", "machdep.cpu.brand_string"])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown CPU".into());
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    format!("{os} {arch}, {cpu}, {threads} hardware threads")
}

fn main() {
    let n: i64 = std::env::var("KVSQL_BENCH_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let commits = 500usize;
    let scan_len = 1000i64;
    let order = shuffled(n, 7);

    println!("# KvSQL benchmark\n");
    println!("- Host: {}", host());
    println!(
        "- KvSQL {}, SQLite {} (bundled with rusqlite)",
        kvsql_engine::VERSION,
        rusqlite::version()
    );
    println!("- {n} rows in a table `t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER)` and {n} keys in a plain keyspace (8-byte key, 13-byte value)");
    println!("- Real files in the temp directory, 8 MiB page cache on both sides, one thread");
    println!(
        "- Reads: warm, shuffled key order, every key once per round, median of {ROUNDS} rounds"
    );
    println!("- SQLite: WAL journal with synchronous=FULL, so each commit is synced like a KvSQL commit; the plain keyspace is compared with a `WITHOUT ROWID` table `kv (k BLOB PRIMARY KEY, v BLOB)`");
    println!("- KvSQL has no prepared statements yet, so every KvSQL SQL call parses its text. SQLite is shown both with a cached prepared statement and parsing each time\n");

    // ---- KvSQL setup
    let kpath = temp("kvsql.db");
    let db = Db::open(&kpath).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER)",
        &[],
    )
    .unwrap();
    let t = Instant::now();
    {
        let mut tx = db.begin_write().unwrap();
        for i in 0..n {
            tx.put_row(
                "t",
                &[Value::Int(i), Value::Text(name_of(i)), Value::Int(i % 1000)],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let k_load_rows = t.elapsed();
    let t = Instant::now();
    {
        let mut tx = db.begin_write().unwrap();
        for i in 0..n {
            tx.put("kv", &(i as u64).to_be_bytes(), name_of(i).as_bytes())
                .unwrap();
        }
        tx.commit().unwrap();
    }
    let k_load_kv = t.elapsed();

    // ---- SQLite setup
    let spath = temp("sqlite.db");
    let conn = Connection::open(&spath).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA cache_size=-8192;
         CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER);
         CREATE TABLE kv (k BLOB PRIMARY KEY, v BLOB) WITHOUT ROWID;",
    )
    .unwrap();
    let t = Instant::now();
    {
        let tx = conn.unchecked_transaction().unwrap();
        let mut st = tx.prepare("INSERT INTO t VALUES (?, ?, ?)").unwrap();
        for i in 0..n {
            st.execute(params![i, name_of(i), i % 1000]).unwrap();
        }
        drop(st);
        tx.commit().unwrap();
    }
    let s_load_rows = t.elapsed();
    let t = Instant::now();
    {
        let tx = conn.unchecked_transaction().unwrap();
        let mut st = tx.prepare("INSERT INTO kv VALUES (?, ?)").unwrap();
        for i in 0..n {
            st.execute(params![
                (i as u64).to_be_bytes().to_vec(),
                name_of(i).into_bytes()
            ])
            .unwrap();
        }
        drop(st);
        tx.commit().unwrap();
    }
    let s_load_kv = t.elapsed();

    // ---- Point reads
    println!("## Point reads by primary key\n");
    println!("| Operation | Time per read | Reads per second |");
    println!("|---|---|---|");
    let d = per_op(order.len(), || {
        for &i in &order {
            let v = db.get("kv", &(i as u64).to_be_bytes()).unwrap();
            assert!(v.is_some());
        }
    });
    row("KvSQL `get` on a plain keyspace", d);
    let k_get_kv = d;
    let d = per_op(order.len(), || {
        for &i in &order {
            let r = db.get_row("t", &Value::Int(i)).unwrap();
            assert!(r.is_some());
        }
    });
    row("KvSQL `get_row` on the table (key door)", d);
    let k_get_row = d;
    let d = per_op(order.len(), || {
        for &i in &order {
            let r = db
                .execute("SELECT name FROM t WHERE id = ?", &[Value::Int(i)])
                .unwrap();
            assert_eq!(r.rows.len(), 1);
        }
    });
    row("KvSQL `SELECT name FROM t WHERE id = ?` (SQL door)", d);
    let k_sql = d;
    let d = per_op(order.len(), || {
        let mut st = conn.prepare_cached("SELECT v FROM kv WHERE k = ?").unwrap();
        for &i in &order {
            let v: Vec<u8> = st
                .query_row([(i as u64).to_be_bytes().to_vec()], |r| r.get(0))
                .unwrap();
            assert!(!v.is_empty());
        }
    });
    row("SQLite `SELECT v FROM kv WHERE k = ?`, prepared", d);
    let s_kv = d;
    let d = per_op(order.len(), || {
        let mut st = conn
            .prepare_cached("SELECT name FROM t WHERE id = ?")
            .unwrap();
        for &i in &order {
            let _: String = st.query_row([i], |r| r.get(0)).unwrap();
        }
    });
    row("SQLite `SELECT name FROM t WHERE id = ?`, prepared", d);
    let s_prep = d;
    let d = per_op(order.len(), || {
        for &i in &order {
            let mut st = conn.prepare("SELECT name FROM t WHERE id = ?").unwrap();
            let _: String = st.query_row([i], |r| r.get(0)).unwrap();
        }
    });
    row("SQLite same query, parsed every time", d);
    let s_parse = d;
    println!();

    // ---- Point reads from several threads at once
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(2, 8);
    println!("## Point reads from {threads} threads at once\n");
    println!("Each thread reads every key once in its own shuffled order. SQLite gets one connection per thread.\n");
    println!("| Operation | Reads per second, all threads |");
    println!("|---|---|");
    let orders: Vec<Vec<i64>> = (0..threads).map(|t| shuffled(n, 100 + t as u64)).collect();
    let total_reads = (n as usize * threads) as f64;
    let rate = |d: Duration| format!("{:.0}", total_reads / d.as_secs_f64());
    let d = per_op(1, || {
        std::thread::scope(|s| {
            for o in &orders {
                let db = &db;
                s.spawn(move || {
                    for &i in o {
                        assert!(db.get_row("t", &Value::Int(i)).unwrap().is_some());
                    }
                });
            }
        });
    });
    println!("| KvSQL `get_row` | {} |", rate(d));
    let d = per_op(1, || {
        std::thread::scope(|s| {
            for o in &orders {
                let db = &db;
                s.spawn(move || {
                    for &i in o {
                        let r = db
                            .execute("SELECT name FROM t WHERE id = ?", &[Value::Int(i)])
                            .unwrap();
                        assert_eq!(r.rows.len(), 1);
                    }
                });
            }
        });
    });
    println!("| KvSQL SQL point select | {} |", rate(d));
    let conns: Vec<std::sync::Mutex<Connection>> = (0..threads)
        .map(|_| {
            let c = Connection::open(&spath).unwrap();
            c.execute_batch("PRAGMA cache_size=-8192;").unwrap();
            std::sync::Mutex::new(c)
        })
        .collect();
    let d = per_op(1, || {
        std::thread::scope(|s| {
            for (o, c) in orders.iter().zip(&conns) {
                s.spawn(move || {
                    let c = c.lock().unwrap();
                    let mut st = c.prepare_cached("SELECT name FROM t WHERE id = ?").unwrap();
                    for &i in o {
                        let _: String = st.query_row([i], |r| r.get(0)).unwrap();
                    }
                });
            }
        });
    });
    println!("| SQLite prepared point select | {} |", rate(d));
    drop(conns);
    println!();

    // ---- Range scans
    let starts: Vec<i64> = shuffled(n - scan_len, 11).into_iter().take(200).collect();
    println!("## Range scan of {scan_len} consecutive keys\n");
    println!("| Operation | Time per scan | Scans per second |");
    println!("|---|---|---|");
    let d = per_op(starts.len(), || {
        for &s in &starts {
            let items = db
                .scan("kv", &(s as u64).to_be_bytes(), None, scan_len as usize)
                .unwrap();
            assert_eq!(items.len(), scan_len as usize);
        }
    });
    row("KvSQL `scan` on a plain keyspace", d);
    let d = per_op(starts.len(), || {
        for &s in &starts {
            let r = db
                .execute(
                    "SELECT id, name FROM t WHERE id >= ? AND id < ?",
                    &[Value::Int(s), Value::Int(s + scan_len)],
                )
                .unwrap();
            assert_eq!(r.rows.len(), scan_len as usize);
        }
    });
    row("KvSQL SQL `WHERE id >= ? AND id < ?`", d);
    let d = per_op(starts.len(), || {
        let mut st = conn
            .prepare_cached("SELECT id, name FROM t WHERE id >= ? AND id < ?")
            .unwrap();
        for &s in &starts {
            let rows: Vec<(i64, String)> = st
                .query_map([s, s + scan_len], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert_eq!(rows.len(), scan_len as usize);
        }
    });
    row("SQLite same query, prepared", d);
    println!();

    // ---- Aggregate
    println!("## Aggregate over the whole table\n");
    println!("| Operation | Time |");
    println!("|---|---|");
    let d = per_op(1, || {
        let r = db
            .execute("SELECT score, count(*) FROM t GROUP BY score", &[])
            .unwrap();
        assert_eq!(r.rows.len(), 1000);
    });
    println!(
        "| KvSQL `SELECT score, count(*) FROM t GROUP BY score` | {} |",
        fmt(d)
    );
    let d = per_op(1, || {
        let mut st = conn
            .prepare_cached("SELECT score, count(*) FROM t GROUP BY score")
            .unwrap();
        let c = st.query_map([], |_| Ok(())).unwrap().count();
        assert_eq!(c, 1000);
    });
    println!("| SQLite same query | {} |", fmt(d));
    println!();

    // ---- Durable single-row commits
    println!("## One small write per transaction, synced on every commit\n");
    println!("| Operation | Time per commit | Commits per second |");
    println!("|---|---|---|");
    let mut next = n;
    let d = per_op(commits, || {
        for _ in 0..commits {
            db.put("kv", &(next as u64).to_be_bytes(), b"fresh value!!")
                .unwrap();
            next += 1;
        }
    });
    row("KvSQL `put` and commit", d);
    let d = per_op(commits, || {
        for _ in 0..commits {
            db.execute(
                "INSERT INTO t VALUES (?, ?, ?)",
                &[Value::Int(next), Value::Text(name_of(next)), Value::Int(1)],
            )
            .unwrap();
            next += 1;
        }
    });
    row("KvSQL SQL `INSERT` and commit", d);
    let mut snext = n;
    let d = per_op(commits, || {
        let mut st = conn.prepare_cached("INSERT INTO kv VALUES (?, ?)").unwrap();
        for _ in 0..commits {
            st.execute(params![
                (snext as u64).to_be_bytes().to_vec(),
                b"fresh value!!".to_vec()
            ])
            .unwrap();
            snext += 1;
        }
    });
    row("SQLite prepared `INSERT INTO kv`, autocommit", d);
    let d = per_op(commits, || {
        let mut st = conn
            .prepare_cached("INSERT INTO t VALUES (?, ?, ?)")
            .unwrap();
        for _ in 0..commits {
            st.execute(params![snext, name_of(snext), 1]).unwrap();
            snext += 1;
        }
    });
    row("SQLite prepared `INSERT INTO t`, autocommit", d);
    println!();

    // ---- Bulk load and size
    println!("## Bulk load in one transaction, and file size\n");
    println!("| Operation | KvSQL | SQLite |");
    println!("|---|---|---|");
    println!(
        "| Load {n} table rows | {} | {} |",
        fmt(k_load_rows),
        fmt(s_load_rows)
    );
    println!(
        "| Load {n} plain keys | {} | {} |",
        fmt(k_load_kv),
        fmt(s_load_kv)
    );
    drop(db);
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    drop(conn);
    println!(
        "| File size after the writes above | {:.1} MiB | {:.1} MiB |",
        file_size(&kpath) as f64 / 1048576.0,
        file_size(&spath) as f64 / 1048576.0
    );
    println!();

    println!("## Summary\n");
    println!(
        "- KvSQL `get_row` is {:.1}x faster than the same row read through KvSQL SQL.",
        k_sql.as_nanos() as f64 / k_get_row.as_nanos() as f64
    );
    println!(
        "- KvSQL `get_row` against SQLite's prepared point select: {:.2}x the speed. KvSQL `get` against SQLite's prepared KV lookup: {:.2}x.",
        s_prep.as_nanos() as f64 / k_get_row.as_nanos() as f64,
        s_kv.as_nanos() as f64 / k_get_kv.as_nanos() as f64
    );
    println!(
        "- KvSQL SQL point select against SQLite parsing each time: {:.2}x the speed; against SQLite prepared: {:.2}x.",
        s_parse.as_nanos() as f64 / k_sql.as_nanos() as f64,
        s_prep.as_nanos() as f64 / k_sql.as_nanos() as f64
    );
    println!("- A figure above 1x means KvSQL is faster.");

    remove_db(&kpath);
    remove_db(&spath);
}
