use kvsql_engine::{sim::SimStorage, Db, Options};
use std::time::Instant;
fn main() {
    let db = Db::open_with(Box::new(SimStorage::new()), Options::default()).unwrap();
    let n = 60_000u32;
    let key = |i: u32| format!("key-{:08}", i.wrapping_mul(2_654_435_761));
    let t = Instant::now();
    let mut tx = db.begin_write().unwrap();
    for i in 0..n {
        tx.put("t", key(i).as_bytes(), &i.to_le_bytes()).unwrap();
    }
    tx.commit().unwrap();
    println!("insert {:?}/op", t.elapsed() / n);
    let t = Instant::now();
    for i in 0..n {
        db.get("t", key(i).as_bytes()).unwrap().unwrap();
    }
    println!("get {:?}/op", t.elapsed() / n);
}
