//! KvSQL Engine: an embedded, single-file store.
//!
//! Data lives in named keyspaces, each an ordered map from byte keys to byte
//! values. Every change goes through a write transaction that commits all at
//! once: after a crash or power cut the file holds either the whole
//! transaction or none of it.
//!
//! ```
//! # fn main() -> kvsql_engine::Result<()> {
//! # let dir = std::env::temp_dir().join(format!("kvsql-doc-{}", std::process::id()));
//! # std::fs::create_dir_all(&dir).unwrap();
//! # let path = dir.join("demo.kvsql");
//! # let _ = std::fs::remove_file(&path);
//! use kvsql_engine::Db;
//!
//! let db = Db::open(&path)?;
//! let mut tx = db.begin_write()?;
//! tx.put("users", b"42", b"Ada")?;
//! tx.put("users", b"43", b"Grace")?;
//! tx.commit()?;
//!
//! assert_eq!(db.get("users", b"42")?, Some(b"Ada".to_vec()));
//! assert_eq!(db.scan("users", b"", None, 10)?.len(), 2);
//! # drop(db);
//! # std::fs::remove_dir_all(&dir).unwrap();
//! # Ok(())
//! # }
//! ```
//!
//! The same records answer SQL. A table is a keyspace; its rows are stored
//! under their primary keys, so `get_row` and SQL reach the same bytes:
//!
//! ```
//! # fn main() -> kvsql_engine::Result<()> {
//! use kvsql_engine::{Db, sql::Value, sim::SimStorage, Options};
//! # let db = Db::open_with(Box::new(SimStorage::new()), Options::default())?;
//! db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)", &[])?;
//! db.execute("INSERT INTO users VALUES (42, 'Ada')", &[])?;
//! let row = db.get_row("users", &Value::Int(42))?;      // no SQL parsing
//! assert_eq!(row, Some(vec![Value::Int(42), Value::Text("Ada".into())]));
//! let r = db.execute("SELECT name FROM users WHERE id = ?", &[Value::Int(42)])?;
//! assert_eq!(r.rows, vec![vec![Value::Text("Ada".into())]]);
//! # Ok(())
//! # }
//! ```

mod crc32;
mod db;
mod error;
mod idhash;
mod page;
pub mod sql;
mod storage;
mod tree;

#[doc(hidden)]
pub mod sim;

pub use db::{Db, Options, ReadTx, VerifyReport, WriteTx};
pub use error::{Error, Result};
pub use storage::{FileStorage, Storage};
pub use tree::{MAX_KEY_LEN, MAX_VALUE_LEN};

/// Version of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
