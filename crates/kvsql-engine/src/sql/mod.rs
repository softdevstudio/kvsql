//! SQL on the same records the key-value API reads and writes.
//!
//! A table is a keyspace of the same name. Each row is stored under its
//! primary key: an INTEGER key as 8 bytes big-endian with the sign bit
//! flipped, a TEXT key as its UTF-8 bytes, a BLOB key as itself (see
//! [`key_for`]). The row value is [`encode_row`] of every column.
//!
//! The KV door and the SQL door meet at those bytes. `get_row` and
//! `put_row` reach a row by key with no parsing at all; SQL reaches the
//! same rows through the planner. Secondary indexes are kept by both doors,
//! inside the same transaction.
//!
//! A keyspace with no table definition reads in SQL as a table with two
//! BLOB columns, `key` and `value`.

mod catalog;
mod eval;
mod exec;
mod parser;
mod value;

pub use value::{decode_row, encode_row, ColType, Value};

use crate::error::{Error, Result};
pub(crate) use catalog::SchemaCache;

/// The rows and columns a statement produced.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryResult {
    /// Column names for SELECT; empty for other statements.
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// Rows inserted, updated or deleted by the last statement.
    pub changes: u64,
}

/// The keyspace key that holds the row with this primary key value.
pub fn key_for(pk: &Value) -> Result<Vec<u8>> {
    value::pk_key(pk)
}

/// Prefix of keyspaces the engine keeps for itself.
pub(crate) const RESERVED_PREFIX: &str = "__kvsql";
pub(crate) const CATALOG: &str = "__kvsql_catalog";

pub(crate) fn is_reserved(ks: &str) -> bool {
    ks.starts_with(RESERVED_PREFIX)
}

/// Reads the executor needs from a transaction.
pub(crate) trait Kv {
    fn kv_get(&mut self, ks: &str, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn kv_scan(
        &mut self,
        ks: &str,
        from: &[u8],
        to: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    fn kv_exists(&mut self, ks: &str) -> Result<bool>;
    fn kv_last_key(&mut self, ks: &str) -> Result<Option<Vec<u8>>>;
    fn schemas(&mut self) -> &mut SchemaCache;
}

/// Writes the executor needs. These bypass the KV door's checks.
pub(crate) trait KvWrite: Kv {
    fn kv_put(&mut self, ks: &str, key: &[u8], value: &[u8]) -> Result<()>;
    fn kv_delete(&mut self, ks: &str, key: &[u8]) -> Result<bool>;
    fn kv_create(&mut self, ks: &str) -> Result<()>;
    fn kv_drop(&mut self, ks: &str) -> Result<bool>;
    /// Remember the current state so a failing statement can be undone.
    fn mark(&mut self);
    /// Go back to the last `mark`.
    fn rollback(&mut self);
}

/// Run statements in a read-only transaction.
pub(crate) fn query<K: Kv>(kv: &mut K, sql: &str, params: &[Value]) -> Result<QueryResult> {
    let stmts = prepare(sql, params)?;
    let mut last = QueryResult::default();
    for s in &stmts {
        match s {
            parser::Stmt::Select(sel) => last = exec::select(kv, sel, params)?,
            _ => return Err(Error::Sql("a read transaction can only run SELECT".into())),
        }
    }
    Ok(last)
}

pub(crate) fn is_read_only(sql: &str) -> Result<bool> {
    let (stmts, _) = parser::parse(sql)?;
    Ok(stmts.iter().all(|s| matches!(s, parser::Stmt::Select(_))))
}

fn prepare(sql: &str, params: &[Value]) -> Result<Vec<parser::Stmt>> {
    let (stmts, n) = parser::parse(sql)?;
    if n != params.len() {
        return Err(Error::Sql(format!(
            "statement has {n} parameters but {} were given",
            params.len()
        )));
    }
    Ok(stmts)
}

/// Run each statement in turn. A failing statement is undone on its own.
pub(crate) fn execute<K: KvWrite>(kv: &mut K, sql: &str, params: &[Value]) -> Result<QueryResult> {
    let stmts = prepare(sql, params)?;
    let mut last = QueryResult::default();
    for s in &stmts {
        kv.mark();
        match exec::statement(kv, s, params) {
            Ok(r) => last = r,
            Err(e) => {
                kv.rollback();
                return Err(e);
            }
        }
    }
    Ok(last)
}

/// KV door: read a table row by primary key.
pub(crate) fn get_row<K: Kv>(kv: &mut K, table: &str, pk: &Value) -> Result<Option<Vec<Value>>> {
    exec::get_row(kv, table, pk)
}

/// KV door: insert or replace a whole row, keeping indexes.
pub(crate) fn put_row<K: KvWrite>(kv: &mut K, table: &str, row: Vec<Value>) -> Result<()> {
    exec::put_row(kv, table, row)
}

/// KV door: delete a row by primary key, keeping indexes.
pub(crate) fn delete_row<K: KvWrite>(kv: &mut K, table: &str, pk: &Value) -> Result<bool> {
    exec::delete_row_by_pk(kv, table, pk)
}

/// Checks for a raw `put` into a keyspace: rows written to a table's
/// keyspace must be valid rows and keep its indexes; engine keyspaces are
/// off limits. Returns `Ok(false)` when the keyspace is plain and the caller
/// should store the bytes as they are.
pub(crate) fn raw_put_hook<K: KvWrite>(
    kv: &mut K,
    ks: &str,
    key: &[u8],
    value: &[u8],
) -> Result<bool> {
    if is_reserved(ks) {
        return Err(Error::Sql(format!(
            "keyspace {ks} is reserved for the engine"
        )));
    }
    let Some(t) = catalog::table(kv, ks)? else {
        return Ok(false);
    };
    if t.plain {
        return Ok(false);
    }
    let row = decode_row(value)
        .map_err(|_| Error::Constraint(format!("value for table {ks} is not an encoded row")))?;
    let pk_col = &t.columns[t.pk];
    let pk = row.get(t.pk).cloned().unwrap_or(Value::Null);
    let pk = pk_col.ty.coerce(pk, &pk_col.name)?;
    if value::pk_key(&pk)? != key {
        return Err(Error::Constraint(format!(
            "row's primary key does not match the key it is stored under in {ks}"
        )));
    }
    exec::put_row(kv, ks, row)?;
    Ok(true)
}

/// Checks for a raw `delete`. Returns `Some(found)` when it handled the
/// delete itself (a table row), `None` for a plain keyspace.
pub(crate) fn raw_delete_hook<K: KvWrite>(
    kv: &mut K,
    ks: &str,
    key: &[u8],
) -> Result<Option<bool>> {
    if is_reserved(ks) {
        return Err(Error::Sql(format!(
            "keyspace {ks} is reserved for the engine"
        )));
    }
    let Some(t) = catalog::table(kv, ks)? else {
        return Ok(None);
    };
    if t.plain {
        return Ok(None);
    }
    let pk = value::pk_value(t.columns[t.pk].ty, key)?;
    Ok(Some(exec::delete_row_by_pk(kv, ks, &pk)?))
}

/// Checks for a raw keyspace drop: tables go through DROP TABLE.
pub(crate) fn raw_drop_hook<K: KvWrite>(kv: &mut K, ks: &str) -> Result<()> {
    if is_reserved(ks) {
        return Err(Error::Sql(format!(
            "keyspace {ks} is reserved for the engine"
        )));
    }
    if let Some(t) = catalog::table(kv, ks)? {
        if !t.plain {
            return Err(Error::Sql(format!("{ks} is a table; use DROP TABLE")));
        }
    }
    Ok(())
}

/// Check every table against its definition and indexes: each row decodes,
/// fits its columns and sits under its own primary key, and each index holds
/// exactly one entry per row.
pub(crate) fn verify<K: Kv>(kv: &mut K) -> Result<()> {
    let tables: Vec<String> = kv
        .kv_scan(CATALOG, b"t:", Some(b"t;"), usize::MAX)?
        .into_iter()
        .map(|(k, _)| String::from_utf8_lossy(&k[2..]).into_owned())
        .collect();
    for name in tables {
        let Some(t) = catalog::table(kv, &name)? else {
            return Err(Error::Corrupt(format!(
                "catalog lists {name} but it cannot be loaded"
            )));
        };
        let mut expected: Vec<std::collections::BTreeSet<Vec<u8>>> =
            vec![Default::default(); t.indexes.len()];
        for (k, v) in kv.kv_scan(&name, b"", None, usize::MAX)? {
            let row = decode_row(&v)?;
            let row = catalog::coerce_row(&t, row).map_err(|e| {
                Error::Corrupt(format!("row in {name} breaks its table definition: {e}"))
            })?;
            if value::pk_key(&row[t.pk])? != k {
                return Err(Error::Corrupt(format!(
                    "row in {name} is stored under the wrong key"
                )));
            }
            for (i, ix) in t.indexes.iter().enumerate() {
                expected[i].insert(exec::index_entry(&row[ix.col], &k));
            }
        }
        for (i, ix) in t.indexes.iter().enumerate() {
            let got: std::collections::BTreeSet<Vec<u8>> = kv
                .kv_scan(&catalog::index_ks(&ix.name), b"", None, usize::MAX)?
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            if got != expected[i] {
                return Err(Error::Corrupt(format!(
                    "index {} does not match table {name}: {} entries, {} rows",
                    ix.name,
                    got.len(),
                    expected[i].len()
                )));
            }
        }
    }
    Ok(())
}
