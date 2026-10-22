//! Readers working beside the writer.

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::*;
use kvsql_engine::sim::{Rng, SimStorage};
use kvsql_engine::{Db, Options, ReadTx};

fn sim_db(cache_pages: usize) -> Db {
    Db::open_with(Box::new(SimStorage::new()), Options { cache_pages }).unwrap()
}

#[test]
fn reader_keeps_its_snapshot() {
    let db = sim_db(64);
    db.put("t", b"a", b"1").unwrap();
    db.put("t", b"b", b"1").unwrap();
    let mut r = db.begin_read().unwrap();
    let seen = r.txid();

    let mut tx = db.begin_write().unwrap();
    tx.put("t", b"a", b"2").unwrap();
    tx.delete("t", b"b").unwrap();
    tx.put("u", b"new", b"x").unwrap();
    tx.commit().unwrap();

    assert_eq!(r.txid(), seen);
    assert_eq!(r.get("t", b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(r.get("t", b"b").unwrap(), Some(b"1".to_vec()));
    assert_eq!(r.keyspaces().unwrap(), vec!["t".to_string()]);
    // A new reader sees the new commit.
    let mut r2 = db.begin_read().unwrap();
    assert_eq!(r2.get("t", b"a").unwrap(), Some(b"2".to_vec()));
    assert_eq!(r2.get("t", b"b").unwrap(), None);
    assert_eq!(r2.txid(), seen + 1);
}

#[test]
fn reader_does_not_block_writer() {
    let db = sim_db(64);
    db.put("t", b"k", b"0").unwrap();
    let r = db.begin_read().unwrap();
    // The writer commits many times while the reader is open.
    for i in 0..100u32 {
        db.put("t", b"k", i.to_string().as_bytes()).unwrap();
    }
    drop(r);
    assert_eq!(db.get("t", b"k").unwrap(), Some(b"99".to_vec()));
}

#[test]
fn open_reader_holds_pages_until_it_closes() {
    let db = sim_db(64);
    let v = vec![1u8; 300];
    let fill = |db: &Db| {
        let mut tx = db.begin_write().unwrap();
        for i in 0..300u32 {
            tx.put("t", format!("{i:05}").as_bytes(), &v).unwrap();
        }
        tx.commit().unwrap();
    };
    fill(&db);
    let mut r = db.begin_read().unwrap();
    let before = r.scan("t", b"", None, usize::MAX).unwrap();
    for _ in 0..20 {
        fill(&db); // rewrites every leaf, freeing the old ones
    }
    let grown = db.verify().unwrap().page_count;
    // The reader's pages were not reused, so its view is intact.
    assert_eq!(r.scan("t", b"", None, usize::MAX).unwrap(), before);
    drop(r);
    for _ in 0..20 {
        fill(&db);
    }
    let after = db.verify().unwrap().page_count;
    assert!(
        after <= grown,
        "pages were not reused after the reader closed: {grown} -> {after}"
    );
}

/// Account value: balance, then padding that can be regenerated from a seed,
/// so a reader can tell a value it reads is exactly the one written.
fn account_value(balance: u64, rng: &mut Rng) -> Vec<u8> {
    let len = match rng.below(10) {
        0 => 1500 + rng.below(9000) as usize, // overflow pages
        1..=3 => 300 + rng.below(600) as usize,
        _ => rng.below(80) as usize,
    };
    let seed = rng.next_u64();
    let mut v = Vec::with_capacity(20 + len);
    v.extend_from_slice(&balance.to_le_bytes());
    v.extend_from_slice(&seed.to_le_bytes());
    v.extend_from_slice(&(len as u32).to_le_bytes());
    v.extend_from_slice(&Rng::new(seed).bytes(len));
    v
}

fn check_account(v: &[u8]) -> u64 {
    let balance = u64::from_le_bytes(v[0..8].try_into().unwrap());
    let seed = u64::from_le_bytes(v[8..16].try_into().unwrap());
    let len = u32::from_le_bytes(v[16..20].try_into().unwrap()) as usize;
    assert_eq!(v.len(), 20 + len, "account value has the wrong length");
    assert_eq!(
        v[20..],
        Rng::new(seed).bytes(len)[..],
        "account padding damaged"
    );
    balance
}

const ACCOUNTS: u64 = 300;
const START: u64 = 1000;

fn total(r: &mut ReadTx<'_>) -> (u64, usize) {
    let all = r.scan("acct", b"", None, usize::MAX).unwrap();
    (all.iter().map(|(_, v)| check_account(v)).sum(), all.len())
}

/// Money moves between accounts in every commit, values change size, junk
/// keys come and go so pages are freed and reused constantly. Reader threads
/// hold snapshots for random times and check the total never changes and
/// every value is intact, twice per snapshot.
#[test]
fn readers_see_consistent_snapshots_under_churn() {
    let seed = seed("snapshot_churn");
    let secs = env_u64("KVSQL_CHURN_SECS", 3);
    let db = sim_db(48); // a small cache so reads go to the disk often
    {
        let mut rng = Rng::new(seed);
        let mut tx = db.begin_write().unwrap();
        for a in 0..ACCOUNTS {
            tx.put(
                "acct",
                format!("{a:05}").as_bytes(),
                &account_value(START, &mut rng),
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let stop = AtomicBool::new(false);
    let commits = AtomicU64::new(0);
    let checks = AtomicU64::new(0);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut rng = Rng::new(seed ^ 1);
            let deadline = Instant::now() + Duration::from_secs(secs);
            while Instant::now() < deadline {
                let mut tx = db.begin_write().unwrap();
                for _ in 0..1 + rng.below(4) {
                    let a = format!("{:05}", rng.below(ACCOUNTS));
                    let b = format!("{:05}", rng.below(ACCOUNTS));
                    if a == b {
                        continue;
                    }
                    let ba = check_account(&tx.get("acct", a.as_bytes()).unwrap().unwrap());
                    let bb = check_account(&tx.get("acct", b.as_bytes()).unwrap().unwrap());
                    let amt = rng.below(ba + 1);
                    tx.put("acct", a.as_bytes(), &account_value(ba - amt, &mut rng))
                        .unwrap();
                    tx.put("acct", b.as_bytes(), &account_value(bb + amt, &mut rng))
                        .unwrap();
                }
                for _ in 0..rng.below(30) {
                    let k = key(&mut rng, 3000);
                    if rng.chance(55) {
                        tx.put("junk", &k, &value(&mut rng)).unwrap();
                    } else {
                        tx.delete("junk", &k).unwrap();
                    }
                }
                if rng.chance(2) {
                    tx.drop_keyspace("junk").unwrap();
                }
                if rng.chance(5) {
                    drop(tx);
                } else {
                    tx.commit().unwrap();
                    commits.fetch_add(1, Ordering::Relaxed);
                }
            }
            stop.store(true, Ordering::Relaxed);
        });
        for t in 0..4u64 {
            let (db, stop, checks) = (&db, &stop, &checks);
            s.spawn(move || {
                let mut rng = Rng::new(seed ^ (100 + t));
                while !stop.load(Ordering::Relaxed) {
                    let mut r = db.begin_read().unwrap();
                    let first = total(&mut r);
                    assert_eq!(first, (ACCOUNTS * START, ACCOUNTS as usize), "seed {seed}");
                    std::thread::sleep(Duration::from_micros(rng.below(20_000)));
                    // Same snapshot, read again after the writer moved on.
                    assert_eq!(total(&mut r), first, "seed {seed}: snapshot changed");
                    let junk = r.scan("junk", b"", None, usize::MAX).unwrap();
                    assert_eq!(r.scan("junk", b"", None, usize::MAX).unwrap(), junk);
                    checks.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    let report = db.verify().unwrap();
    eprintln!(
        "snapshot_churn: {} commits, {} reader checks, {report:?}",
        commits.load(Ordering::Relaxed),
        checks.load(Ordering::Relaxed)
    );
    assert!(commits.load(Ordering::Relaxed) > 50);
    assert!(checks.load(Ordering::Relaxed) > 50);
}
