//! The case studies and use examples on kvsql.com, as runnable programs.
//! scripts/record-results.sh runs this and saves the output as
//! test/results/latest/case-studies.md; every figure the site quotes for
//! these comes from that file.
//!
//! Each study uses real files in the temp directory, except the checkout
//! study, which runs on the simulated disk so it can cut the power at every
//! point of a commit.

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use kvsql_db::http::Client;
use kvsql_db::json::{self, Json};
use kvsql_db::server::{self, Config};
use kvsql_db::shell::{self, Backend};
use kvsql_engine::sim::{Rng, SimStorage};
use kvsql_engine::sql::Value;
use kvsql_engine::{Db, Options};

fn temp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("kvsql-case-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn us(d: Duration, n: usize) -> String {
    format!("{:.2} µs", d.as_nanos() as f64 / n as f64 / 1000.0)
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn host() -> String {
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
    let threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    format!(
        "{} {}, {cpu}, {threads} hardware threads",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// A web app's sessions: looked up by token on every request, counted by
/// SQL for a dashboard, and revoked per user in one transaction.
fn sessions() {
    println!("## Case study: sessions and reporting in one store\n");
    let path = temp("sessions.db");
    let db = Db::open(&path).unwrap();
    db.execute(
        "CREATE TABLE sessions (token TEXT PRIMARY KEY, user_id INTEGER NOT NULL,
           country TEXT, expires INTEGER NOT NULL);
         CREATE INDEX sessions_user ON sessions(user_id)",
        &[],
    )
    .unwrap();
    let countries = ["DE", "US", "IL", "FR", "JP", "BR", "IN", "GB"];
    let n = 100_000i64;
    let now = 1_800_000_000i64;
    let token = |i: i64| {
        format!(
            "tok-{:016x}",
            (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        )
    };
    let t = Instant::now();
    let mut tx = db.begin_write().unwrap();
    for i in 0..n {
        tx.put_row(
            "sessions",
            &[
                Value::Text(token(i)),
                Value::Int(i % 20_000),
                Value::Text(countries[(i % 8) as usize].into()),
                Value::Int(now - 3_600 + (i % 7_200)),
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let load = t.elapsed();
    println!(
        "- {n} sessions for 20,000 users, written in one transaction with `put_row`: {}",
        ms(load)
    );

    // Every request: one lookup by token through the key door.
    let mut rng = Rng::new(3);
    let lookups = 200_000usize;
    let t = Instant::now();
    let mut found = 0;
    for _ in 0..lookups {
        let i = rng.below(n as u64) as i64;
        if db
            .get_row("sessions", &Value::Text(token(i)))
            .unwrap()
            .is_some()
        {
            found += 1;
        }
    }
    let d = t.elapsed();
    assert_eq!(found, lookups);
    println!(
        "- {lookups} random token lookups with `get_row`: {} each, all found",
        us(d, lookups)
    );

    // The same lookup through SQL, for comparison.
    let t = Instant::now();
    for _ in 0..lookups {
        let i = rng.below(n as u64) as i64;
        let r = db
            .execute(
                "SELECT user_id FROM sessions WHERE token = ?",
                &[Value::Text(token(i))],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 1);
    }
    println!(
        "- The same lookups through `SELECT user_id FROM sessions WHERE token = ?`: {} each",
        us(t.elapsed(), lookups)
    );

    // The dashboard: live sessions per country, from the same rows.
    let t = Instant::now();
    let r = db
        .execute(
            "SELECT country, count(*) FROM sessions WHERE expires > ? GROUP BY country ORDER BY 2 DESC, 1",
            &[Value::Int(now)],
        )
        .unwrap();
    let d = t.elapsed();
    let live: i64 = r.rows.iter().map(|row| int(&row[1])).sum();
    println!(
        "- Dashboard query, live sessions per country over all {n} rows: {} ({live} live, {} countries)",
        ms(d),
        r.rows.len()
    );

    // Revoke every session of one user in one transaction, while a reader
    // that started before keeps its view.
    let user = 4_242i64;
    let before = db
        .execute(
            "SELECT count(*) FROM sessions WHERE user_id = ?",
            &[Value::Int(user)],
        )
        .unwrap()
        .rows[0][0]
        .clone();
    let mut old_view = db.begin_read().unwrap();
    let t = Instant::now();
    let r = db
        .execute(
            "DELETE FROM sessions WHERE user_id = ?",
            &[Value::Int(user)],
        )
        .unwrap();
    let d = t.elapsed();
    let still_seen = old_view
        .query(
            "SELECT count(*) FROM sessions WHERE user_id = ?",
            &[Value::Int(user)],
        )
        .unwrap()
        .rows[0][0]
        .clone();
    drop(old_view);
    let after = db
        .execute(
            "SELECT count(*) FROM sessions WHERE user_id = ?",
            &[Value::Int(user)],
        )
        .unwrap()
        .rows[0][0]
        .clone();
    let gone = db.get_row("sessions", &Value::Text(token(user))).unwrap();
    assert!(gone.is_none());
    println!(
        "- Revoking user {user}: {} sessions before, {} deleted through the index in {} including the synced commit, {} left; a reader opened before the delete still counted {}; `get_row` on one of the revoked tokens then found nothing",
        int(&before),
        r.changes,
        ms(d),
        int(&after),
        int(&still_seen)
    );
    let v = db.verify().unwrap();
    println!(
        "- `verify` afterwards: {} pages, {} keys, every index entry matches its row\n",
        v.page_count, v.keys
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// An edge gateway: devices post readings to kvsqld over HTTP, the
/// dashboard reads the latest value by key and history through SQL.
fn gateway() {
    println!("## Case study: an edge gateway with kvsqld\n");
    let path = temp("gateway.db");
    let db = Arc::new(Db::open(&path).unwrap());
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    server::spawn(l, db.clone(), Config::default());
    let mut admin = Client::new(&url, None).unwrap();
    let setup = "CREATE TABLE readings (id INTEGER PRIMARY KEY, device TEXT NOT NULL, ts INTEGER NOT NULL, temp REAL);
                 CREATE INDEX readings_device ON readings(device);
                 CREATE TABLE latest (device TEXT PRIMARY KEY, ts INTEGER, temp REAL)";
    let (s, _) = admin.request("POST", "/sql", setup.as_bytes()).unwrap();
    assert_eq!(s, 200);

    let devices = 20u64;
    let per_device = 500u64;
    let t = Instant::now();
    let mut handles = Vec::new();
    for d in 0..devices {
        let url = url.clone();
        handles.push(thread::spawn(move || {
            let mut c = Client::new(&url, None).unwrap();
            let mut rng = Rng::new(100 + d);
            let name = format!("dev-{d:02}");
            for k in 0..per_device {
                let id = (d * per_device + k + 1) as i64;
                let ts = 1_800_000_000 + k as i64 * 30;
                // Device 7 overheats for a while in the middle of the run.
                let hot = d == 7 && (200..240).contains(&k);
                let temp = if hot {
                    31.0 + rng.below(40) as f64 / 10.0
                } else {
                    18.0 + rng.below(80) as f64 / 10.0
                };
                // One request, one transaction: the history row and the
                // latest value change together.
                let body = Json::obj(vec![
                    (
                        "sql",
                        Json::Str(
                            "INSERT INTO readings VALUES (?, ?, ?, ?);
                             DELETE FROM latest WHERE device = ?;
                             INSERT INTO latest VALUES (?, ?, ?)"
                                .into(),
                        ),
                    ),
                    (
                        "params",
                        Json::Arr(vec![
                            Json::Int(id),
                            Json::Str(name.clone()),
                            Json::Int(ts),
                            Json::Real(temp),
                            Json::Str(name.clone()),
                            Json::Str(name.clone()),
                            Json::Int(ts),
                            Json::Real(temp),
                        ]),
                    ),
                ]);
                let (s, b) = c
                    .request("POST", "/sql", body.to_string_compact().as_bytes())
                    .unwrap();
                assert_eq!(s, 200, "{}", String::from_utf8_lossy(&b));
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let d = t.elapsed();
    let total = devices * per_device;
    println!(
        "- {devices} devices on their own connections sent {total} readings, one request each, every request a synced transaction holding the history row and the latest value: {:.1} s, about {:.0} requests a second",
        d.as_secs_f64(),
        total as f64 / d.as_secs_f64()
    );

    // The dashboard tile: latest value by key, no SQL.
    let n = 2_000usize;
    let t = Instant::now();
    let mut last = Json::Null;
    for i in 0..n {
        let (s, b) = admin
            .request(
                "GET",
                &format!("/rows/latest/dev-{:02}", i % devices as usize),
                b"",
            )
            .unwrap();
        assert_eq!(s, 200);
        last = json::parse(&String::from_utf8_lossy(&b)).unwrap();
    }
    println!(
        "- {n} dashboard reads of `GET /rows/latest/dev-NN` over HTTP: {} each, round trip included (last answer `{}`)",
        us(t.elapsed(), n),
        last.to_string_compact()
    );

    // History and alerts through SQL over the same records.
    let q = |c: &mut Client, sql: &str| -> (Duration, Json) {
        let t = Instant::now();
        let (s, b) = c.request("POST", "/sql", sql.as_bytes()).unwrap();
        assert_eq!(s, 200, "{}", String::from_utf8_lossy(&b));
        (
            t.elapsed(),
            json::parse(&String::from_utf8_lossy(&b)).unwrap(),
        )
    };
    let (d1, per) = q(
        &mut admin,
        "SELECT device, count(*), round(avg(temp), 2), max(temp) FROM readings GROUP BY device ORDER BY max(temp) DESC LIMIT 3",
    );
    let (d2, hot) = q(
        &mut admin,
        "SELECT device, count(*), min(ts), max(ts) FROM readings WHERE temp > 30.0 GROUP BY device",
    );
    let (d3, one) = q(
        &mut admin,
        "SELECT count(*), round(avg(temp), 2) FROM readings WHERE device = 'dev-07'",
    );
    println!(
        "- Per-device summary over all {total} readings, top three by peak: {}, answer `{}`",
        ms(d1),
        per.get("rows").unwrap().to_string_compact()
    );
    println!(
        "- Alert query, readings over 30 °C: {}, answer `{}` (device, count, first and last timestamp)",
        ms(d2),
        hot.get("rows").unwrap().to_string_compact()
    );
    println!(
        "- One device's history through the index: {}, answer `{}`",
        ms(d3),
        one.get("rows").unwrap().to_string_compact()
    );
    let (s, b) = admin.request("GET", "/verify", b"").unwrap();
    assert_eq!(s, 200);
    println!(
        "- `GET /verify` afterwards: `{}`\n",
        String::from_utf8_lossy(&b)
    );
    let _ = std::fs::remove_file(&path);
}

fn shop_db(storage: SimStorage) -> Db {
    Db::open_with(Box::new(storage), Options::default()).unwrap()
}

/// What the shop holds, read back for checking.
#[derive(Debug, PartialEq)]
struct ShopState {
    cart: Option<Vec<u8>>,
    stock: Vec<Vec<Value>>,
    orders: Vec<Vec<Value>>,
    lines: i64,
}

fn shop_state(db: &Db) -> ShopState {
    ShopState {
        cart: db.get("carts", b"customer-17").unwrap(),
        stock: db
            .execute("SELECT sku, qty FROM stock ORDER BY sku", &[])
            .unwrap()
            .rows,
        orders: db
            .execute("SELECT id, customer, total FROM orders ORDER BY id", &[])
            .unwrap()
            .rows,
        lines: int(&db
            .execute("SELECT count(*) FROM order_lines", &[])
            .unwrap()
            .rows[0][0]),
    }
}

/// The checkout: read the cart by key, write the order and its lines by
/// SQL, take the stock by SQL, delete the cart by key, all in one commit.
fn checkout(db: &Db) -> kvsql_engine::Result<()> {
    let mut tx = db.begin_write()?;
    let cart = tx.get("carts", b"customer-17")?.expect("cart");
    let cart = String::from_utf8(cart).unwrap();
    tx.execute("INSERT INTO orders VALUES (5001, 'customer-17', 0)", &[])?;
    let mut total = 0i64;
    for (n, item) in cart.split(',').enumerate() {
        let (sku, qty) = item.split_once('x').unwrap();
        let qty: i64 = qty.parse().unwrap();
        let r = tx.execute(
            "UPDATE stock SET qty = qty - ? WHERE sku = ? AND qty >= ?",
            &[Value::Int(qty), Value::Text(sku.into()), Value::Int(qty)],
        )?;
        assert_eq!(r.changes, 1, "out of stock");
        let price = int(&tx.get_row("prices", &Value::Text(sku.into()))?.unwrap()[1]);
        total += price * qty;
        tx.execute(
            "INSERT INTO order_lines VALUES (?, 5001, ?, ?)",
            &[
                Value::Int(n as i64 + 1),
                Value::Text(sku.into()),
                Value::Int(qty),
            ],
        )?;
    }
    tx.execute(
        "UPDATE orders SET total = ? WHERE id = 5001",
        &[Value::Int(total)],
    )?;
    tx.delete("carts", b"customer-17")?;
    tx.commit()
}

/// The power goes at every write or sync of the checkout's commit, and each
/// cut is replayed with several disk behaviours. Every reopened file must
/// hold the shop before the checkout or after it.
fn checkout_power_cuts() {
    println!("## Case study: a checkout that is never half done\n");
    let storage = SimStorage::new();
    let db = shop_db(storage.clone());
    db.execute(
        "CREATE TABLE stock (sku TEXT PRIMARY KEY, qty INTEGER NOT NULL);
         CREATE TABLE prices (sku TEXT PRIMARY KEY, cents INTEGER NOT NULL);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, customer TEXT NOT NULL, total INTEGER);
         CREATE TABLE order_lines (id INTEGER PRIMARY KEY, order_id INTEGER, sku TEXT, qty INTEGER);
         CREATE INDEX orders_customer ON orders(customer);
         CREATE INDEX lines_order ON order_lines(order_id);
         INSERT INTO stock VALUES ('kettle', 12), ('mug', 40), ('teapot', 5), ('tray', 9);
         INSERT INTO prices VALUES ('kettle', 3900), ('mug', 850), ('teapot', 2400), ('tray', 1500)",
        &[],
    )
    .unwrap();
    // Older orders and a few thousand other carts, so the commit touches a
    // real tree and not a single page.
    let mut tx = db.begin_write().unwrap();
    for i in 0..3_000i64 {
        tx.put(
            "carts",
            format!("customer-{:05}", i + 100).as_bytes(),
            b"mugx1",
        )
        .unwrap();
        tx.execute(
            "INSERT INTO orders VALUES (?, ?, ?)",
            &[
                Value::Int(i + 1),
                Value::Text(format!("customer-{:05}", i % 400)),
                Value::Int(850),
            ],
        )
        .unwrap();
    }
    tx.put("carts", b"customer-17", b"kettlex1,mugx4,teapotx1")
        .unwrap();
    tx.commit().unwrap();
    drop(db);
    let image = storage.crash_image(&mut Rng::new(0));
    let before = shop_state(&shop_db(SimStorage::from_image(image.clone())));

    // The run with no failure, to learn the checkout's shape.
    let clean = SimStorage::from_image(image.clone());
    let db = shop_db(clean.clone());
    let (w0, s0) = clean.counts();
    checkout(&db).unwrap();
    let (w1, s1) = clean.counts();
    let after = shop_state(&db);
    drop(db);
    let steps = (w1 - w0) + (s1 - s0);
    println!(
        "- The checkout reads the cart by key, inserts the order and three order lines and takes the stock through SQL, reads each price by key, and deletes the cart by key, all in one transaction. Its commit is {} writes and {} syncs on the simulated disk",
        w1 - w0,
        s1 - s0
    );
    println!(
        "- Before: cart `{}`, stock {:?}. After: no cart, stock {:?}, order 5001 total {} cents",
        String::from_utf8_lossy(before.cart.as_ref().unwrap()),
        before.stock.iter().map(|r| int(&r[1])).collect::<Vec<_>>(),
        after.stock.iter().map(|r| int(&r[1])).collect::<Vec<_>>(),
        int(&after.orders.last().unwrap()[2])
    );

    let mut rng = Rng::new(17);
    let (mut images, mut kept_before, mut kept_after) = (0u64, 0u64, 0u64);
    for cut in 0..=steps {
        let s = SimStorage::from_image(image.clone());
        let db = shop_db(s.clone());
        s.fail_after(cut);
        let r = checkout(&db);
        drop(db);
        for _ in 0..200 {
            let img = s.crash_image(&mut rng);
            let db = shop_db(SimStorage::from_image(img));
            db.verify().expect("verify after a cut");
            let st = shop_state(&db);
            if st == before {
                kept_before += 1;
            } else if st == after {
                kept_after += 1;
            } else {
                panic!("cut {cut}: half a checkout: {st:?}");
            }
            images += 1;
        }
        if cut == steps {
            assert!(r.is_ok());
        }
    }
    println!(
        "- Power cut after each of the {} steps and after none, each cut replayed 200 ways (every unsynced write kept, none kept, any subset, subsets with torn sectors): {images} reopened files",
        steps
    );
    println!(
        "- Every one passed `verify` and held either the shop before the checkout ({kept_before}) or after it ({kept_after}). None held an order without its stock taken, or a cart still there beside its order\n"
    );
}

/// Short use examples, each a few lines of real API use.
fn use_examples() {
    println!("## Use example: a cache with expiry\n");
    let db = Db::open_with(Box::new(SimStorage::new()), Options::default()).unwrap();
    db.execute(
        "CREATE TABLE cache (key TEXT PRIMARY KEY, body BLOB, expires INTEGER NOT NULL);
         CREATE INDEX cache_expires ON cache(expires)",
        &[],
    )
    .unwrap();
    let mut tx = db.begin_write().unwrap();
    for i in 0..10_000i64 {
        tx.put_row(
            "cache",
            &[
                Value::Text(format!("/api/items/{i}")),
                Value::Blob(vec![b'x'; 200]),
                Value::Int(1_000 + i % 600),
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let hit = db
        .get_row("cache", &Value::Text("/api/items/42".into()))
        .unwrap();
    let miss = db
        .get_row("cache", &Value::Text("/api/items/99999".into()))
        .unwrap();
    let swept = db
        .execute("DELETE FROM cache WHERE expires < ?", &[Value::Int(1_300)])
        .unwrap()
        .changes;
    let left = int(&db.execute("SELECT count(*) FROM cache", &[]).unwrap().rows[0][0]);
    println!(
        "- 10,000 cached responses. `get_row` hit: {}, miss: {}. Sweep `DELETE FROM cache WHERE expires < 1300`: {swept} removed, {left} left\n",
        hit.is_some(),
        miss.is_some()
    );

    println!("## Use example: settings read by key, audited by SQL\n");
    let db = Db::open_with(Box::new(SimStorage::new()), Options::default()).unwrap();
    let mut tx = db.begin_write().unwrap();
    for (k, v) in [
        ("checkout.new_flow", "on"),
        ("search.fuzzy", "off"),
        ("search.limit", "50"),
        ("upload.max_mb", "20"),
    ] {
        tx.put("settings", k.as_bytes(), v.as_bytes()).unwrap();
    }
    tx.commit().unwrap();
    let flag = db.get("settings", b"checkout.new_flow").unwrap();
    let r = db
        .execute(
            "SELECT CAST(key AS TEXT), CAST(value AS TEXT) FROM settings WHERE CAST(key AS TEXT) LIKE 'search.%' ORDER BY 1",
            &[],
        )
        .unwrap();
    let shown: Vec<String> = r
        .rows
        .iter()
        .map(|row| format!("{} = {}", row[0], row[1]))
        .collect();
    println!(
        "- `get(\"settings\", b\"checkout.new_flow\")` gives `{}`. The same plain keyspace in SQL, with no schema, `WHERE CAST(key AS TEXT) LIKE 'search.%'`: `{}`\n",
        String::from_utf8_lossy(&flag.unwrap()),
        shown.join("`, `")
    );

    println!("## Use example: the kvsql shell on a file\n");
    let path = temp("shell.db");
    let mut b = Backend::Local(Db::open(&path).unwrap());
    println!("```");
    for line in [
        "CREATE TABLE jobs (id INTEGER PRIMARY KEY, kind TEXT, state TEXT)",
        "INSERT INTO jobs VALUES (1, 'resize', 'done'), (2, 'resize', 'queued'), (3, 'email', 'queued')",
        "SELECT state, count(*) AS n FROM jobs GROUP BY state",
        ".row jobs 2",
        ".put notes deploy shipped 0.1.0",
        ".get notes deploy",
        ".verify",
    ] {
        println!("kvsql> {line}");
        match shell::run_line(&mut b, line) {
            Some(Ok(s)) if !s.is_empty() => println!("{s}"),
            Some(Err(e)) => println!("{e}"),
            _ => {}
        }
    }
    println!("```\n");
    drop(b);
    let _ = std::fs::remove_file(&path);
}

fn main() {
    println!("# KvSQL case studies and use examples\n");
    println!("- Host: {}", host());
    println!("- KvSQL {}, release build, one run", kvsql_engine::VERSION);
    println!("- Sessions and gateway: real files in the temp directory, every commit synced. Checkout: the simulated disk used by the power-cut tests\n");
    sessions();
    gateway();
    checkout_power_cuts();
    use_examples();
}
