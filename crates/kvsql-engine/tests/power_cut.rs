//! Power cuts at every possible point of a commit.
//!
//! Each round opens the database from a simulated disk, runs a few
//! transactions with the power set to fail after a random number of writes
//! and syncs, then builds the disk a power cut could leave behind: unsynced
//! writes dropped, reordered or torn at 512-byte sectors. The reopened file
//! must pass a full verify and hold exactly the last acknowledged commit, or
//! the commit that was in flight when the power went.

mod common;

use common::*;
use kvsql_engine::sim::{Rng, SimStorage};
use kvsql_engine::{Db, Error, Options};

fn apply_txn(rng: &mut Rng, tx: &mut kvsql_engine::WriteTx<'_>, model: &mut Model) {
    let ops = 1 + rng.below(40);
    for _ in 0..ops {
        let ks = KEYSPACES[rng.below(3) as usize];
        match rng.below(100) {
            0..=64 => {
                let k = key(rng, 600);
                let v = value(rng);
                tx.put(ks, &k, &v).unwrap();
                model.insert((ks.to_string(), k), v);
            }
            65..=97 => {
                let k = key(rng, 600);
                tx.delete(ks, &k).unwrap();
                model.remove(&(ks.to_string(), k));
            }
            _ => {
                tx.drop_keyspace(ks).unwrap();
                model.retain(|(kks, _), _| kks != ks);
            }
        }
    }
}

#[test]
fn power_cut_never_loses_or_tears_a_commit() {
    let seed = seed("power_cut");
    let mut rng = Rng::new(seed);
    let rounds = env_u64("KVSQL_POWER_CUTS", 1500);
    let mut image: Vec<u8> = Vec::new();
    let mut model = Model::new();
    let (mut landed_in_flight, mut lost_in_flight, mut mid_commit) = (0u64, 0u64, 0u64);
    let mut reader_checks = 0u64;

    for round in 0..rounds {
        let ctx = format!("seed {seed} round {round}");
        let storage = SimStorage::from_image(image.clone());
        let db = Db::open_with(Box::new(storage.clone()), Options { cache_pages: 32 })
            .unwrap_or_else(|e| panic!("{ctx}: reopen failed: {e}"));
        db.verify()
            .unwrap_or_else(|e| panic!("{ctx}: verify after power cut: {e}"));
        assert_eq!(dump(&db), model, "{ctx}: contents after power cut");

        // Half the rounds keep a reader open across the commits, so freed
        // pages it can still reach must stay untouched.
        let mut reader = rng
            .chance(50)
            .then(|| (db.begin_read().unwrap(), model.clone()));

        // Power fails after 0..24 more writes or syncs, or not at all.
        // Consecutive pages go out in one write, so a commit is a handful
        // of writes and two syncs.
        if rng.chance(85) {
            storage.fail_after(rng.below(24));
        }
        let mut in_flight: Option<Model> = None;
        for _ in 0..1 + rng.below(4) {
            let mut next = model.clone();
            let mut tx = match db.begin_write() {
                Ok(tx) => tx,
                Err(Error::Poisoned) => break,
                Err(e) => panic!("{ctx}: {e}"),
            };
            apply_txn(&mut rng, &mut tx, &mut next);
            match tx.commit() {
                Ok(()) => {
                    model = next;
                    if let Some((r, seen)) = reader.as_mut() {
                        assert_eq!(&dump_read(r), seen, "{ctx}: open reader's view changed");
                        reader_checks += 1;
                    }
                    if rng.chance(30) {
                        reader = Some((db.begin_read().unwrap(), model.clone()));
                    }
                }
                Err(Error::Io(_)) => {
                    in_flight = Some(next);
                    mid_commit += 1;
                    break;
                }
                Err(e) => panic!("{ctx}: commit: {e}"),
            }
        }
        // After a failed commit the handle refuses further work.
        if in_flight.is_some() {
            assert!(matches!(db.get("alpha", b"x"), Err(Error::Poisoned)));
        }
        drop(reader);
        drop(db);
        image = storage.crash_image(&mut rng);

        // Decide what the next round must find.
        if let Some(next) = in_flight {
            let check = Db::open_with(
                Box::new(SimStorage::from_image(image.clone())),
                Options::default(),
            )
            .unwrap_or_else(|e| panic!("{ctx}: reopen failed: {e}"));
            let got = dump(&check);
            if got == next {
                model = next;
                landed_in_flight += 1;
            } else {
                assert_eq!(
                    got, model,
                    "{ctx}: holds neither the last commit nor the one in flight"
                );
                lost_in_flight += 1;
            }
        }
    }
    eprintln!(
        "power_cut: {rounds} rounds, {mid_commit} cuts inside a commit \
         ({landed_in_flight} in-flight commits landed whole, {lost_in_flight} rolled back whole), \
         {reader_checks} checks of a reader held across commits"
    );
    assert!(
        mid_commit > rounds / 4,
        "too few cuts landed inside a commit"
    );
    assert!(landed_in_flight > 0 && lost_in_flight > 0);
    assert!(reader_checks > 0);
}

#[test]
fn power_cut_while_creating_the_file() {
    for n in 0..4 {
        let s = SimStorage::new();
        s.fail_after(n);
        let first = Db::open_with(Box::new(s.clone()), Options::default());
        let img = s.crash_image(&mut Rng::new(n));
        drop(first);
        let db = Db::open_with(Box::new(SimStorage::from_image(img)), Options::default())
            .unwrap_or_else(|e| panic!("fail_after {n}: {e}"));
        db.verify().unwrap();
        db.put("t", b"k", b"v").unwrap();
        assert_eq!(db.get("t", b"k").unwrap(), Some(b"v".to_vec()));
    }
}

/// SQL writes with indexes, cut at random points. After every cut the file
/// verifies (rows fit their tables, indexes match rows exactly) and holds
/// the state before the statement or the state after it.
#[test]
fn power_cut_during_sql_keeps_indexes_exact() {
    let seed = seed("sql_power_cut");
    let mut rng = Rng::new(seed);
    let rounds = env_u64("KVSQL_POWER_CUTS", 1500) / 3;
    let setup = SimStorage::new();
    let db = Db::open_with(Box::new(setup.clone()), Options::default()).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT);
         CREATE INDEX t_a ON t(a); CREATE INDEX t_b ON t(b)",
        &[],
    )
    .unwrap();
    drop(db);
    let mut image = setup.crash_image(&mut Rng::new(0));
    let dump = |db: &Db| db.execute("SELECT * FROM t ORDER BY id", &[]).unwrap().rows;
    let (mut landed, mut lost, mut cut) = (0, 0, 0);
    for round in 0..rounds {
        let ctx = format!("seed {seed} round {round}");
        let stmt = match rng.below(4) {
            0 | 1 => {
                let rows: Vec<String> = (0..1 + rng.below(30))
                    .map(|_| {
                        format!(
                            "({}, {}, '{}')",
                            rng.below(400),
                            rng.below(20),
                            "v".repeat(rng.below(300) as usize)
                        )
                    })
                    .collect();
                format!("INSERT INTO t VALUES {}", rows.join(", "))
            }
            2 => format!(
                "UPDATE t SET a = a + 1, b = b || 'x' WHERE id % {} = 0",
                2 + rng.below(5)
            ),
            _ => format!("DELETE FROM t WHERE a = {}", rng.below(20)),
        };
        // What the statement does with no failure.
        let clean = Db::open_with(
            Box::new(SimStorage::from_image(image.clone())),
            Options::default(),
        )
        .unwrap();
        let before = dump(&clean);
        let ok = clean.execute(&stmt, &[]).is_ok();
        let after = dump(&clean);
        drop(clean);

        let storage = SimStorage::from_image(image.clone());
        let db = Db::open_with(Box::new(storage.clone()), Options::default()).unwrap();
        storage.fail_after(rng.below(12));
        let r = db.execute(&stmt, &[]);
        drop(db);
        if matches!(r, Err(Error::Io(_))) {
            cut += 1;
        }
        image = storage.crash_image(&mut rng);
        let db = Db::open_with(
            Box::new(SimStorage::from_image(image.clone())),
            Options::default(),
        )
        .unwrap_or_else(|e| panic!("{ctx}: reopen: {e}"));
        db.verify().unwrap_or_else(|e| panic!("{ctx}: verify: {e}"));
        let got = dump(&db);
        if ok && got == after && after != before {
            landed += 1;
        } else {
            assert_eq!(got, before, "{ctx}: {stmt}: neither before nor after");
            lost += 1;
        }
    }
    eprintln!("sql_power_cut: {rounds} statements, {cut} cut mid-commit, {landed} landed whole, {lost} rolled back or failed");
    assert!(cut > 0 && landed > 0);
}
