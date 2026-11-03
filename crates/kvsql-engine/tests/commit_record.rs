//! Format version 2: one sync per commit, checked by the commit record when
//! the file is opened. These tests damage a real file the way a crash
//! between writing and syncing can, then reopen it. Unix only, for the
//! positional file I/O; the power-cut tests cover every platform.
#![cfg(unix)]

mod common;

use common::*;
use kvsql_engine::sql::Value;
use kvsql_engine::Db;
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::path::Path;

const PAGE: u64 = 4096;
const HEADER: usize = 16;

fn read_page(path: &Path, id: u64) -> Vec<u8> {
    let f = std::fs::File::open(path).unwrap();
    let mut buf = vec![0u8; PAGE as usize];
    f.read_exact_at(&mut buf, id * PAGE).unwrap();
    buf
}

fn write_page(path: &Path, id: u64, page: &[u8]) {
    let f = OpenOptions::new().write(true).open(path).unwrap();
    f.write_all_at(page, id * PAGE).unwrap();
}

fn u64_at(p: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(p[at..at + 8].try_into().unwrap())
}

fn u32_at(p: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(p[at..at + 4].try_into().unwrap())
}

/// The newest meta page: (slot, version, txid, record head).
fn newest_meta(path: &Path) -> (u64, u32, u64, u64) {
    let mut best = None;
    for slot in 0..2 {
        let p = read_page(path, slot);
        if &p[HEADER..HEADER + 8] != b"KVSQLDB\0" {
            continue;
        }
        let txid = u64_at(&p, HEADER + 16);
        let m = (slot, u32_at(&p, HEADER + 8), txid, u64_at(&p, HEADER + 56));
        if best.is_none_or(|b: (u64, u32, u64, u64)| txid > b.2) {
            best = Some(m);
        }
    }
    best.unwrap()
}

/// The pages the newest commit's record lists.
fn recorded_pages(path: &Path) -> Vec<u64> {
    let (_, version, _, mut rec) = newest_meta(path);
    assert_eq!(version, 2);
    let mut out = Vec::new();
    while rec != 0 {
        let p = read_page(path, rec);
        assert_eq!(p[4], 6, "page {rec} is a record page");
        let n = u32_at(&p, HEADER + 8) as usize;
        for j in 0..n {
            out.push(u64_at(&p, HEADER + 20 + 12 * j));
        }
        rec = u64_at(&p, HEADER);
    }
    out
}

fn two_commits(tag: &str) -> std::path::PathBuf {
    let path = temp_dir(tag).join("db.kvsql");
    let db = Db::open(&path).unwrap();
    db.put("k", b"a", b"first").unwrap();
    db.put("k", b"b", b"second").unwrap();
    drop(db);
    path
}

fn check_falls_back(path: &Path) {
    let db = Db::open(path).unwrap();
    db.verify().unwrap();
    assert_eq!(db.get("k", b"a").unwrap(), Some(b"first".to_vec()));
    assert_eq!(
        db.get("k", b"b").unwrap(),
        None,
        "the unsynced commit is gone"
    );
    // The file goes on working: write, reopen, read.
    db.put("k", b"c", b"third").unwrap();
    drop(db);
    let db = Db::open(path).unwrap();
    db.verify().unwrap();
    assert_eq!(db.get("k", b"c").unwrap(), Some(b"third".to_vec()));
    assert_eq!(db.get("k", b"b").unwrap(), None);
}

#[test]
fn every_commit_has_a_record_and_opens_whole() {
    let path = two_commits("record_whole");
    let pages = recorded_pages(&path);
    assert!(!pages.is_empty());
    let db = Db::open(&path).unwrap();
    db.verify().unwrap();
    assert_eq!(db.get("k", b"b").unwrap(), Some(b"second".to_vec()));
}

#[test]
fn meta_on_disk_but_a_data_page_missing_falls_back() {
    // Every page the commit wrote, one at a time: as if the meta page
    // reached the disk and that page did not.
    let probe = two_commits("record_probe");
    let n = recorded_pages(&probe).len();
    for i in 0..n {
        let path = two_commits(&format!("record_missing_{i}"));
        let victim = recorded_pages(&path)[i];
        write_page(&path, victim, &vec![0u8; PAGE as usize]);
        check_falls_back(&path);
    }
}

#[test]
fn a_stale_page_with_a_valid_seal_falls_back() {
    // The old contents of a page are still a well-formed page, just not the
    // one this commit wrote. The recorded CRC tells them apart.
    let path = two_commits("record_stale");
    let pages = recorded_pages(&path);
    let (_, _, txid, _) = newest_meta(&path);
    // Flip one byte of a recorded page and reseal it, so the page checks
    // out on its own but is not the page this commit wrote.
    let victim = pages[0];
    let mut p = read_page(&path, victim);
    let last = p.len() - 1;
    p[last] ^= 0x55;
    let crc = crc32c(&p[4..]);
    p[0..4].copy_from_slice(&crc.to_le_bytes());
    write_page(&path, victim, &p);
    assert_eq!(newest_meta(&path).2, txid);
    check_falls_back(&path);
}

#[test]
fn a_torn_record_page_falls_back() {
    let path = two_commits("record_torn");
    let (_, _, _, rec) = newest_meta(&path);
    let mut p = read_page(&path, rec);
    // The first sector arrived, the rest kept older bytes: here, the
    // entries past the first sector's header are lost.
    for b in &mut p[HEADER + 8..512] {
        *b = 0;
    }
    write_page(&path, rec, &p);
    check_falls_back(&path);
}

#[test]
fn a_record_from_an_older_commit_is_not_accepted() {
    // The meta page reached the disk, the record page did not, and the
    // record page's old contents are the record of an earlier commit.
    let path = temp_dir("record_old").join("db.kvsql");
    let db = Db::open(&path).unwrap();
    db.put("k", b"a", b"first").unwrap();
    drop(db);
    let (_, _, _, old_rec) = newest_meta(&path);
    let old = read_page(&path, old_rec);
    let db = Db::open(&path).unwrap();
    db.put("k", b"b", b"second").unwrap();
    drop(db);
    let (_, _, _, new_rec) = newest_meta(&path);
    // Put the old record's body where the new record lives, resealed for
    // that page number, as a reused page would look.
    let mut p = old.clone();
    p[8..16].copy_from_slice(&new_rec.to_le_bytes());
    let crc = crc32c(&p[4..]);
    p[0..4].copy_from_slice(&crc.to_le_bytes());
    write_page(&path, new_rec, &p);
    check_falls_back(&path);
}

#[test]
fn a_version_1_file_from_0_1_opens_and_upgrades() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1-from-0.1.0.kvsql");
    let path = temp_dir("record_v1").join("v1.kvsql");
    std::fs::copy(&fixture, &path).unwrap();
    assert_eq!(newest_meta(&path).1, 1);
    let rows = |db: &Db| {
        db.execute("SELECT id, name, city FROM users ORDER BY id", &[])
            .unwrap()
            .rows
    };
    let want = |extra: bool| {
        let mut v = vec![
            vec![
                Value::Int(1),
                Value::Text("Ada".into()),
                Value::Text("Rome".into()),
            ],
            vec![
                Value::Int(2),
                Value::Text("Bo".into()),
                Value::Text("Lima".into()),
            ],
            vec![
                Value::Int(3),
                Value::Text("Cy".into()),
                Value::Text("Rome".into()),
            ],
        ];
        if extra {
            v.push(vec![
                Value::Int(4),
                Value::Text("Di".into()),
                Value::Text("Rome".into()),
            ]);
        }
        v
    };
    let db = Db::open(&path).unwrap();
    db.verify().unwrap();
    assert_eq!(rows(&db), want(false));
    db.execute("INSERT INTO users VALUES (4, 'Di', 'Rome')", &[])
        .unwrap();
    drop(db);
    assert_eq!(newest_meta(&path).1, 2, "the first commit writes version 2");
    // No version 1 meta page is left for KvSQL 0.1 to open in its place.
    for slot in 0..2 {
        let p = read_page(&path, slot);
        let is_v1 = &p[HEADER..HEADER + 8] == b"KVSQLDB\0" && u32_at(&p, HEADER + 8) == 1;
        assert!(!is_v1, "slot {slot} still holds a version 1 meta page");
    }
    let db = Db::open(&path).unwrap();
    db.verify().unwrap();
    assert_eq!(rows(&db), want(true));
    // The index still answers through the new commit.
    let r = db
        .execute("SELECT count(*) FROM users WHERE city = 'Rome'", &[])
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(3)]]);
}

#[test]
fn record_pages_are_accounted_and_reused() {
    // Many commits in a row: verify accounts for every page, and the file
    // does not grow without bound from record pages.
    let path = temp_dir("record_reuse").join("db.kvsql");
    let db = Db::open(&path).unwrap();
    for i in 0..300u32 {
        db.put("k", &(i % 20).to_be_bytes(), &i.to_be_bytes())
            .unwrap();
        if i % 50 == 0 {
            db.verify().unwrap();
        }
    }
    let r = db.verify().unwrap();
    drop(db);
    assert!(
        r.page_count < 40,
        "{} pages after 300 small commits",
        r.page_count
    );
    let db = Db::open(&path).unwrap();
    db.verify().unwrap();
}

/// CRC-32C, bit by bit; tests only.
fn crc32c(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0x82F6_3B78
            } else {
                c >> 1
            };
        }
    }
    !c
}
