//! Table and index definitions, kept in the `__kvsql_catalog` keyspace.
//!
//! `t:<table>` holds one line per column: name, type, not-null flag and
//! primary-key flag, tab separated. `i:<index>` holds `<table>\t<column>`.
//! Index entries live in the keyspace `__kvsql_index:<index>`.

use std::collections::HashMap;
use std::sync::Arc;

use super::parser::ColumnDef;
use super::value::{ColType, Value};
use super::{is_reserved, Kv, KvWrite, CATALOG};
use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct Index {
    pub name: String,
    pub col: usize,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub pk: usize,
    pub indexes: Vec<Index>,
    /// A keyspace with no table definition, read as (key, value) blobs.
    pub plain: bool,
}

impl Table {
    pub fn col(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }
}

/// Table definitions looked up in one transaction.
#[derive(Default)]
pub struct SchemaCache {
    map: HashMap<String, Option<Arc<Table>>>,
}

impl SchemaCache {
    pub fn clear(&mut self) {
        self.map.clear();
    }
}

pub fn index_ks(index: &str) -> String {
    format!("{}_index:{index}", super::RESERVED_PREFIX)
}

fn tkey(name: &str) -> Vec<u8> {
    format!("t:{name}").into_bytes()
}
fn ikey(name: &str) -> Vec<u8> {
    format!("i:{name}").into_bytes()
}

fn bad(msg: &str) -> Error {
    Error::Corrupt(format!("catalog: {msg}"))
}

fn decode_table(name: &str, text: &[u8]) -> Result<Table> {
    let text = std::str::from_utf8(text).map_err(|_| bad("not UTF-8"))?;
    let mut columns = Vec::new();
    let mut pk = None;
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 4 {
            return Err(bad("bad column line"));
        }
        let ty = match f[1] {
            "INTEGER" => ColType::Integer,
            "REAL" => ColType::Real,
            "TEXT" => ColType::Text,
            "BLOB" => ColType::Blob,
            "ANY" => ColType::Any,
            _ => return Err(bad("bad column type")),
        };
        if f[3] == "1" {
            pk = Some(columns.len());
        }
        columns.push(ColumnDef {
            name: f[0].to_string(),
            ty,
            not_null: f[2] == "1",
            primary_key: f[3] == "1",
        });
    }
    Ok(Table {
        name: name.to_string(),
        columns,
        pk: pk.ok_or_else(|| bad("table without primary key"))?,
        indexes: Vec::new(),
        plain: false,
    })
}

fn encode_table(columns: &[ColumnDef]) -> Vec<u8> {
    columns
        .iter()
        .map(|c| {
            format!(
                "{}\t{}\t{}\t{}",
                c.name,
                c.ty.name(),
                u8::from(c.not_null),
                u8::from(c.primary_key)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}

fn plain_table(name: &str) -> Table {
    let col = |n: &str, pk| ColumnDef {
        name: n.to_string(),
        ty: ColType::Blob,
        not_null: true,
        primary_key: pk,
    };
    Table {
        name: name.to_string(),
        columns: vec![col("key", true), col("value", false)],
        pk: 0,
        indexes: Vec::new(),
        plain: true,
    }
}

/// The table called `name`: a defined table, a plain keyspace, or none.
pub fn table<K: Kv>(kv: &mut K, name: &str) -> Result<Option<Arc<Table>>> {
    if let Some(t) = kv.schemas().map.get(name) {
        return Ok(t.clone());
    }
    let t = load(kv, name)?.map(Arc::new);
    kv.schemas().map.insert(name.to_string(), t.clone());
    Ok(t)
}

fn load<K: Kv>(kv: &mut K, name: &str) -> Result<Option<Table>> {
    if is_reserved(name) {
        return Ok(None);
    }
    if let Some(def) = kv.kv_get(CATALOG, &tkey(name))? {
        let mut t = decode_table(name, &def)?;
        for (k, v) in kv.kv_scan(CATALOG, b"i:", Some(b"i;"), usize::MAX)? {
            let v = String::from_utf8(v).map_err(|_| bad("index entry not UTF-8"))?;
            let (tab, col) = v.split_once('\t').ok_or_else(|| bad("bad index entry"))?;
            if tab == name {
                let col = t.col(col).ok_or_else(|| bad("index on missing column"))?;
                t.indexes.push(Index {
                    name: String::from_utf8_lossy(&k[2..]).into_owned(),
                    col,
                });
            }
        }
        return Ok(Some(t));
    }
    if kv.kv_exists(name)? {
        return Ok(Some(plain_table(name)));
    }
    Ok(None)
}

fn check_name(name: &str, what: &str) -> Result<()> {
    if name.is_empty() || name.len() > 200 || name.contains(['\t', '\n']) {
        return Err(Error::Sql(format!("bad {what} name {name:?}")));
    }
    if is_reserved(name) {
        return Err(Error::Sql(format!(
            "{what} names starting with {} are reserved",
            super::RESERVED_PREFIX
        )));
    }
    Ok(())
}

pub fn create_table<K: KvWrite>(
    kv: &mut K,
    name: &str,
    columns: &[ColumnDef],
    if_not_exists: bool,
) -> Result<bool> {
    check_name(name, "table")?;
    if kv.kv_get(CATALOG, &tkey(name))?.is_some() {
        return if if_not_exists {
            Ok(false)
        } else {
            Err(Error::Sql(format!("table {name} already exists")))
        };
    }
    if kv.kv_exists(name)? {
        return Err(Error::Sql(format!(
            "a keyspace named {name} already exists; drop it first or pick another name"
        )));
    }
    let pks: Vec<&ColumnDef> = columns.iter().filter(|c| c.primary_key).collect();
    if pks.len() != 1 {
        return Err(Error::Sql(format!(
            "table {name} needs exactly one PRIMARY KEY column"
        )));
    }
    if matches!(pks[0].ty, ColType::Real | ColType::Any) {
        return Err(Error::Sql(
            "a primary key must be INTEGER, TEXT or BLOB".into(),
        ));
    }
    for (i, c) in columns.iter().enumerate() {
        check_name(&c.name, "column")?;
        if columns[..i]
            .iter()
            .any(|d| d.name.eq_ignore_ascii_case(&c.name))
        {
            return Err(Error::Sql(format!("duplicate column {}", c.name)));
        }
    }
    kv.kv_put(CATALOG, &tkey(name), &encode_table(columns))?;
    kv.kv_create(name)?;
    kv.schemas().clear();
    Ok(true)
}

pub fn drop_table<K: KvWrite>(kv: &mut K, name: &str, if_exists: bool) -> Result<bool> {
    let Some(t) = table(kv, name)? else {
        return if if_exists {
            Ok(false)
        } else {
            Err(Error::Sql(format!("no such table: {name}")))
        };
    };
    if t.plain {
        return Err(Error::Sql(format!(
            "{name} is a plain keyspace, not a table; drop it through the KV API"
        )));
    }
    for ix in &t.indexes {
        kv.kv_drop(&index_ks(&ix.name))?;
        kv.kv_delete(CATALOG, &ikey(&ix.name))?;
    }
    kv.kv_delete(CATALOG, &tkey(name))?;
    kv.kv_drop(name)?;
    kv.schemas().clear();
    Ok(true)
}

pub fn create_index<K: KvWrite>(
    kv: &mut K,
    name: &str,
    table_name: &str,
    column: &str,
    if_not_exists: bool,
) -> Result<bool> {
    check_name(name, "index")?;
    if kv.kv_get(CATALOG, &ikey(name))?.is_some() {
        return if if_not_exists {
            Ok(false)
        } else {
            Err(Error::Sql(format!("index {name} already exists")))
        };
    }
    let Some(t) = table(kv, table_name)? else {
        return Err(Error::Sql(format!("no such table: {table_name}")));
    };
    if t.plain {
        return Err(Error::Sql("plain keyspaces cannot have indexes".into()));
    }
    let col = t
        .col(column)
        .ok_or_else(|| Error::Sql(format!("no such column: {column}")))?;
    let ks = index_ks(name);
    kv.kv_create(&ks)?;
    // Index the rows already there, in batches.
    let mut from = Vec::new();
    loop {
        let batch = kv.kv_scan(&t.name, &from, None, 512)?;
        let done = batch.len() < 512;
        for (k, v) in &batch {
            let row = super::decode_row(v)?;
            kv.kv_put(&ks, &super::exec::index_entry(&row[col], k), &[])?;
        }
        match batch.last() {
            Some((k, _)) if !done => {
                from = k.clone();
                from.push(0);
            }
            _ => break,
        }
    }
    kv.kv_put(
        CATALOG,
        &ikey(name),
        format!("{}\t{}", t.name, t.columns[col].name).as_bytes(),
    )?;
    kv.schemas().clear();
    Ok(true)
}

pub fn drop_index<K: KvWrite>(kv: &mut K, name: &str, if_exists: bool) -> Result<bool> {
    if kv.kv_get(CATALOG, &ikey(name))?.is_none() {
        return if if_exists {
            Ok(false)
        } else {
            Err(Error::Sql(format!("no such index: {name}")))
        };
    }
    kv.kv_delete(CATALOG, &ikey(name))?;
    kv.kv_drop(&index_ks(name))?;
    kv.schemas().clear();
    Ok(true)
}

/// Value check used by both doors before a row is stored.
pub fn coerce_row(t: &Table, row: Vec<Value>) -> Result<Vec<Value>> {
    if row.len() != t.columns.len() {
        return Err(Error::Constraint(format!(
            "table {} has {} columns but the row has {}",
            t.name,
            t.columns.len(),
            row.len()
        )));
    }
    row.into_iter()
        .zip(&t.columns)
        .map(|(v, c)| {
            let v = c.ty.coerce(v, &c.name)?;
            if v.is_null() && (c.not_null || c.primary_key) {
                return Err(Error::Constraint(format!(
                    "NOT NULL constraint failed: {}.{}",
                    t.name, c.name
                )));
            }
            Ok(v)
        })
        .collect()
}
