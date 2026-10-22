//! Every KV operation against an in-memory model, plus the edges.

mod common;

use common::*;
use kvsql_engine::sim::{Rng, SimStorage};
use kvsql_engine::{Db, Error, Options, MAX_KEY_LEN};

fn sim_db() -> (Db, SimStorage) {
    let s = SimStorage::new();
    let db = Db::open_with(Box::new(s.clone()), Options::default()).unwrap();
    (db, s)
}

#[test]
fn put_get_delete_scan() {
    let (db, _) = sim_db();
    assert_eq!(db.get("t", b"a").unwrap(), None);
    db.put("t", b"a", b"1").unwrap();
    db.put("t", b"b", b"2").unwrap();
    db.put("t", b"c", b"3").unwrap();
    assert_eq!(db.get("t", b"b").unwrap(), Some(b"2".to_vec()));
    db.put("t", b"b", b"two").unwrap();
    assert_eq!(db.get("t", b"b").unwrap(), Some(b"two".to_vec()));
    assert_eq!(
        db.scan("t", b"b", Some(b"c"), 10).unwrap(),
        vec![(b"b".to_vec(), b"two".to_vec())]
    );
    assert_eq!(db.scan("t", b"", None, 2).unwrap().len(), 2);
    assert!(db.delete("t", b"a").unwrap());
    assert!(!db.delete("t", b"a").unwrap());
    assert!(!db.delete("missing", b"a").unwrap());
    assert_eq!(db.get("t", b"a").unwrap(), None);
    assert_eq!(db.keyspaces().unwrap(), vec!["t".to_string()]);
    db.verify().unwrap();
}

#[test]
fn keyspaces_are_separate() {
    let (db, _) = sim_db();
    db.put("a", b"k", b"in a").unwrap();
    db.put("b", b"k", b"in b").unwrap();
    assert_eq!(db.get("a", b"k").unwrap(), Some(b"in a".to_vec()));
    assert_eq!(db.get("b", b"k").unwrap(), Some(b"in b".to_vec()));
    let mut tx = db.begin_write().unwrap();
    assert!(tx.drop_keyspace("a").unwrap());
    assert!(!tx.drop_keyspace("nope").unwrap());
    tx.commit().unwrap();
    assert_eq!(db.get("a", b"k").unwrap(), None);
    assert_eq!(db.keyspaces().unwrap(), vec!["b".to_string()]);
    db.verify().unwrap();
}

#[test]
fn rejects_bad_input() {
    let (db, _) = sim_db();
    assert!(matches!(db.put("t", b"", b"v"), Err(Error::KeyTooLarge(0))));
    let long = vec![b'k'; MAX_KEY_LEN + 1];
    assert!(matches!(
        db.put("t", &long, b"v"),
        Err(Error::KeyTooLarge(_))
    ));
    db.put("t", &long[..MAX_KEY_LEN], b"ok").unwrap();
    assert!(matches!(
        db.put("", b"k", b"v"),
        Err(Error::BadKeyspaceName)
    ));
    db.verify().unwrap();
}

#[test]
fn large_values_round_trip() {
    let (db, _) = sim_db();
    let mut rng = Rng::new(7);
    for len in [0usize, 1, 992, 993, 4000, 4068, 4069, 100_000, 1_000_000] {
        let v = rng.bytes(len);
        db.put("big", format!("v{len}").as_bytes(), &v).unwrap();
        assert_eq!(
            db.get("big", format!("v{len}").as_bytes()).unwrap(),
            Some(v)
        );
    }
    // Replacing and deleting large values must hand their pages back.
    let before = db.verify().unwrap();
    db.put("big", b"v1000000", b"small now").unwrap();
    db.delete("big", b"v100000").unwrap();
    let after = db.verify().unwrap();
    assert!(after.free_pages > before.free_pages);
}

#[test]
fn dropped_transaction_changes_nothing() {
    let (db, s) = sim_db();
    db.put("t", b"keep", b"1").unwrap();
    let writes = s.counts().0;
    {
        let mut tx = db.begin_write().unwrap();
        tx.put("t", b"gone", b"2").unwrap();
        tx.delete("t", b"keep").unwrap();
        assert_eq!(tx.get("t", b"gone").unwrap(), Some(b"2".to_vec()));
        assert_eq!(tx.get("t", b"keep").unwrap(), None);
    }
    assert_eq!(
        s.counts().0,
        writes,
        "an uncommitted transaction must not write"
    );
    assert_eq!(db.get("t", b"keep").unwrap(), Some(b"1".to_vec()));
    assert_eq!(db.get("t", b"gone").unwrap(), None);
    db.verify().unwrap();
}

#[test]
fn transaction_sees_its_own_writes_in_scans() {
    let (db, _) = sim_db();
    let mut tx = db.begin_write().unwrap();
    for i in 0..500u32 {
        tx.put("t", format!("{i:05}").as_bytes(), &i.to_le_bytes())
            .unwrap();
    }
    assert_eq!(
        tx.scan("t", b"00100", Some(b"00200"), 1000).unwrap().len(),
        100
    );
    tx.commit().unwrap();
    assert_eq!(db.scan("t", b"", None, usize::MAX).unwrap().len(), 500);
}

/// The main model test: random puts, deletes, drops and rolled-back
/// transactions, checked against a BTreeMap and verified after every commit,
/// with reopens from the durable image along the way.
#[test]
fn random_ops_match_model() {
    let seed = seed("random_ops_match_model");
    let mut rng = Rng::new(seed);
    let rounds = env_u64("KVSQL_MODEL_ROUNDS", 300);
    let mut storage = SimStorage::new();
    let mut db = Db::open_with(Box::new(storage.clone()), Options { cache_pages: 64 }).unwrap();
    let mut model = Model::new();
    for round in 0..rounds {
        let mut next = model.clone();
        let mut tx = db.begin_write().unwrap();
        let big = rng.chance(10);
        let ops = 1 + rng.below(if big { 400 } else { 30 });
        for _ in 0..ops {
            let ks = KEYSPACES[rng.below(3) as usize];
            match rng.below(100) {
                0..=59 => {
                    let k = key(&mut rng, 2000);
                    let v = value(&mut rng);
                    tx.put(ks, &k, &v).unwrap();
                    next.insert((ks.to_string(), k), v);
                }
                60..=94 => {
                    // Delete an existing key most of the time.
                    let k = match next.range((ks.to_string(), Vec::new())..).next() {
                        Some(((kks, kk), _)) if kks == ks && rng.chance(80) => kk.clone(),
                        _ => key(&mut rng, 2000),
                    };
                    let had = next.remove(&(ks.to_string(), k.clone())).is_some();
                    assert_eq!(tx.delete(ks, &k).unwrap(), had, "seed {seed} round {round}");
                }
                95..=97 => {
                    let k = key(&mut rng, 2000);
                    let want = next.get(&(ks.to_string(), k.clone())).cloned();
                    assert_eq!(tx.get(ks, &k).unwrap(), want, "seed {seed} round {round}");
                }
                _ if rng.chance(15) => {
                    // Rare, so trees get deep between drops.
                    tx.drop_keyspace(ks).unwrap();
                    next.retain(|(kks, _), _| kks != ks);
                }
                _ => {}
            }
        }
        if rng.chance(10) {
            drop(tx); // roll back
        } else {
            tx.commit().unwrap();
            model = next;
        }
        db.verify()
            .unwrap_or_else(|e| panic!("seed {seed} round {round}: {e}"));
        if rng.chance(5) {
            // Reopen from what is durable.
            drop(db);
            storage = SimStorage::from_image(storage.crash_image(&mut Rng::new(0)));
            db = Db::open_with(Box::new(storage.clone()), Options { cache_pages: 64 }).unwrap();
        }
        if round % 25 == 0 || round + 1 == rounds {
            assert_eq!(dump(&db), model, "seed {seed} round {round}");
            // Spot-check range scans against the model.
            let ks = KEYSPACES[rng.below(3) as usize];
            let a = key(&mut rng, 2000);
            let b = key(&mut rng, 2000);
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            let want: Vec<_> = model
                .range((ks.to_string(), lo.clone())..(ks.to_string(), hi.clone()))
                .map(|((_, k), v)| (k.clone(), v.clone()))
                .collect();
            assert_eq!(db.scan(ks, &lo, Some(&hi), usize::MAX).unwrap(), want);
        }
    }
    let report = db.verify().unwrap();
    eprintln!("final: {report:?}");
}

#[test]
fn file_is_reused_not_grown_forever() {
    let (db, _) = sim_db();
    let v = vec![7u8; 300];
    for round in 0..50 {
        let mut tx = db.begin_write().unwrap();
        for i in 0..200u32 {
            tx.put("t", format!("{i:05}").as_bytes(), &v).unwrap();
        }
        tx.commit().unwrap();
        let mut tx = db.begin_write().unwrap();
        for i in 0..200u32 {
            tx.delete("t", format!("{i:05}").as_bytes()).unwrap();
        }
        tx.commit().unwrap();
        if round == 5 {
            let r = db.verify().unwrap();
            assert!(r.page_count < 100, "{r:?}");
        }
    }
    let r = db.verify().unwrap();
    assert!(r.page_count < 100, "file keeps growing: {r:?}");
}

#[test]
fn real_file_persists_and_locks() {
    let dir = temp_dir("file");
    let path = dir.join("db.kvsql");
    {
        let db = Db::open(&path).unwrap();
        db.put("t", b"k", b"v").unwrap();
        // A second handle on the same file must be refused.
        assert!(matches!(Db::open(&path), Err(Error::Locked)));
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(db.get("t", b"k").unwrap(), Some(b"v".to_vec()));
    db.verify().unwrap();
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn rejects_files_that_are_not_kvsql() {
    let s = SimStorage::from_image(vec![0xAB; 3 * 4096]);
    assert!(matches!(
        Db::open_with(Box::new(s), Options::default()),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn damaged_page_is_reported_not_returned() {
    let s = SimStorage::new();
    let db = Db::open_with(Box::new(s.clone()), Options { cache_pages: 0 }).unwrap();
    for i in 0..100u32 {
        db.put("t", format!("{i:04}").as_bytes(), b"value").unwrap();
    }
    drop(db);
    let mut img = s.crash_image(&mut Rng::new(1));
    // Flip one byte in every page after the meta pages.
    for p in 2..img.len() / 4096 {
        img[p * 4096 + 2000] ^= 0x40;
    }
    let db = Db::open_with(
        Box::new(SimStorage::from_image(img)),
        Options { cache_pages: 0 },
    );
    match db {
        Err(Error::Corrupt(_)) => {}
        Ok(db) => {
            assert!(matches!(db.get("t", b"0001"), Err(Error::Corrupt(_))));
            assert!(matches!(db.verify(), Err(Error::Corrupt(_))));
        }
        Err(e) => panic!("unexpected {e}"),
    }
}

#[test]
fn deep_tree_grows_and_shrinks() {
    let (db, _) = sim_db();
    let key = |i: u32| {
        format!(
            "key-{:08}-{}",
            i.wrapping_mul(2_654_435_761),
            "p".repeat(40)
        )
    };
    let mut tx = db.begin_write().unwrap();
    for i in 0..60_000u32 {
        tx.put("t", key(i).as_bytes(), &i.to_le_bytes()).unwrap();
    }
    tx.commit().unwrap();
    let full = db.verify().unwrap();
    assert_eq!(full.keys, 60_000);
    let mut tx = db.begin_write().unwrap();
    for i in 0..60_000u32 {
        if i % 50 != 0 {
            assert!(tx.delete("t", key(i).as_bytes()).unwrap());
        }
    }
    tx.commit().unwrap();
    let thin = db.verify().unwrap();
    assert_eq!(thin.keys, 1_200);
    // Merging must give pages back: far fewer used pages than when full.
    assert!(thin.used_pages * 10 < full.used_pages, "{full:?} {thin:?}");
    for i in (0..60_000u32).step_by(50) {
        assert_eq!(
            db.get("t", key(i).as_bytes()).unwrap(),
            Some(i.to_le_bytes().to_vec())
        );
    }
}

/// Opening a file that is not a KvSQL database refuses it and leaves it as
/// it was, however small it is.
#[test]
fn foreign_files_are_left_alone() {
    let dir = std::env::temp_dir().join(format!("kvsql-foreign-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, body) in [
        ("notes.txt", b"shopping list\n".to_vec()),
        ("one-page.bin", vec![7u8; 4096]),
        ("big.bin", vec![1u8; 20_000]),
    ] {
        let p = dir.join(name);
        std::fs::write(&p, &body).unwrap();
        assert!(kvsql_engine::Db::open(&p).is_err(), "{name} opened");
        assert_eq!(std::fs::read(&p).unwrap(), body, "{name} changed");
    }
    // An empty file becomes a database.
    let p = dir.join("empty.db");
    std::fs::write(&p, b"").unwrap();
    let db = kvsql_engine::Db::open(&p).unwrap();
    db.put("k", b"a", b"1").unwrap();
    drop(db);
    assert_eq!(
        kvsql_engine::Db::open(&p).unwrap().get("k", b"a").unwrap(),
        Some(b"1".to_vec())
    );
    let _ = std::fs::remove_dir_all(&dir);
}
