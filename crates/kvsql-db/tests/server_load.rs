//! KvSQL DB under load: many clients at once, and a real kvsqld process
//! killed with SIGKILL while clients are writing.
//!
//! KVSQL_SERVER_SECS sets how long the concurrent test runs (default 3).
//! KVSQL_SERVER_KILLS sets how many times the server is killed (default 10).

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use kvsql_db::http::Client;
use kvsql_db::json::{self, Json};
use kvsql_db::server::{self, Config};
use kvsql_engine::sim::{Rng, SimStorage};
use kvsql_engine::sql::Value;
use kvsql_engine::{Db, Options};

const ACCOUNTS: i64 = 20;
const START: i64 = 1000;

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn temp_path(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("kvsql-db-{tag}-{}-{n}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn post_sql(c: &mut Client, sql: &str, params: Vec<Json>) -> std::io::Result<(u16, Json)> {
    let body = Json::obj(vec![
        ("sql", Json::Str(sql.into())),
        ("params", Json::Arr(params)),
    ]);
    let (s, b) = c.request("POST", "/sql", body.to_string_compact().as_bytes())?;
    let j = json::parse(&String::from_utf8_lossy(&b)).unwrap_or(Json::Null);
    Ok((s, j))
}

const SETUP: &str = "CREATE TABLE acct (id INTEGER PRIMARY KEY, bal INTEGER NOT NULL); \
                     CREATE TABLE log (id INTEGER PRIMARY KEY, client INTEGER, n INTEGER); \
                     CREATE INDEX log_client ON log (client)";

fn seed_accounts(c: &mut Client) {
    let (s, j) = post_sql(c, SETUP, vec![]).unwrap();
    assert_eq!(s, 200, "{j:?}");
    let values: Vec<String> = (0..ACCOUNTS).map(|i| format!("({i}, {START})")).collect();
    let (s, j) = post_sql(
        c,
        &format!("INSERT INTO acct VALUES {}", values.join(", ")),
        vec![],
    )
    .unwrap();
    assert_eq!(s, 200, "{j:?}");
}

const TRANSFER: &str =
    "UPDATE acct SET bal = bal - ? WHERE id = ?; UPDATE acct SET bal = bal + ? WHERE id = ?";

fn transfer(c: &mut Client, rng: &mut Rng) -> std::io::Result<u16> {
    let from = rng.below(ACCOUNTS as u64) as i64;
    let to = rng.below(ACCOUNTS as u64) as i64;
    let amt = rng.below(50) as i64;
    let p = vec![
        Json::Int(amt),
        Json::Int(from),
        Json::Int(amt),
        Json::Int(to),
    ];
    Ok(post_sql(c, TRANSFER, p)?.0)
}

fn total(c: &mut Client) -> (i64, i64) {
    let (s, j) = post_sql(c, "SELECT sum(bal), count(*) FROM acct", vec![]).unwrap();
    assert_eq!(s, 200, "{j:?}");
    let Some(Json::Arr(rows)) = j.get("rows") else {
        panic!("{j:?}")
    };
    let Json::Arr(r) = &rows[0] else { panic!() };
    let (Json::Int(sum), Json::Int(n)) = (&r[0], &r[1]) else {
        panic!("{r:?}")
    };
    (*sum, *n)
}

/// Writers move money and append log rows while readers check that the
/// total never changes. Every client has its own connection.
#[test]
fn concurrent_clients_see_consistent_totals() {
    let db = Arc::new(Db::open_with(Box::new(SimStorage::new()), Options::default()).unwrap());
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    server::spawn(l, db.clone(), Config::default());
    seed_accounts(&mut Client::new(&url, None).unwrap());

    let secs = env_num("KVSQL_SERVER_SECS", 3);
    let stop = Arc::new(AtomicBool::new(false));
    let transfers = Arc::new(AtomicU64::new(0));
    let checks = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for w in 0..6u64 {
        let (url, stop, transfers) = (url.clone(), stop.clone(), transfers.clone());
        handles.push(thread::spawn(move || {
            let mut c = Client::new(&url, None).unwrap();
            let mut rng = Rng::new(w + 1);
            let mut n = 0i64;
            while !stop.load(Ordering::SeqCst) {
                assert_eq!(transfer(&mut c, &mut rng).unwrap(), 200);
                let id = (w as i64) << 32 | n;
                let p = vec![Json::Int(id), Json::Int(w as i64), Json::Int(n)];
                let (s, j) = post_sql(&mut c, "INSERT INTO log VALUES (?, ?, ?)", p).unwrap();
                assert_eq!(s, 200, "{j:?}");
                n += 1;
                transfers.fetch_add(1, Ordering::Relaxed);
            }
            (w as i64, n)
        }));
    }
    let mut readers = Vec::new();
    for _ in 0..4 {
        let (url, stop, checks) = (url.clone(), stop.clone(), checks.clone());
        readers.push(thread::spawn(move || {
            let mut c = Client::new(&url, None).unwrap();
            while !stop.load(Ordering::SeqCst) {
                assert_eq!(total(&mut c), (ACCOUNTS * START, ACCOUNTS));
                // The KV door sees the same rows.
                let (s, _) = c.request("GET", "/rows/acct/3", b"").unwrap();
                assert_eq!(s, 200);
                checks.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    thread::sleep(Duration::from_secs(secs));
    stop.store(true, Ordering::SeqCst);
    let written: Vec<(i64, i64)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    for r in readers {
        r.join().unwrap();
    }
    let mut c = Client::new(&url, None).unwrap();
    assert_eq!(total(&mut c), (ACCOUNTS * START, ACCOUNTS));
    for (w, n) in written {
        let (_, j) = post_sql(
            &mut c,
            "SELECT count(*) FROM log WHERE client = ?",
            vec![Json::Int(w)],
        )
        .unwrap();
        assert_eq!(
            j.get("rows").unwrap().to_string_compact(),
            format!("[[{n}]]")
        );
    }
    db.verify().unwrap();
    println!(
        "concurrent clients: {} transfers from 6 writers, {} total checks from 4 readers in {secs}s",
        transfers.load(Ordering::Relaxed),
        checks.load(Ordering::Relaxed)
    );
}

struct Server {
    child: Child,
    url: String,
}

fn start_server(path: &Path) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kvsqld"))
        .args(["--db", path.to_str().unwrap(), "--listen", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let addr = line
        .split(" listening on ")
        .nth(1)
        .and_then(|r| r.split(',').next())
        .unwrap_or_else(|| panic!("unexpected first line: {line:?}"));
    Server {
        child,
        url: format!("http://{addr}"),
    }
}

/// What the clients were told was committed.
#[derive(Default)]
struct Acked {
    logs: Vec<(i64, i64)>,
    kv: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Start kvsqld, put it under load from several clients, SIGKILL it at a
/// random moment, then open the file directly: every write that got a 200
/// must be there, the money total must hold, and verify must pass.
#[test]
fn kill_server_under_load() {
    let rounds = env_num("KVSQL_SERVER_KILLS", 10);
    let path = temp_path("kill");
    let mut rng = Rng::new(0x5eed);
    let acked = Arc::new(Mutex::new(Acked::default()));
    let mut acked_total = 0usize;
    for round in 0..rounds {
        let mut srv = start_server(&path);
        if round == 0 {
            seed_accounts(&mut Client::new(&srv.url, None).unwrap());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        for w in 0..4u64 {
            let (url, stop, acked) = (srv.url.clone(), stop.clone(), acked.clone());
            let seed = round * 100 + w + 1;
            handles.push(thread::spawn(move || {
                let mut rng = Rng::new(seed);
                let Ok(mut c) = Client::new(&url, None) else {
                    return;
                };
                let mut n = 0i64;
                while !stop.load(Ordering::SeqCst) {
                    match rng.below(3) {
                        0 => {
                            if transfer(&mut c, &mut rng).is_err() {
                                return;
                            }
                        }
                        1 => {
                            let id = (seed as i64) << 32 | n;
                            let p = vec![Json::Int(id), Json::Int(seed as i64), Json::Int(n)];
                            match post_sql(&mut c, "INSERT INTO log VALUES (?, ?, ?)", p) {
                                Ok((200, _)) => acked.lock().unwrap().logs.push((id, n)),
                                Ok((s, j)) => panic!("insert failed: {s} {j:?}"),
                                Err(_) => return,
                            }
                        }
                        _ => {
                            let key = format!("k{seed}-{n}").into_bytes();
                            let val = vec![(n % 251) as u8; rng.below(3000) as usize];
                            let path = format!("/kv/blobs/{}", String::from_utf8_lossy(&key));
                            match c.request("PUT", &path, &val) {
                                Ok((200, _)) => acked.lock().unwrap().kv.push((key, val)),
                                Ok((s, _)) => panic!("put failed: {s}"),
                                Err(_) => return,
                            }
                        }
                    }
                    n += 1;
                }
            }));
        }
        thread::sleep(Duration::from_millis(50 + rng.below(250)));
        srv.child.kill().unwrap();
        srv.child.wait().unwrap();
        stop.store(true, Ordering::SeqCst);
        for h in handles {
            h.join().unwrap();
        }

        // The process is gone and its file lock with it.
        let db = Db::open(&path).unwrap();
        db.verify().unwrap();
        let r = db
            .execute("SELECT sum(bal), count(*) FROM acct", &[])
            .unwrap();
        assert_eq!(
            r.rows,
            vec![vec![Value::Int(ACCOUNTS * START), Value::Int(ACCOUNTS)]]
        );
        let a = acked.lock().unwrap();
        for (id, n) in &a.logs {
            let row = db.get_row("log", &Value::Int(*id)).unwrap();
            assert!(
                row.is_some(),
                "round {round}: acknowledged log row {id} lost"
            );
            assert_eq!(row.unwrap()[2], Value::Int(*n));
        }
        for (k, v) in &a.kv {
            assert_eq!(
                db.get("blobs", k).unwrap().as_ref(),
                Some(v),
                "round {round}: acknowledged put lost"
            );
        }
        acked_total = a.logs.len() + a.kv.len();
    }
    let _ = std::fs::remove_file(&path);
    println!("server kills: {rounds} SIGKILLs under load, {acked_total} acknowledged writes all present afterwards");
}
