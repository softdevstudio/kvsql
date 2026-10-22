#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use kvsql_engine::sim::Rng;
use kvsql_engine::{Db, ReadTx};

pub type Model = BTreeMap<(String, Vec<u8>), Vec<u8>>;

/// Everything in the database, keyed by (keyspace, key).
pub fn dump(db: &Db) -> Model {
    let mut out = Model::new();
    for ks in db.keyspaces().unwrap() {
        for (k, v) in db.scan(&ks, b"", None, usize::MAX).unwrap() {
            out.insert((ks.clone(), k), v);
        }
    }
    out
}

/// Everything a read transaction sees.
pub fn dump_read(r: &mut ReadTx<'_>) -> Model {
    let mut out = Model::new();
    for ks in r.keyspaces().unwrap() {
        for (k, v) in r.scan(&ks, b"", None, usize::MAX).unwrap() {
            out.insert((ks.clone(), k), v);
        }
    }
    out
}

/// A random value: mostly short, sometimes long enough to need overflow pages.
pub fn value(rng: &mut Rng) -> Vec<u8> {
    let len = match rng.below(20) {
        0 => 1000 + rng.below(12000) as usize,
        1 => 0,
        2..=4 => 200 + rng.below(800) as usize,
        _ => rng.below(64) as usize,
    };
    rng.bytes(len)
}

/// A key from a small space, so updates and deletes hit existing keys.
pub fn key(rng: &mut Rng, space: u64) -> Vec<u8> {
    let n = rng.below(space);
    let mut k = format!("k{n:06}").into_bytes();
    if rng.chance(10) {
        // Occasionally a long key, to exercise big cells and separators.
        k.extend(std::iter::repeat_n(b'x', rng.below(400) as usize));
    }
    k
}

pub const KEYSPACES: [&str; 3] = ["alpha", "beta", "gamma"];

pub fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "kvsql-test-{tag}-{}-{}",
        std::process::id(),
        Rng::new(nanos()).next_u64()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

pub fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Seed from KVSQL_SEED, or the clock. Printed so a failure can be replayed.
pub fn seed(test: &str) -> u64 {
    let s = env_u64("KVSQL_SEED", nanos());
    eprintln!("{test}: seed {s} (rerun with KVSQL_SEED={s})");
    s
}
