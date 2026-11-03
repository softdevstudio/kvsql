//! Recorded benchmark: KvSQL against its Rust neighbours (redb, fjall,
//! GlueSQL) with SQLite as the reference, on the same machine, the same data
//! and real files. scripts/record-results.sh runs it and saves the output as
//! test/results/latest/neighbours.md. README.md next to this file explains
//! the method and where the comparison is not like for like.
//!
//! KVSQL_NEIGHBOURS_ROWS sets the key and row count (default 100000) and
//! KVSQL_NEIGHBOURS_COMMITS the synced commits per round (default 200).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gluesql::core::data::Value as GValue;
use gluesql::core::executor::Payload;
use gluesql::core::prelude::Glue;
use gluesql::gluesql_redb_storage::RedbStorage;
use kvsql_engine::sim::Rng;
use kvsql_engine::sql::Value;
use kvsql_engine::Db;
use redb::{ReadableDatabase, TableDefinition};
use rusqlite::{params, Connection};

const ROUNDS: usize = 5;
const CACHE: usize = 8 << 20;
const SCAN_LEN: usize = 100;
const KV: TableDefinition<&[u8], &[u8]> = TableDefinition::new("kv");

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn workdir() -> PathBuf {
    let p = std::env::temp_dir().join(format!("kvsql-neighbours-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// File lengths and allocated bytes under a path (a file, or every file
/// below a folder). Allocated bytes differ from length for sparse or
/// preallocated files; off Unix they are reported as the length.
fn disk_size(p: &Path) -> (u64, u64) {
    let Ok(m) = std::fs::symlink_metadata(p) else {
        return (0, 0);
    };
    if m.is_file() {
        #[cfg(unix)]
        let alloc = std::os::unix::fs::MetadataExt::blocks(&m) * 512;
        #[cfg(not(unix))]
        let alloc = m.len();
        return (m.len(), alloc);
    }
    std::fs::read_dir(p)
        .map(|rd| {
            rd.flatten()
                .map(|e| disk_size(&e.path()))
                .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
        })
        .unwrap_or((0, 0))
}

fn sqlite_size(p: &Path) -> (u64, u64) {
    ["", "-wal", "-shm"]
        .iter()
        .map(|s| disk_size(Path::new(&format!("{}{s}", p.display()))))
        .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
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

fn mib(b: u64) -> String {
    format!("{:.1} MiB", b as f64 / 1048576.0)
}

fn sizes((len, alloc): (u64, u64)) -> String {
    format!("{} | {}", mib(len), mib(alloc))
}

fn key(i: usize) -> [u8; 8] {
    (i as u64).to_be_bytes()
}

fn name_of(i: usize) -> String {
    format!("user-{i:08}")
}

fn shuffled(n: usize, seed: u64) -> Vec<usize> {
    let mut v: Vec<usize> = (0..n).collect();
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

fn glue_rows(p: Vec<Payload>) -> Vec<Vec<GValue>> {
    match p.into_iter().next() {
        Some(Payload::Select { rows, .. }) => rows,
        other => panic!("GlueSQL: expected rows, got {other:?}"),
    }
}

fn main() {
    let n = env("KVSQL_NEIGHBOURS_ROWS", 100_000);
    let commits = env("KVSQL_NEIGHBOURS_COMMITS", 200);
    let order = shuffled(n, 7);
    // GlueSQL takes about a quarter of a millisecond per statement whatever
    // the table size, so its reads use the first 10000 keys of the same
    // shuffled order to keep the run short.
    let glue_order: Vec<usize> = order.iter().copied().take(10_000).collect();
    let starts: Vec<usize> = shuffled(n - SCAN_LEN, 11).into_iter().take(500).collect();
    let dir = workdir();
    let value = |i: usize| name_of(i).into_bytes();

    println!("# KvSQL and its neighbours\n");
    println!("- Host: {}", host());
    println!(
        "- KvSQL {}, redb 4.4.0, fjall 3.1.12, GlueSQL 0.20.0 on its redb storage (which uses redb 2.6), SQLite {} (bundled with rusqlite)",
        kvsql_engine::VERSION,
        rusqlite::version()
    );
    println!("- {n} keys (8-byte big-endian key, 13-byte value) and, for the SQL stores, {n} rows in `t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER)`");
    println!("- Real files in one temp folder, one thread. Cache: 8 MiB for KvSQL (2048 pages), redb, fjall (block cache) and SQLite; GlueSQL's redb storage has no cache setting and runs with redb's default");
    println!("- Reads: warm, shuffled key order, every key once per round, median of {ROUNDS} rounds. Scans: {} scans of {SCAN_LEN} consecutive keys from shuffled start keys", starts.len());
    println!("- Synced commits: {commits} per round, each one small write synced to disk before the call returns (KvSQL commit, redb Durability::Immediate, fjall insert then persist(SyncData), GlueSQL autocommit on redb, SQLite WAL with synchronous=FULL)\n");

    // ---------------------------------------------------------------- load
    // KvSQL
    let kpath = dir.join("kvsql.db");
    let kdb = Db::open(&kpath).unwrap();
    let t = Instant::now();
    {
        let mut tx = kdb.begin_write().unwrap();
        for i in 0..n {
            tx.put("kv", &key(i), &value(i)).unwrap();
        }
        tx.commit().unwrap();
    }
    let k_load_kv = t.elapsed();
    kdb.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER)",
        &[],
    )
    .unwrap();
    let t = Instant::now();
    {
        let mut tx = kdb.begin_write().unwrap();
        for i in 0..n {
            tx.put_row(
                "t",
                &[
                    Value::Int(i as i64),
                    Value::Text(name_of(i)),
                    Value::Int((i % 1000) as i64),
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let k_load_rows = t.elapsed();

    // redb
    let rpath = dir.join("redb.db");
    let rdb = redb::Database::builder()
        .set_cache_size(CACHE)
        .create(&rpath)
        .unwrap();
    let t = Instant::now();
    {
        let tx = rdb.begin_write().unwrap();
        {
            let mut table = tx.open_table(KV).unwrap();
            for i in 0..n {
                table.insert(&key(i)[..], &value(i)[..]).unwrap();
            }
        }
        tx.commit().unwrap();
    }
    let r_load = t.elapsed();

    // fjall
    let fpath = dir.join("fjall");
    let fdb = fjall::Database::builder(&fpath)
        .cache_size(CACHE as u64)
        .open()
        .unwrap();
    let fks = fdb
        .keyspace("kv", fjall::KeyspaceCreateOptions::default)
        .unwrap();
    let t = Instant::now();
    {
        let mut b = fdb.batch();
        for i in 0..n {
            b.insert(&fks, key(i), value(i));
        }
        b.commit().unwrap();
        fdb.persist(fjall::PersistMode::SyncData).unwrap();
    }
    let f_load = t.elapsed();

    // GlueSQL
    let gpath = dir.join("gluesql.redb");
    let mut glue = Glue::new(RedbStorage::new(&gpath).unwrap());
    glue.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER)")
        .unwrap();
    let t = Instant::now();
    {
        glue.execute("BEGIN").unwrap();
        for chunk in (0..n).collect::<Vec<_>>().chunks(1000) {
            let values: Vec<String> = chunk
                .iter()
                .map(|&i| format!("({i}, '{}', {})", name_of(i), i % 1000))
                .collect();
            glue.execute(format!("INSERT INTO t VALUES {}", values.join(", ")))
                .unwrap();
        }
        glue.execute("COMMIT").unwrap();
    }
    let g_load = t.elapsed();

    // SQLite
    let spath = dir.join("sqlite.db");
    let conn = Connection::open(&spath).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA cache_size=-8192;
         CREATE TABLE kv (k BLOB PRIMARY KEY, v BLOB) WITHOUT ROWID;
         CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER);",
    )
    .unwrap();
    let t = Instant::now();
    {
        let tx = conn.unchecked_transaction().unwrap();
        let mut st = tx.prepare("INSERT INTO kv VALUES (?, ?)").unwrap();
        for i in 0..n {
            st.execute(params![key(i).to_vec(), value(i)]).unwrap();
        }
        drop(st);
        tx.commit().unwrap();
    }
    let s_load_kv = t.elapsed();
    let t = Instant::now();
    {
        let tx = conn.unchecked_transaction().unwrap();
        let mut st = tx.prepare("INSERT INTO t VALUES (?, ?, ?)").unwrap();
        for i in 0..n {
            st.execute(params![i as i64, name_of(i), (i % 1000) as i64])
                .unwrap();
        }
        drop(st);
        tx.commit().unwrap();
    }
    let s_load_rows = t.elapsed();

    // ------------------------------------------------------------ key gets
    println!("## Key get, one call per read\n");
    println!("Each call reads one key through the store's plain key-value door and sees the latest commit (KvSQL `Db::get`, a redb read transaction per get, fjall `Keyspace::get`, SQLite autocommit prepared select).\n");
    println!("| Store and call | Time per read | Reads per second |");
    println!("|---|---|---|");
    let d = per_op(n, || {
        for &i in &order {
            assert!(kdb.get("kv", &key(i)).unwrap().is_some());
        }
    });
    row("KvSQL `Db::get`", d);
    let d = per_op(n, || {
        for &i in &order {
            let tx = rdb.begin_read().unwrap();
            let table = tx.open_table(KV).unwrap();
            assert!(table.get(&key(i)[..]).unwrap().is_some());
        }
    });
    row("redb `begin_read`, `open_table`, `get`", d);
    let d = per_op(n, || {
        for &i in &order {
            assert!(fks.get(key(i)).unwrap().is_some());
        }
    });
    row("fjall `Keyspace::get`", d);
    let d = per_op(n, || {
        let mut st = conn.prepare_cached("SELECT v FROM kv WHERE k = ?").unwrap();
        for &i in &order {
            let v: Vec<u8> = st.query_row([key(i).to_vec()], |r| r.get(0)).unwrap();
            assert!(!v.is_empty());
        }
    });
    row("SQLite prepared `SELECT v FROM kv WHERE k = ?`", d);
    println!();

    println!("## Key get inside one open read transaction\n");
    println!("All reads of a round go through one read transaction, so the per-call snapshot cost is left out. fjall and SQLite are not in this table: fjall's `Keyspace::get` has no cheaper form, and SQLite's is the row above.\n");
    println!("| Store and call | Time per read | Reads per second |");
    println!("|---|---|---|");
    let d = per_op(n, || {
        let mut tx = kdb.begin_read().unwrap();
        for &i in &order {
            assert!(tx.get("kv", &key(i)).unwrap().is_some());
        }
    });
    row("KvSQL `ReadTx::get`", d);
    let d = per_op(n, || {
        let tx = rdb.begin_read().unwrap();
        let table = tx.open_table(KV).unwrap();
        for &i in &order {
            assert!(table.get(&key(i)[..]).unwrap().is_some());
        }
    });
    row("redb `ReadOnlyTable::get`", d);
    println!();

    // --------------------------------------------------------------- scans
    println!("## Range scan of {SCAN_LEN} consecutive keys\n");
    println!("| Store and call | Time per scan | Scans per second |");
    println!("|---|---|---|");
    let d = per_op(starts.len(), || {
        for &s in &starts {
            let items = kdb.scan("kv", &key(s), None, SCAN_LEN).unwrap();
            assert_eq!(items.len(), SCAN_LEN);
        }
    });
    row("KvSQL `Db::scan`", d);
    let d = per_op(starts.len(), || {
        for &s in &starts {
            let tx = rdb.begin_read().unwrap();
            let table = tx.open_table(KV).unwrap();
            let lo = key(s);
            let mut c = 0;
            for item in table.range(&lo[..]..).unwrap().take(SCAN_LEN) {
                let (k, v) = item.unwrap();
                assert!(!k.value().is_empty() && !v.value().is_empty());
                c += 1;
            }
            assert_eq!(c, SCAN_LEN);
        }
    });
    row("redb `range`, read transaction per scan", d);
    let d = per_op(starts.len(), || {
        for &s in &starts {
            let mut c = 0;
            for item in fks.range(key(s)..).take(SCAN_LEN) {
                let (k, v) = item.into_inner().unwrap();
                assert!(!k.is_empty() && !v.is_empty());
                c += 1;
            }
            assert_eq!(c, SCAN_LEN);
        }
    });
    row("fjall `Keyspace::range`", d);
    let d = per_op(starts.len(), || {
        let mut st = conn
            .prepare_cached("SELECT k, v FROM kv WHERE k >= ? ORDER BY k LIMIT ?")
            .unwrap();
        for &s in &starts {
            let c = st
                .query_map(params![key(s).to_vec(), SCAN_LEN as i64], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .unwrap()
                .inspect(|r| assert!(r.is_ok()))
                .count();
            assert_eq!(c, SCAN_LEN);
        }
    });
    row("SQLite prepared `WHERE k >= ? ORDER BY k LIMIT ?`", d);
    println!();

    // ----------------------------------------------------------- SQL point
    println!("## Point select by primary key through SQL\n");
    println!("`SELECT name FROM t WHERE id = ?` with the id as a parameter. KvSQL and GlueSQL parse the text on every call (KvSQL keeps a parsed-statement cache; GlueSQL has no prepared statements). SQLite is shown prepared once and parsing every time. GlueSQL reads the first {} keys of the shuffled order each round, the others every key.\n", glue_order.len());
    println!("| Store and call | Time per select | Selects per second |");
    println!("|---|---|---|");
    let d = per_op(n, || {
        for &i in &order {
            let r = kdb
                .execute("SELECT name FROM t WHERE id = ?", &[Value::Int(i as i64)])
                .unwrap();
            assert_eq!(r.rows.len(), 1);
        }
    });
    row("KvSQL `Db::execute`", d);
    let k_sql = d;
    let d = per_op(glue_order.len(), || {
        for &i in &glue_order {
            let rows = glue_rows(
                glue.execute_with_params("SELECT name FROM t WHERE id = $1", [i as i64])
                    .unwrap(),
            );
            assert_eq!(rows.len(), 1);
        }
    });
    row("GlueSQL `Glue::execute_with_params`", d);
    let g_sql = d;
    let d = per_op(n, || {
        let mut st = conn
            .prepare_cached("SELECT name FROM t WHERE id = ?")
            .unwrap();
        for &i in &order {
            let _: String = st.query_row([i as i64], |r| r.get(0)).unwrap();
        }
    });
    row("SQLite, prepared once", d);
    let s_prep = d;
    let d = per_op(n, || {
        for &i in &order {
            let mut st = conn.prepare("SELECT name FROM t WHERE id = ?").unwrap();
            let _: String = st.query_row([i as i64], |r| r.get(0)).unwrap();
        }
    });
    row("SQLite, parsed every call", d);
    println!();
    let k_row = per_op(n, || {
        for &i in &order {
            assert!(kdb.get_row("t", &Value::Int(i as i64)).unwrap().is_some());
        }
    });
    println!(
        "The same row through KvSQL's key door (`Db::get_row`): {} per read.\n",
        fmt(k_row)
    );

    // ------------------------------------------------------ synced commits
    println!("## One small write per transaction, synced on every commit\n");
    println!("| Store and call | Time per commit | Commits per second |");
    println!("|---|---|---|");
    let mut next = n;
    let d = per_op(commits, || {
        for _ in 0..commits {
            kdb.put("kv", &key(next), b"fresh value!!").unwrap();
            next += 1;
        }
    });
    row("KvSQL `Db::put`", d);
    let mut next = n;
    let d = per_op(commits, || {
        for _ in 0..commits {
            let mut tx = rdb.begin_write().unwrap();
            tx.set_durability(redb::Durability::Immediate).unwrap();
            {
                let mut table = tx.open_table(KV).unwrap();
                table.insert(&key(next)[..], &b"fresh value!!"[..]).unwrap();
            }
            tx.commit().unwrap();
            next += 1;
        }
    });
    row("redb write transaction, `Durability::Immediate`", d);
    let mut next = n;
    let d = per_op(commits, || {
        for _ in 0..commits {
            fks.insert(key(next), b"fresh value!!").unwrap();
            fdb.persist(fjall::PersistMode::SyncData).unwrap();
            next += 1;
        }
    });
    row("fjall `insert` then `persist(SyncData)`", d);
    let mut next = n;
    let d = per_op(commits, || {
        for _ in 0..commits {
            glue.execute_with_params(
                "INSERT INTO t VALUES ($1, 'fresh value!', 1)",
                [next as i64],
            )
            .unwrap();
            next += 1;
        }
    });
    row("GlueSQL `INSERT INTO t`, autocommit", d);
    let mut next = n;
    let d = per_op(commits, || {
        let mut st = conn.prepare_cached("INSERT INTO kv VALUES (?, ?)").unwrap();
        for _ in 0..commits {
            st.execute(params![key(next).to_vec(), b"fresh value!!".to_vec()])
                .unwrap();
            next += 1;
        }
    });
    row("SQLite prepared `INSERT INTO kv`, autocommit", d);
    println!();

    // ------------------------------------------------------- load and size
    drop(kdb);
    drop(rdb);
    fdb.persist(fjall::PersistMode::SyncAll).unwrap();
    drop(fks);
    drop(fdb);
    drop(glue);
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    drop(conn);
    println!("## Bulk load in one transaction, and size on disk\n");
    println!("Size is measured after every write above, with each store closed (fjall: its whole folder; SQLite: the database file and its WAL). File length is what a directory listing shows; allocated is what the file system holds for it (fjall preallocates its journal to 64 MiB as a sparse file).\n");
    println!("| Store | Load {n} keys | Load {n} table rows | File length | Allocated |");
    println!("|---|---|---|---|---|");
    println!(
        "| KvSQL | {} | {} | {} |",
        fmt(k_load_kv),
        fmt(k_load_rows),
        sizes(disk_size(&kpath))
    );
    println!(
        "| redb | {} | | {} |",
        fmt(r_load),
        sizes(disk_size(&rpath))
    );
    println!(
        "| fjall | {} (one batch, then a sync) | | {} |",
        fmt(f_load),
        sizes(disk_size(&fpath))
    );
    println!(
        "| GlueSQL | | {} (INSERTs of 1000 rows in one transaction) | {} |",
        fmt(g_load),
        sizes(disk_size(&gpath))
    );
    println!(
        "| SQLite | {} | {} | {} |",
        fmt(s_load_kv),
        fmt(s_load_rows),
        sizes(sqlite_size(&spath))
    );
    println!("\nKvSQL and SQLite hold both the keys and the table in one file; redb and fjall hold only the keys; GlueSQL holds only the table.\n");

    println!("## Ratios\n");
    println!(
        "- KvSQL SQL point select against GlueSQL: {:.1}x the speed; against SQLite prepared: {:.2}x. A figure above 1x means KvSQL is faster.",
        g_sql.as_nanos() as f64 / k_sql.as_nanos() as f64,
        s_prep.as_nanos() as f64 / k_sql.as_nanos() as f64
    );
    let _ = std::fs::remove_dir_all(&dir);
}
