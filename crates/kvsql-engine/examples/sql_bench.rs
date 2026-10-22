use kvsql_engine::{sim::SimStorage, sql::Value, Db, Options};
use std::time::Instant;
fn main() {
    let db = Db::open_with(Box::new(SimStorage::new()), Options::default()).unwrap();
    db.execute(
        "CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)",
        &[],
    )
    .unwrap();
    let n = 50_000i64;
    let t = Instant::now();
    let mut tx = db.begin_write().unwrap();
    for i in 0..n {
        tx.put_row(
            "u",
            &[
                Value::Int(i),
                Value::Text(format!("user{i}")),
                Value::Int(i % 90),
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    println!("put_row {:?}/op", t.elapsed() / n as u32);
    let t = Instant::now();
    for i in 0..n {
        db.get_row("u", &Value::Int(i)).unwrap().unwrap();
    }
    println!("get_row {:?}/op", t.elapsed() / n as u32);
    let t = Instant::now();
    for i in 0..n {
        db.execute("SELECT name FROM u WHERE id = ?", &[Value::Int(i)])
            .unwrap();
    }
    println!("SQL point select {:?}/op", t.elapsed() / n as u32);
    let t = Instant::now();
    let r = db
        .execute("SELECT age, count(*) FROM u GROUP BY age", &[])
        .unwrap();
    println!(
        "group by over {n} rows: {:?} ({} groups)",
        t.elapsed(),
        r.rows.len()
    );
}
