//! Recorded benchmark: KvSQL against its Rust neighbours (redb, fjall,
//! GlueSQL) with SQLite as the reference, on the same machine, the same data
//! and real files. scripts/record-results.sh runs it and saves the output as
//! test/results/latest/neighbours.md. README.md next to this file explains
//! the method and where the comparison is not like for like.
//!
//! KVSQL_NEIGHBOURS_ROWS sets the key and row count (default 100000) and
//! KVSQL_NEIGHBOURS_COMMITS the synced commits per round (default 200).

#![allow(clippy::needless_range_loop)]

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fjall::Readable;
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

/// The temp folder holding every store's files, removed on drop (also when
/// a check panics).
struct WorkDir(PathBuf);

impl WorkDir {
    fn new() -> WorkDir {
        let p = std::env::temp_dir().join(format!("kvsql-neighbours-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        WorkDir(p)
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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

fn mib(b: u64) -> String {
    format!("{:.1} MiB", b as f64 / 1048576.0)
}

fn sizes((len, alloc): (u64, u64)) -> String {
    format!("{} | {}", mib(len), mib(alloc))
}

/// One timed operation: `ops` operations per round.
struct Case<'a> {
    label: String,
    ops: usize,
    run: Box<dyn FnMut() + 'a>,
}

fn case<'a>(label: &str, ops: usize, run: impl FnMut() + 'a) -> Case<'a> {
    Case {
        label: label.to_string(),
        ops,
        run: Box::new(run),
    }
}

/// Runs the cases interleaved: round r runs every case once, starting at a
/// different case each round, so a slow stretch on the host is spread over
/// the stores instead of landing on one. Prints the median per operation
/// and the fastest and slowest round.
fn table(what: &str, cases: Vec<Case<'_>>) {
    let mut cases = cases;
    let mut times: Vec<Vec<Duration>> = vec![Vec::new(); cases.len()];
    for r in 0..ROUNDS {
        for k in 0..cases.len() {
            let i = (r + k) % cases.len();
            let c = &mut cases[i];
            let t = Instant::now();
            (c.run)();
            times[i].push(t.elapsed() / c.ops as u32);
        }
    }
    println!(
        "| Store and call | Median per {what} | Fastest to slowest round | Per second (median) |"
    );
    println!("|---|---|---|---|");
    for (c, t) in cases.iter().zip(times.iter_mut()) {
        t.sort();
        let med = t[ROUNDS / 2];
        println!(
            "| {} | {} | {} to {} | {:.0} |",
            c.label,
            fmt(med),
            fmt(t[0]),
            fmt(t[ROUNDS - 1]),
            1e9 / med.as_nanos().max(1) as f64
        );
    }
    println!();
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

/// The file system type of a folder, where `stat` can tell.
fn fs_type(p: &Path) -> String {
    std::process::Command::new("stat")
        .args(["-f", "-c", "%T"])
        .arg(p)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
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
    let starts: Vec<usize> = shuffled(n - SCAN_LEN, 11).into_iter().take(500).collect();
    let values: Vec<Vec<u8>> = (0..n).map(|i| name_of(i).into_bytes()).collect();
    let names: Vec<String> = (0..n).map(name_of).collect();
    // GlueSQL's autocommit select syncs to disk on every statement (see the
    // README), so that row reads only this many keys per round.
    let auto_order: Vec<usize> = order.iter().copied().take(500).collect();
    let work = WorkDir::new();
    let dir = &work.0;

    println!("# KvSQL and its neighbours\n");
    println!("- Host: {}", host());
    println!("- Temp folder file system: {}", fs_type(dir));
    println!(
        "- KvSQL {}, redb 4.4.0, fjall 3.1.12, GlueSQL 0.20.0 on its redb storage (which uses redb 2.6), SQLite {} (bundled with rusqlite)",
        kvsql_engine::VERSION,
        rusqlite::version()
    );
    println!("- {n} keys (8-byte big-endian key, 13-byte value) and, for the SQL stores, {n} rows in `t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER)`");
    println!("- Real files in one temp folder, one thread. Cache: 8 MiB for KvSQL (2048 pages), redb, fjall (block cache) and SQLite; GlueSQL's redb storage has no cache setting and runs with redb's default");
    println!("- fjall's data is flushed from its memtable to on-disk tables and compacted after the load, so its reads go through its tables and block cache");
    println!("- Reads: warm, shuffled key order, every key once per round, and every value read is checked. {ROUNDS} rounds, interleaved across the stores (each round runs every store once, starting with a different one); each row gives the median and the fastest and slowest round");
    println!("- Scans: {} scans of {SCAN_LEN} consecutive keys from shuffled start keys, first key and count checked", starts.len());
    println!("- Synced commits: {commits} per round, each one small write synced to disk before the call returns (KvSQL commit, redb Durability::Immediate, fjall insert then persist(SyncData), GlueSQL autocommit on redb, SQLite WAL with synchronous=FULL)\n");

    // ---------------------------------------------------------------- load
    let kpath = dir.join("kvsql.db");
    let kdb = Db::open(&kpath).unwrap();
    let t = Instant::now();
    {
        let mut tx = kdb.begin_write().unwrap();
        for i in 0..n {
            tx.put("kv", &key(i), &values[i]).unwrap();
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
                table.insert(&key(i)[..], &values[i][..]).unwrap();
            }
        }
        tx.commit().unwrap();
    }
    let r_load = t.elapsed();

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
            b.insert(&fks, key(i), values[i].clone());
        }
        b.commit().unwrap();
        fdb.persist(fjall::PersistMode::SyncData).unwrap();
    }
    let f_load = t.elapsed();
    let t = Instant::now();
    fks.rotate_memtable_and_wait().unwrap();
    fks.major_compact().unwrap();
    let f_flush = t.elapsed();

    let gpath = dir.join("gluesql.redb");
    let glue = RefCell::new(Glue::new(RedbStorage::new(&gpath).unwrap()));
    glue.borrow_mut()
        .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER)")
        .unwrap();
    let t = Instant::now();
    {
        let mut g = glue.borrow_mut();
        g.execute("BEGIN").unwrap();
        for chunk in (0..n).collect::<Vec<_>>().chunks(1000) {
            let vals: Vec<String> = chunk
                .iter()
                .map(|&i| format!("({i}, '{}', {})", name_of(i), i % 1000))
                .collect();
            g.execute(format!("INSERT INTO t VALUES {}", vals.join(", ")))
                .unwrap();
        }
        g.execute("COMMIT").unwrap();
    }
    let g_load = t.elapsed();

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
            st.execute(params![key(i).to_vec(), values[i]]).unwrap();
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
    table(
        "read",
        vec![
            case("KvSQL `Db::get`", n, || {
                for &i in &order {
                    assert_eq!(kdb.get("kv", &key(i)).unwrap().unwrap(), values[i]);
                }
            }),
            case("redb `begin_read`, `open_table`, `get`", n, || {
                for &i in &order {
                    let tx = rdb.begin_read().unwrap();
                    let table = tx.open_table(KV).unwrap();
                    assert_eq!(
                        table.get(&key(i)[..]).unwrap().unwrap().value(),
                        &values[i][..]
                    );
                }
            }),
            case("fjall `Keyspace::get`", n, || {
                for &i in &order {
                    assert_eq!(&*fks.get(key(i)).unwrap().unwrap(), &values[i][..]);
                }
            }),
            case("SQLite prepared `SELECT v FROM kv WHERE k = ?`", n, || {
                let mut st = conn.prepare_cached("SELECT v FROM kv WHERE k = ?").unwrap();
                for &i in &order {
                    let v: Vec<u8> = st.query_row([key(i).to_vec()], |r| r.get(0)).unwrap();
                    assert_eq!(v, values[i]);
                }
            }),
        ],
    );

    println!("## Key get inside one open snapshot\n");
    println!("All reads of a round go through one read transaction or snapshot, so the per-call snapshot cost is left out. SQLite is not in this table; its row above already reuses one prepared statement.\n");
    table(
        "read",
        vec![
            case("KvSQL `ReadTx::get`", n, || {
                let mut tx = kdb.begin_read().unwrap();
                for &i in &order {
                    assert_eq!(tx.get("kv", &key(i)).unwrap().unwrap(), values[i]);
                }
            }),
            case("redb `ReadOnlyTable::get`", n, || {
                let tx = rdb.begin_read().unwrap();
                let table = tx.open_table(KV).unwrap();
                for &i in &order {
                    assert_eq!(
                        table.get(&key(i)[..]).unwrap().unwrap().value(),
                        &values[i][..]
                    );
                }
            }),
            case("fjall `Snapshot::get`", n, || {
                let snap = fdb.snapshot();
                for &i in &order {
                    assert_eq!(&*snap.get(&fks, key(i)).unwrap().unwrap(), &values[i][..]);
                }
            }),
        ],
    );

    // --------------------------------------------------------------- scans
    println!("## Range scan of {SCAN_LEN} consecutive keys\n");
    table(
        "scan",
        vec![
            case("KvSQL `Db::scan`", starts.len(), || {
                for &s in &starts {
                    let items = kdb.scan("kv", &key(s), None, SCAN_LEN).unwrap();
                    assert_eq!(items.len(), SCAN_LEN);
                    assert_eq!(items[0].0, key(s));
                    assert_eq!(items[SCAN_LEN - 1].1, values[s + SCAN_LEN - 1]);
                }
            }),
            case(
                "redb `range`, read transaction per scan",
                starts.len(),
                || {
                    for &s in &starts {
                        let tx = rdb.begin_read().unwrap();
                        let table = tx.open_table(KV).unwrap();
                        let lo = key(s);
                        let mut c = 0;
                        for item in table.range(&lo[..]..).unwrap().take(SCAN_LEN) {
                            let (k, v) = item.unwrap();
                            assert_eq!(k.value(), &key(s + c)[..]);
                            assert!(!v.value().is_empty());
                            c += 1;
                        }
                        assert_eq!(c, SCAN_LEN);
                    }
                },
            ),
            case("fjall `Keyspace::range`", starts.len(), || {
                for &s in &starts {
                    let mut c = 0;
                    for item in fks.range(key(s)..).take(SCAN_LEN) {
                        let (k, v) = item.into_inner().unwrap();
                        assert_eq!(&*k, &key(s + c)[..]);
                        assert!(!v.is_empty());
                        c += 1;
                    }
                    assert_eq!(c, SCAN_LEN);
                }
            }),
            case(
                "SQLite prepared `WHERE k >= ? ORDER BY k LIMIT ?`",
                starts.len(),
                || {
                    let mut st = conn
                        .prepare_cached("SELECT k, v FROM kv WHERE k >= ? ORDER BY k LIMIT ?")
                        .unwrap();
                    for &s in &starts {
                        let mut c = 0;
                        let rows = st
                            .query_map(params![key(s).to_vec(), SCAN_LEN as i64], |r| {
                                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                            })
                            .unwrap();
                        for r in rows {
                            let (k, v) = r.unwrap();
                            assert_eq!(k, key(s + c));
                            assert!(!v.is_empty());
                            c += 1;
                        }
                        assert_eq!(c, SCAN_LEN);
                    }
                },
            ),
        ],
    );

    // ----------------------------------------------------------- SQL point
    println!("## Point select by primary key through SQL\n");
    println!("`SELECT name FROM t WHERE id = ?` with the id as a parameter, and the name checked. KvSQL and GlueSQL parse the text on every call (KvSQL keeps a parsed-statement cache; GlueSQL has no prepared statements). GlueSQL runs each round's selects inside one `BEGIN` ... `COMMIT`: on its redb storage an autocommit statement, a SELECT included, opens and commits a redb write transaction and so syncs to disk. That autocommit form is shown on its own row, over {} keys per round. SQLite is shown prepared once and parsing every time. The last row reads the same row through KvSQL's key door.\n", auto_order.len());
    table(
        "select",
        vec![
            case("KvSQL `Db::execute`", n, || {
                for &i in &order {
                    let r = kdb
                        .execute("SELECT name FROM t WHERE id = ?", &[Value::Int(i as i64)])
                        .unwrap();
                    assert_eq!(r.rows, vec![vec![Value::Text(names[i].clone())]]);
                }
            }),
            case(
                "GlueSQL `execute_with_params` inside one transaction",
                n,
                || {
                    let mut g = glue.borrow_mut();
                    g.execute("BEGIN").unwrap();
                    for &i in &order {
                        let rows = glue_rows(
                            g.execute_with_params("SELECT name FROM t WHERE id = $1", [i as i64])
                                .unwrap(),
                        );
                        assert_eq!(rows, vec![vec![GValue::Str(names[i].clone())]]);
                    }
                    g.execute("COMMIT").unwrap();
                },
            ),
            case(
                "GlueSQL `execute_with_params`, autocommit (syncs per statement)",
                auto_order.len(),
                || {
                    let mut g = glue.borrow_mut();
                    for &i in &auto_order {
                        let rows = glue_rows(
                            g.execute_with_params("SELECT name FROM t WHERE id = $1", [i as i64])
                                .unwrap(),
                        );
                        assert_eq!(rows.len(), 1);
                    }
                },
            ),
            case("SQLite, prepared once", n, || {
                let mut st = conn
                    .prepare_cached("SELECT name FROM t WHERE id = ?")
                    .unwrap();
                for &i in &order {
                    let v: String = st.query_row([i as i64], |r| r.get(0)).unwrap();
                    assert_eq!(v, names[i]);
                }
            }),
            case("SQLite, parsed every call", n, || {
                for &i in &order {
                    let mut st = conn.prepare("SELECT name FROM t WHERE id = ?").unwrap();
                    let v: String = st.query_row([i as i64], |r| r.get(0)).unwrap();
                    assert_eq!(v, names[i]);
                }
            }),
            case("KvSQL `Db::get_row` (key door, no SQL)", n, || {
                for &i in &order {
                    let r = kdb.get_row("t", &Value::Int(i as i64)).unwrap().unwrap();
                    assert_eq!(r[1], Value::Text(names[i].clone()));
                }
            }),
        ],
    );

    // ------------------------------------------------------ synced commits
    println!("## One small write per transaction, synced on every commit\n");
    let (mut kn, mut rn, mut fnx, mut gn, mut sn) = (n, n, n, n, n);
    table(
        "commit",
        vec![
            case("KvSQL `Db::put`", commits, || {
                for _ in 0..commits {
                    kdb.put("kv", &key(kn), b"fresh value!!").unwrap();
                    kn += 1;
                }
            }),
            case(
                "redb write transaction, `Durability::Immediate`",
                commits,
                || {
                    for _ in 0..commits {
                        let mut tx = rdb.begin_write().unwrap();
                        tx.set_durability(redb::Durability::Immediate).unwrap();
                        {
                            let mut table = tx.open_table(KV).unwrap();
                            table.insert(&key(rn)[..], &b"fresh value!!"[..]).unwrap();
                        }
                        tx.commit().unwrap();
                        rn += 1;
                    }
                },
            ),
            case("fjall `insert` then `persist(SyncData)`", commits, || {
                for _ in 0..commits {
                    fks.insert(key(fnx), b"fresh value!!").unwrap();
                    fdb.persist(fjall::PersistMode::SyncData).unwrap();
                    fnx += 1;
                }
            }),
            case("GlueSQL `INSERT INTO t`, autocommit", commits, || {
                let mut g = glue.borrow_mut();
                for _ in 0..commits {
                    g.execute_with_params(
                        "INSERT INTO t VALUES ($1, 'fresh value!', 1)",
                        [gn as i64],
                    )
                    .unwrap();
                    gn += 1;
                }
            }),
            case(
                "SQLite prepared `INSERT INTO kv`, autocommit",
                commits,
                || {
                    let mut st = conn.prepare_cached("INSERT INTO kv VALUES (?, ?)").unwrap();
                    for _ in 0..commits {
                        st.execute(params![key(sn).to_vec(), b"fresh value!!".to_vec()])
                            .unwrap();
                        sn += 1;
                    }
                },
            ),
        ],
    );

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
    println!("Loads are timed once, before the tables above. KvSQL, redb, fjall and SQLite load through their APIs one key or row at a time; GlueSQL takes {} `INSERT` statements of up to 1000 rows each in one transaction. fjall's load is one write batch and a sync; flushing it to on-disk tables and compacting them took {} more.\n", n.div_ceil(1000), fmt(f_flush));
    println!("Sizes are read after every write above, with each store closed (fjall: its whole folder; SQLite: the database file and its WAL). File length is what a directory listing shows; allocated is what the file system holds (fjall preallocates its journal to 64 MiB as a sparse file). The stores hold different things, so sizes can't be compared across rows: KvSQL and SQLite hold the keys, the table and space left by about a thousand commits; redb and fjall hold only the keys; GlueSQL only the table.\n");
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
        "| fjall | {} | | {} |",
        fmt(f_load),
        sizes(disk_size(&fpath))
    );
    println!(
        "| GlueSQL | | {} | {} |",
        fmt(g_load),
        sizes(disk_size(&gpath))
    );
    println!(
        "| SQLite | {} | {} | {} |",
        fmt(s_load_kv),
        fmt(s_load_rows),
        sizes(sqlite_size(&spath))
    );
}
