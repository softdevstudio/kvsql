//! A real process writing to a real file, killed at random moments.
//!
//! The child commits transaction after transaction and prints `ack N` only
//! after commit N returns. The parent kills it, reopens the file and checks:
//! the file verifies, every acknowledged commit is there, and the newest
//! commit is complete (all of its keys present with the right values).

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use common::*;
use kvsql_engine::sim::Rng;
use kvsql_engine::Db;

const CHILD_ENV: &str = "KVSQL_KILL_CHILD_PATH";

/// Deterministic contents of commit `n`: three keys, one sometimes large.
fn commit_values(n: u64) -> Vec<(String, Vec<u8>)> {
    let mut rng = Rng::new(n);
    let big = if n.is_multiple_of(7) { 6000 } else { 40 };
    vec![
        (format!("log/{n:09}"), rng.bytes(big)),
        (
            format!("idx/{:09}", n.wrapping_mul(2_654_435_761) % 1_000_000_007),
            n.to_le_bytes().to_vec(),
        ),
        ("last".to_string(), n.to_le_bytes().to_vec()),
    ]
}

/// Runs only inside the child process the parent starts.
#[test]
fn child_writer() {
    let Ok(path) = std::env::var(CHILD_ENV) else {
        return;
    };
    let db = Db::open(&path).unwrap();
    let mut n = db
        .get("kv", b"last")
        .unwrap()
        .map(|v| u64::from_le_bytes(v.try_into().unwrap()))
        .unwrap_or(0);
    loop {
        n += 1;
        let mut tx = db.begin_write().unwrap();
        for (k, v) in commit_values(n) {
            tx.put("kv", k.as_bytes(), &v).unwrap();
        }
        // Old log entries are deleted so pages get freed and reused.
        if n > 50 {
            tx.delete("kv", format!("log/{:09}", n - 50).as_bytes())
                .unwrap();
        }
        tx.commit().unwrap();
        println!("ack {n}");
    }
}

#[test]
fn sigkill_never_loses_an_acknowledged_commit() {
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    let seed = seed("sigkill");
    let mut rng = Rng::new(seed);
    let rounds = env_u64("KVSQL_KILL_ROUNDS", 40);
    let dir = temp_dir("kill");
    let path = dir.join("db.kvsql");
    let exe = std::env::current_exe().unwrap();
    let mut last_acked = 0;

    for round in 0..rounds {
        let mut child = Command::new(&exe)
            .args(["--exact", "child_writer", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, &path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let (txs, rxs) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if let Some(n) = line.strip_prefix("ack ") {
                    if txs.send(n.trim().parse::<u64>().unwrap()).is_err() {
                        break;
                    }
                }
            }
        });
        // Wait for the child to make progress, then kill it at a random moment.
        let first = rxs.recv_timeout(Duration::from_secs(30));
        assert!(first.is_ok(), "round {round}: child made no progress");
        std::thread::sleep(Duration::from_millis(rng.below(60)));
        child.kill().unwrap();
        child.wait().unwrap();
        reader.join().unwrap();
        let acked = rxs.try_iter().last().unwrap_or(first.unwrap());
        last_acked = acked;

        let db = Db::open(&path).unwrap();
        db.verify()
            .unwrap_or_else(|e| panic!("seed {seed} round {round}: {e}"));
        let last = db
            .get("kv", b"last")
            .unwrap()
            .map(|v| u64::from_le_bytes(v.try_into().unwrap()))
            .unwrap();
        assert!(
            last >= acked,
            "seed {seed} round {round}: acknowledged commit {acked} lost, file has {last}"
        );
        for n in last.saturating_sub(49).max(1)..=last {
            for (k, v) in commit_values(n) {
                if k == "last" {
                    continue;
                }
                assert_eq!(
                    db.get("kv", k.as_bytes()).unwrap().as_deref(),
                    Some(v.as_slice()),
                    "seed {seed} round {round}: commit {n} key {k}"
                );
            }
        }
        assert_eq!(
            db.get("kv", format!("log/{:09}", last + 1).as_bytes())
                .unwrap(),
            None,
            "seed {seed} round {round}: part of commit {} is visible",
            last + 1
        );
    }
    eprintln!("sigkill: {rounds} kills, {last_acked} commits acknowledged");
    std::fs::remove_dir_all(dir).unwrap();
}
