//! Statement execution: access paths, joins, grouping, ordering, and the
//! row writes both doors share.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::catalog::{self, coerce_row, index_ks, Table};
use super::eval::{display, eval, Acc, AggSpec, Binder, OrdVal, Scope, E};
use super::parser::{BinOp, Expr, FromItem, JoinKind, Select, SelectItem, Stmt};
use super::value::{
    decode_row, encode_row, index_part, pk_key, pk_value, prefix_end, total_cmp, ColType, Value,
};
use super::{Kv, KvWrite, QueryResult};
use crate::error::{Error, Result};

const BATCH: usize = 512;

/// Callback for each row a scan finds: (key, row); returns false to stop.
type RowFn<'a, K> = dyn FnMut(&mut K, Vec<u8>, Vec<Value>) -> Result<bool> + 'a;

fn sql_err<T>(m: impl Into<String>) -> Result<T> {
    Err(Error::Sql(m.into()))
}

fn need_table<K: Kv>(kv: &mut K, name: &str) -> Result<Arc<Table>> {
    catalog::table(kv, name)?.ok_or_else(|| Error::Sql(format!("no such table: {name}")))
}

// ---------------------------------------------------------------------------
// Row storage shared by SQL and the KV door

fn row_from_kv(t: &Table, key: &[u8], val: &[u8]) -> Result<Vec<Value>> {
    if t.plain {
        return Ok(vec![Value::Blob(key.to_vec()), Value::Blob(val.to_vec())]);
    }
    let row = decode_row(val)?;
    if row.len() != t.columns.len() {
        return Err(Error::Corrupt(format!(
            "row in {} has {} columns, table has {}",
            t.name,
            row.len(),
            t.columns.len()
        )));
    }
    Ok(row)
}

pub fn index_entry(v: &Value, pk_key: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(16 + pk_key.len());
    index_part(v, &mut k);
    k.extend_from_slice(pk_key);
    k
}

/// Coerce a row for a table; plain keyspaces take text or blobs.
fn prepare_row(t: &Table, row: Vec<Value>) -> Result<Vec<Value>> {
    if !t.plain {
        return coerce_row(t, row);
    }
    if row.len() != 2 {
        return Err(Error::Constraint(format!(
            "keyspace {} has two columns, key and value",
            t.name
        )));
    }
    row.into_iter()
        .zip(["key", "value"])
        .map(|(v, c)| match v {
            Value::Blob(b) => Ok(Value::Blob(b)),
            Value::Text(s) => Ok(Value::Blob(s.into_bytes())),
            Value::Null => Err(Error::Constraint(format!(
                "NOT NULL constraint failed: {}.{c}",
                t.name
            ))),
            v => Err(Error::Constraint(format!(
                "keyspace {}.{c} takes TEXT or BLOB, not {}",
                t.name,
                v.type_name().to_uppercase()
            ))),
        })
        .collect()
}

fn store_value(t: &Table, row: &[Value]) -> Vec<u8> {
    if t.plain {
        match &row[1] {
            Value::Blob(b) => b.clone(),
            _ => Vec::new(),
        }
    } else {
        encode_row(row)
    }
}

/// Insert a prepared row; fails if its primary key is taken.
fn insert_new<K: KvWrite>(kv: &mut K, t: &Table, row: &[Value]) -> Result<()> {
    let key = pk_key(&row[t.pk])?;
    if kv.kv_get(&t.name, &key)?.is_some() {
        return Err(Error::Constraint(format!(
            "UNIQUE constraint failed: {}.{}",
            t.name, t.columns[t.pk].name
        )));
    }
    kv.kv_put(&t.name, &key, &store_value(t, row))?;
    for ix in &t.indexes {
        kv.kv_put(&index_ks(&ix.name), &index_entry(&row[ix.col], &key), &[])?;
    }
    Ok(())
}

fn remove<K: KvWrite>(kv: &mut K, t: &Table, key: &[u8], row: &[Value]) -> Result<()> {
    kv.kv_delete(&t.name, key)?;
    for ix in &t.indexes {
        kv.kv_delete(&index_ks(&ix.name), &index_entry(&row[ix.col], key))?;
    }
    Ok(())
}

pub fn get_row<K: Kv>(kv: &mut K, table: &str, pk: &Value) -> Result<Option<Vec<Value>>> {
    let t = need_table(kv, table)?;
    let pk = if t.plain {
        prepare_row(&t, vec![pk.clone(), Value::Blob(Vec::new())])?.swap_remove(0)
    } else {
        let c = &t.columns[t.pk];
        c.ty.coerce(pk.clone(), &c.name)?
    };
    let key = pk_key(&pk)?;
    match kv.kv_get(&t.name, &key)? {
        Some(v) => Ok(Some(row_from_kv(&t, &key, &v)?)),
        None => Ok(None),
    }
}

/// Insert or replace a whole row.
pub fn put_row<K: KvWrite>(kv: &mut K, table: &str, row: Vec<Value>) -> Result<()> {
    let t = need_table(kv, table)?;
    let row = prepare_row(&t, row)?;
    let key = pk_key(&row[t.pk])?;
    if let Some(old) = kv.kv_get(&t.name, &key)? {
        let old = row_from_kv(&t, &key, &old)?;
        remove(kv, &t, &key, &old)?;
    }
    insert_new(kv, &t, &row)
}

pub fn delete_row_by_pk<K: KvWrite>(kv: &mut K, table: &str, pk: &Value) -> Result<bool> {
    let t = need_table(kv, table)?;
    let Some(old) = get_row(kv, table, pk)? else {
        return Ok(false);
    };
    let key = pk_key(&old[t.pk])?;
    remove(kv, &t, &key, &old)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Access paths

enum Access {
    Full,
    /// Primary key values, each evaluated against the row prefix.
    Keys(Vec<E>),
    /// Inclusive primary key bounds; the filter trims equal ends.
    Range(Option<E>, Option<E>),
    Index(String, ColType, E),
}

fn conjuncts(e: &E, out: &mut Vec<E>) {
    if let E::Bin(BinOp::And, a, b) = e {
        conjuncts(a, out);
        conjuncts(b, out);
    } else {
        out.push(e.clone());
    }
}

/// True when `e` can be computed from the first `prefix` columns alone.
fn usable(e: &E, prefix: usize) -> bool {
    e.is_const() || e.max_col().is_some_and(|m| m < prefix)
}

/// Pick an access path for the table whose columns start at `offset`.
fn choose_access(t: &Table, offset: usize, conds: &[E]) -> Access {
    let pk = offset + t.pk;
    let col_of = |e: &E| match e {
        E::Col(c) if *c >= offset && *c < offset + t.columns.len() => Some(*c),
        _ => None,
    };
    let mut lo = None;
    let mut hi = None;
    let mut index = None;
    for c in conds {
        match c {
            E::Bin(op @ (BinOp::Eq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge), a, b) => {
                let (col, other, op) = match (col_of(a), col_of(b)) {
                    (Some(x), _) if usable(b, offset) => (x, b, *op),
                    (_, Some(x)) if usable(a, offset) => (
                        x,
                        a,
                        match op {
                            BinOp::Lt => BinOp::Gt,
                            BinOp::Le => BinOp::Ge,
                            BinOp::Gt => BinOp::Lt,
                            BinOp::Ge => BinOp::Le,
                            o => *o,
                        },
                    ),
                    _ => continue,
                };
                if col == pk {
                    match op {
                        BinOp::Eq => return Access::Keys(vec![(**other).clone()]),
                        BinOp::Gt | BinOp::Ge => lo = Some((**other).clone()),
                        _ => hi = Some((**other).clone()),
                    }
                } else if op == BinOp::Eq && index.is_none() && !t.plain {
                    if let Some(ix) = t.indexes.iter().find(|ix| ix.col + offset == col) {
                        index = Some(Access::Index(
                            ix.name.clone(),
                            t.columns[ix.col].ty,
                            (**other).clone(),
                        ));
                    }
                }
            }
            E::In(a, list, false)
                if col_of(a) == Some(pk) && list.iter().all(|x| usable(x, offset)) =>
            {
                return Access::Keys(list.clone());
            }
            E::Between(a, l, h, false)
                if col_of(a) == Some(pk) && usable(l, offset) && usable(h, offset) =>
            {
                lo = Some((**l).clone());
                hi = Some((**h).clone());
            }
            _ => {}
        }
    }
    if let Some(ix) = index {
        return ix;
    }
    if lo.is_some() || hi.is_some() {
        return Access::Range(lo, hi);
    }
    Access::Full
}

/// The key for a primary key value, when the value's type can match the
/// column at all. Values that cannot match yield `None` (no row).
fn key_of(t: &Table, v: Value) -> Option<Vec<u8>> {
    let ty = t.columns[t.pk].ty;
    let v = match (ty, v) {
        (_, Value::Null) => return None,
        (ColType::Integer, Value::Int(i)) => Value::Int(i),
        (ColType::Integer, Value::Real(r)) if r.fract() == 0.0 && r.abs() < 9.2e18 => {
            Value::Int(r as i64)
        }
        (ColType::Text, Value::Text(s)) => Value::Text(s),
        (ColType::Blob, Value::Blob(b)) => Value::Blob(b),
        _ => return None,
    };
    pk_key(&v).ok()
}

/// Range bound key: only when the bound's type is the key's own type.
fn bound_key(t: &Table, v: Value) -> Option<Vec<u8>> {
    let ty = t.columns[t.pk].ty;
    match (ty, &v) {
        (ColType::Integer, Value::Int(_))
        | (ColType::Text, Value::Text(_))
        | (ColType::Blob, Value::Blob(_)) => pk_key(&v).ok(),
        _ => None,
    }
}

/// Feed every row of `t` reachable through `access` to `f` until it says
/// stop. `prefix` is the joined row so far, used to evaluate key exprs.
fn scan_table<K: Kv>(
    kv: &mut K,
    t: &Table,
    access: &Access,
    prefix: &[Value],
    f: &mut RowFn<'_, K>,
) -> Result<bool> {
    match access {
        Access::Keys(exprs) => {
            let mut keys = BTreeSet::new();
            for e in exprs {
                if let Some(k) = key_of(t, eval(e, prefix, &[])?) {
                    keys.insert(k);
                }
            }
            for k in keys {
                if let Some(v) = kv.kv_get(&t.name, &k)? {
                    let row = row_from_kv(t, &k, &v)?;
                    if !f(kv, k, row)? {
                        return Ok(false);
                    }
                }
            }
            Ok(true)
        }
        Access::Index(name, ty, e) => {
            let v = eval(e, prefix, &[])?;
            if v.is_null() {
                return Ok(true);
            }
            let Ok(v) = ty.coerce(v, "") else {
                return Ok(true);
            };
            let mut p = Vec::new();
            index_part(&v, &mut p);
            let end = prefix_end(&p);
            let ks = index_ks(name);
            let mut from = p.clone();
            loop {
                let batch = kv.kv_scan(&ks, &from, end.as_deref(), BATCH)?;
                let done = batch.len() < BATCH;
                for (ik, _) in &batch {
                    let key = ik[p.len()..].to_vec();
                    if let Some(val) = kv.kv_get(&t.name, &key)? {
                        let row = row_from_kv(t, &key, &val)?;
                        if !f(kv, key, row)? {
                            return Ok(false);
                        }
                    }
                }
                match batch.last() {
                    Some((k, _)) if !done => {
                        from = k.clone();
                        from.push(0);
                    }
                    _ => return Ok(true),
                }
            }
        }
        Access::Full | Access::Range(..) => {
            let (mut from, to) = match access {
                Access::Range(lo, hi) => {
                    let from = match lo {
                        Some(e) => bound_key(t, eval(e, prefix, &[])?).unwrap_or_default(),
                        None => Vec::new(),
                    };
                    let to = match hi {
                        Some(e) => bound_key(t, eval(e, prefix, &[])?).map(|mut k| {
                            k.push(0);
                            k
                        }),
                        None => None,
                    };
                    (from, to)
                }
                _ => (Vec::new(), None),
            };
            loop {
                let batch = kv.kv_scan(&t.name, &from, to.as_deref(), BATCH)?;
                let done = batch.len() < BATCH;
                let last = batch.last().map(|(k, _)| k.clone());
                for (k, v) in batch {
                    let row = row_from_kv(t, &k, &v)?;
                    if !f(kv, k, row)? {
                        return Ok(false);
                    }
                }
                match last {
                    Some(mut k) if !done => {
                        k.push(0);
                        from = k;
                    }
                    _ => return Ok(true),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Statements

pub fn statement<K: KvWrite>(kv: &mut K, s: &Stmt, params: &[Value]) -> Result<QueryResult> {
    let changed = |n: u64| QueryResult {
        changes: n,
        ..Default::default()
    };
    match s {
        Stmt::Select(sel) => select(kv, sel, params),
        Stmt::CreateTable {
            name,
            if_not_exists,
            columns,
        } => catalog::create_table(kv, name, columns, *if_not_exists).map(|_| changed(0)),
        Stmt::DropTable { name, if_exists } => {
            catalog::drop_table(kv, name, *if_exists).map(|_| changed(0))
        }
        Stmt::CreateIndex {
            name,
            table,
            column,
            if_not_exists,
        } => catalog::create_index(kv, name, table, column, *if_not_exists).map(|_| changed(0)),
        Stmt::DropIndex { name, if_exists } => {
            catalog::drop_index(kv, name, *if_exists).map(|_| changed(0))
        }
        Stmt::Insert {
            table,
            columns,
            rows,
        } => insert(kv, table, columns.as_deref(), rows, params).map(changed),
        Stmt::Update {
            table,
            sets,
            filter,
        } => update(kv, table, sets, filter.as_ref(), params).map(changed),
        Stmt::Delete { table, filter } => delete(kv, table, filter.as_ref(), params).map(changed),
    }
}

fn table_scope(t: &Table, alias: &str) -> Scope {
    t.columns
        .iter()
        .map(|c| (alias.to_string(), c.name.clone(), c.ty))
        .collect()
}

fn insert<K: KvWrite>(
    kv: &mut K,
    table: &str,
    columns: Option<&[String]>,
    rows: &[Vec<Expr>],
    params: &[Value],
) -> Result<u64> {
    let t = need_table(kv, table)?;
    let targets: Vec<usize> = match columns {
        None => (0..t.columns.len()).collect(),
        Some(cols) => {
            let mut v = Vec::new();
            for c in cols {
                let i = t.col(c).ok_or_else(|| {
                    Error::Sql(format!("table {} has no column named {c}", t.name))
                })?;
                if v.contains(&i) {
                    return sql_err(format!("column {c} listed twice"));
                }
                v.push(i);
            }
            v
        }
    };
    let empty = Scope::new();
    let mut n = 0;
    for exprs in rows {
        if exprs.len() != targets.len() {
            return sql_err(format!(
                "{} values for {} columns",
                exprs.len(),
                targets.len()
            ));
        }
        let mut row = vec![Value::Null; t.columns.len()];
        for (e, &i) in exprs.iter().zip(&targets) {
            let mut b = Binder {
                scope: &empty,
                params,
                aggs: None,
                aliases: &[],
            };
            row[i] = eval(&b.bind(e)?, &[], &[])?;
        }
        if !t.plain && row[t.pk].is_null() && t.columns[t.pk].ty == ColType::Integer {
            row[t.pk] = Value::Int(next_rowid(kv, &t)?);
        }
        let row = prepare_row(&t, row)?;
        insert_new(kv, &t, &row)?;
        n += 1;
    }
    Ok(n)
}

fn next_rowid<K: Kv>(kv: &mut K, t: &Table) -> Result<i64> {
    match kv.kv_last_key(&t.name)? {
        None => Ok(1),
        Some(k) => match pk_value(ColType::Integer, &k)? {
            Value::Int(i) => i
                .checked_add(1)
                .ok_or_else(|| Error::Constraint("no free integer primary key left".into())),
            _ => unreachable!(),
        },
    }
}

/// Rows of `t` matching `filter`, as (key, row) pairs.
fn matching<K: Kv>(
    kv: &mut K,
    t: &Table,
    filter: Option<&Expr>,
    params: &[Value],
) -> Result<Vec<(Vec<u8>, Vec<Value>)>> {
    let scope = table_scope(t, &t.name);
    let cond = match filter {
        Some(f) => Some(
            Binder {
                scope: &scope,
                params,
                aggs: None,
                aliases: &[],
            }
            .bind(f)?,
        ),
        None => None,
    };
    let mut conds = Vec::new();
    if let Some(c) = &cond {
        conjuncts(c, &mut conds);
    }
    let access = choose_access(t, 0, &conds);
    let mut out = Vec::new();
    let mut err = None;
    scan_table(kv, t, &access, &[], &mut |_, k, row| {
        let keep = match &cond {
            Some(c) => match eval(c, &row, &[]) {
                Ok(v) => v.truth() == Some(true),
                Err(e) => {
                    err = Some(e);
                    return Ok(false);
                }
            },
            None => true,
        };
        if keep {
            out.push((k, row));
        }
        Ok(true)
    })?;
    if let Some(e) = err {
        return Err(e);
    }
    Ok(out)
}

fn update<K: KvWrite>(
    kv: &mut K,
    table: &str,
    sets: &[(String, Expr)],
    filter: Option<&Expr>,
    params: &[Value],
) -> Result<u64> {
    let t = need_table(kv, table)?;
    let scope = table_scope(&t, &t.name);
    let mut bound = Vec::new();
    for (c, e) in sets {
        let i = t
            .col(c)
            .ok_or_else(|| Error::Sql(format!("no such column: {c}")))?;
        let e = Binder {
            scope: &scope,
            params,
            aggs: None,
            aliases: &[],
        }
        .bind(e)?;
        bound.push((i, e));
    }
    let rows = matching(kv, &t, filter, params)?;
    for (key, old) in &rows {
        let mut new = old.clone();
        for (i, e) in &bound {
            new[*i] = eval(e, old, &[])?;
        }
        let new = prepare_row(&t, new)?;
        remove(kv, &t, key, old)?;
        insert_new(kv, &t, &new)?;
    }
    Ok(rows.len() as u64)
}

fn delete<K: KvWrite>(
    kv: &mut K,
    table: &str,
    filter: Option<&Expr>,
    params: &[Value],
) -> Result<u64> {
    let t = need_table(kv, table)?;
    let rows = matching(kv, &t, filter, params)?;
    for (key, old) in &rows {
        remove(kv, &t, key, old)?;
    }
    Ok(rows.len() as u64)
}

// ---------------------------------------------------------------------------
// SELECT

fn has_agg(e: &Expr) -> bool {
    match e {
        Expr::Func {
            name, args, star, ..
        } => {
            matches!(
                name.as_str(),
                "count" | "sum" | "total" | "avg" | "group_concat"
            ) || (matches!(name.as_str(), "min" | "max") && (args.len() == 1 || *star))
                || args.iter().any(has_agg)
        }
        Expr::Lit(_) | Expr::Param(_) | Expr::Column { .. } => false,
        Expr::Neg(a) | Expr::Not(a) | Expr::IsNull(a, _) | Expr::Cast(a, _) => has_agg(a),
        Expr::Bin(_, a, b) => has_agg(a) || has_agg(b),
        Expr::In { expr, list, .. } => has_agg(expr) || list.iter().any(has_agg),
        Expr::Between { expr, lo, hi, .. } => has_agg(expr) || has_agg(lo) || has_agg(hi),
        Expr::Like { expr, pattern, .. } => has_agg(expr) || has_agg(pattern),
        Expr::Case {
            operand,
            whens,
            otherwise,
        } => {
            operand.as_deref().is_some_and(has_agg)
                || whens.iter().any(|(a, b)| has_agg(a) || has_agg(b))
                || otherwise.as_deref().is_some_and(has_agg)
        }
    }
}

struct Source {
    table: Arc<Table>,
    join: JoinKind,
    on: Option<E>,
    access: Access,
}

fn int_arg(e: &Expr, params: &[Value], what: &str) -> Result<i64> {
    let empty = Scope::new();
    let v = eval(
        &Binder {
            scope: &empty,
            params,
            aggs: None,
            aliases: &[],
        }
        .bind(e)?,
        &[],
        &[],
    )?;
    match v.to_number() {
        Some(Value::Int(i)) => Ok(i),
        Some(Value::Real(r)) if r.fract() == 0.0 => Ok(r as i64),
        _ => sql_err(format!("{what} must be an integer")),
    }
}

/// Run the join for sources `level..` on top of `prefix`, calling `emit`
/// with each complete row. Returns false when `emit` asked to stop.
fn join<K: Kv>(
    kv: &mut K,
    sources: &[Source],
    level: usize,
    prefix: &mut Vec<Value>,
    emit: &mut dyn FnMut(&mut K, &[Value]) -> Result<bool>,
) -> Result<bool> {
    if level == sources.len() {
        return emit(kv, prefix);
    }
    let src = &sources[level];
    let width = src.table.columns.len();
    let mut matched = false;
    let mut result = Ok(true);
    let keep_going = {
        let prefix_snapshot = prefix.clone();
        scan_table(
            kv,
            &src.table,
            &src.access,
            &prefix_snapshot,
            &mut |kv, _, row| {
                let mut full = prefix_snapshot.clone();
                full.extend(row);
                if let Some(on) = &src.on {
                    if eval(on, &full, &[])?.truth() != Some(true) {
                        return Ok(true);
                    }
                }
                matched = true;
                match join(kv, sources, level + 1, &mut full, emit) {
                    Ok(true) => Ok(true),
                    Ok(false) => {
                        result = Ok(false);
                        Ok(false)
                    }
                    Err(e) => Err(e),
                }
            },
        )?
    };
    if !keep_going {
        return result;
    }
    if !matched && src.join == JoinKind::Left {
        prefix.extend(std::iter::repeat_n(Value::Null, width));
        let r = join(kv, sources, level + 1, prefix, emit);
        prefix.truncate(prefix.len() - width);
        return r;
    }
    Ok(true)
}

pub fn select<K: Kv>(kv: &mut K, sel: &Select, params: &[Value]) -> Result<QueryResult> {
    // Sources and the column scope.
    let mut scope = Scope::new();
    let mut tables = Vec::new();
    for f in &sel.from {
        let FromItem { table, alias, .. } = f;
        let t = need_table(kv, table)?;
        let a = alias.clone().unwrap_or_else(|| table.clone());
        if tables
            .iter()
            .any(|(_, x, _): &(Arc<Table>, String, usize)| x.eq_ignore_ascii_case(&a))
        {
            return sql_err(format!("table name {a} used twice; give one an alias"));
        }
        let offset = scope.len();
        scope.extend(table_scope(&t, &a));
        tables.push((t, a, offset));
    }

    let bind_plain = |e: &Expr| {
        Binder {
            scope: &scope,
            params,
            aggs: None,
            aliases: &[],
        }
        .bind(e)
    };
    let filter = sel.filter.as_ref().map(bind_plain).transpose()?;
    let mut where_conds = Vec::new();
    if let Some(f) = &filter {
        conjuncts(f, &mut where_conds);
    }
    let mut sources = Vec::new();
    for (i, ((t, _, offset), f)) in tables.iter().zip(&sel.from).enumerate() {
        let on = f.on.as_ref().map(bind_plain).transpose()?;
        let mut conds = Vec::new();
        if let Some(o) = &on {
            conjuncts(o, &mut conds);
        }
        // WHERE terms may narrow a table that no LEFT JOIN makes optional.
        let optional = sel.from[..=i].iter().any(|x| x.join == JoinKind::Left);
        if !optional {
            conds.extend(where_conds.iter().cloned());
        }
        let access = choose_access(t, *offset, &conds);
        sources.push(Source {
            table: t.clone(),
            join: f.join,
            on,
            access,
        });
    }

    // Output columns.
    let mut names = Vec::new();
    let mut item_exprs: Vec<Expr> = Vec::new();
    for it in &sel.items {
        match it {
            SelectItem::Star => {
                if tables.is_empty() {
                    return sql_err("no tables specified for *");
                }
                for (i, (t, alias, _)) in tables.iter().enumerate() {
                    let _ = i;
                    for c in &t.columns {
                        names.push(c.name.clone());
                        item_exprs.push(Expr::Column {
                            table: Some(alias.clone()),
                            name: c.name.clone(),
                        });
                    }
                }
            }
            SelectItem::TableStar(a) => {
                let Some((t, alias, _)) = tables.iter().find(|(_, x, _)| x.eq_ignore_ascii_case(a))
                else {
                    return sql_err(format!("no such table: {a}"));
                };
                for c in &t.columns {
                    names.push(c.name.clone());
                    item_exprs.push(Expr::Column {
                        table: Some(alias.clone()),
                        name: c.name.clone(),
                    });
                }
            }
            SelectItem::Expr(e, alias) => {
                names.push(alias.clone().unwrap_or_else(|| display(e)));
                item_exprs.push(e.clone());
            }
        }
    }
    let aliases: Vec<(String, Expr)> = sel
        .items
        .iter()
        .filter_map(|it| match it {
            SelectItem::Expr(e, Some(a)) => Some((a.clone(), e.clone())),
            _ => None,
        })
        .collect();
    let bind_late = |e: &Expr| {
        Binder {
            scope: &scope,
            params,
            aggs: None,
            aliases: &aliases,
        }
        .bind(e)
    };

    // ORDER BY terms refer to an output column (by position or alias) or
    // to an expression over the source row.
    let order_terms: Vec<(Expr, bool)> = sel
        .order_by
        .iter()
        .map(|(e, desc)| {
            let resolved = match e {
                Expr::Lit(Value::Int(k)) => {
                    if *k < 1 || *k as usize > item_exprs.len() {
                        return sql_err(format!(
                            "ORDER BY term {k} is out of range: there are {} result columns",
                            item_exprs.len()
                        ));
                    }
                    item_exprs[*k as usize - 1].clone()
                }
                Expr::Column { table: None, name } => {
                    let by_alias = sel.items.iter().find_map(|it| match it {
                        SelectItem::Expr(x, Some(a)) if a.eq_ignore_ascii_case(name) => {
                            Some(x.clone())
                        }
                        _ => None,
                    });
                    by_alias.unwrap_or_else(|| e.clone())
                }
                e => e.clone(),
            };
            Ok((resolved, *desc))
        })
        .collect::<Result<_>>()?;

    // GROUP BY takes result column positions the same way.
    let group_terms: Vec<Expr> = sel
        .group_by
        .iter()
        .map(|e| match e {
            Expr::Lit(Value::Int(k)) => {
                if *k < 1 || *k as usize > item_exprs.len() {
                    return sql_err(format!(
                        "GROUP BY term {k} is out of range: there are {} result columns",
                        item_exprs.len()
                    ));
                }
                let x = item_exprs[*k as usize - 1].clone();
                if has_agg(&x) {
                    return sql_err("aggregate functions are not allowed in the GROUP BY clause");
                }
                Ok(x)
            }
            e => Ok(e.clone()),
        })
        .collect::<Result<_>>()?;

    let aggregate = !sel.group_by.is_empty()
        || item_exprs.iter().any(has_agg)
        || sel.having.as_ref().is_some_and(has_agg)
        || order_terms.iter().any(|(e, _)| has_agg(e));
    if sel.having.is_some() && !aggregate {
        return sql_err("HAVING needs GROUP BY or an aggregate");
    }

    let limit = match &sel.limit {
        Some(e) => {
            let n = int_arg(e, params, "LIMIT")?;
            if n < 0 {
                None
            } else {
                Some(n as usize)
            }
        }
        None => None,
    };
    let offset = match &sel.offset {
        Some(e) => int_arg(e, params, "OFFSET")?.max(0) as usize,
        None => 0,
    };

    let mut rows: Vec<(Vec<Value>, Vec<Value>)> = Vec::new(); // (sort keys, output)

    if !aggregate {
        let items: Vec<E> = item_exprs.iter().map(bind_plain).collect::<Result<_>>()?;
        let keys: Vec<E> = order_terms
            .iter()
            .map(|(e, _)| bind_late(e))
            .collect::<Result<_>>()?;
        // Without ORDER BY or DISTINCT we can stop as soon as LIMIT is met.
        let stop_at = if keys.is_empty() && !sel.distinct {
            limit.map(|l| l + offset)
        } else {
            None
        };
        let mut seen: BTreeSet<Vec<OrdVal>> = BTreeSet::new();
        let mut emit = |_: &mut K, row: &[Value]| -> Result<bool> {
            if let Some(f) = &filter {
                if eval(f, row, &[])?.truth() != Some(true) {
                    return Ok(true);
                }
            }
            let out: Vec<Value> = items
                .iter()
                .map(|e| eval(e, row, &[]))
                .collect::<Result<_>>()?;
            if sel.distinct && !seen.insert(out.iter().cloned().map(OrdVal).collect()) {
                return Ok(true);
            }
            let k: Vec<Value> = keys
                .iter()
                .map(|e| eval(e, row, &[]))
                .collect::<Result<_>>()?;
            rows.push((k, out));
            Ok(stop_at.is_none_or(|n| rows.len() < n))
        };
        if sources.is_empty() {
            emit(kv, &[])?;
        } else {
            join(kv, &sources, 0, &mut Vec::new(), &mut emit)?;
        }
    } else {
        let mut specs: Vec<AggSpec> = Vec::new();
        let bind_agg = |e: &Expr, specs: &mut Vec<AggSpec>| {
            Binder {
                scope: &scope,
                params,
                aggs: Some(specs),
                aliases: &aliases,
            }
            .bind(e)
        };
        let items: Vec<E> = item_exprs
            .iter()
            .map(|e| bind_agg(e, &mut specs))
            .collect::<Result<_>>()?;
        let having = sel
            .having
            .as_ref()
            .map(|e| bind_agg(e, &mut specs))
            .transpose()?;
        let keys: Vec<E> = order_terms
            .iter()
            .map(|(e, _)| bind_agg(e, &mut specs))
            .collect::<Result<_>>()?;
        let group_exprs: Vec<E> = group_terms.iter().map(bind_late).collect::<Result<_>>()?;
        let mut groups: BTreeMap<Vec<OrdVal>, (Vec<Value>, Vec<Acc>)> = BTreeMap::new();
        let width = scope.len();
        let mut emit = |_: &mut K, row: &[Value]| -> Result<bool> {
            if let Some(f) = &filter {
                if eval(f, row, &[])?.truth() != Some(true) {
                    return Ok(true);
                }
            }
            let gk: Vec<OrdVal> = group_exprs
                .iter()
                .map(|e| eval(e, row, &[]).map(OrdVal))
                .collect::<Result<_>>()?;
            let entry = groups
                .entry(gk)
                .or_insert_with(|| (row.to_vec(), vec![Acc::default(); specs.len()]));
            for (acc, spec) in entry.1.iter_mut().zip(&specs) {
                acc.update(spec, row)?;
            }
            Ok(true)
        };
        if sources.is_empty() {
            emit(kv, &[])?;
        } else {
            join(kv, &sources, 0, &mut Vec::new(), &mut emit)?;
        }
        if groups.is_empty() && group_exprs.is_empty() {
            groups.insert(
                Vec::new(),
                (vec![Value::Null; width], vec![Acc::default(); specs.len()]),
            );
        }
        let mut seen: BTreeSet<Vec<OrdVal>> = BTreeSet::new();
        for (_, (first, accs)) in groups {
            let aggv: Vec<Value> = accs
                .iter()
                .zip(&specs)
                .map(|(a, s)| a.finish(s))
                .collect::<Result<_>>()?;
            if let Some(h) = &having {
                if eval(h, &first, &aggv)?.truth() != Some(true) {
                    continue;
                }
            }
            let out: Vec<Value> = items
                .iter()
                .map(|e| eval(e, &first, &aggv))
                .collect::<Result<_>>()?;
            if sel.distinct && !seen.insert(out.iter().cloned().map(OrdVal).collect()) {
                continue;
            }
            let k: Vec<Value> = keys
                .iter()
                .map(|e| eval(e, &first, &aggv))
                .collect::<Result<_>>()?;
            rows.push((k, out));
        }
    }

    if !order_terms.is_empty() {
        rows.sort_by(|(a, _), (b, _)| {
            for (i, (_, desc)) in order_terms.iter().enumerate() {
                let o = total_cmp(&a[i], &b[i]);
                let o = if *desc { o.reverse() } else { o };
                if o != Ordering::Equal {
                    return o;
                }
            }
            Ordering::Equal
        });
    }
    let rows: Vec<Vec<Value>> = rows
        .into_iter()
        .skip(offset)
        .take(limit.unwrap_or(usize::MAX))
        .map(|(_, r)| r)
        .collect();
    Ok(QueryResult {
        columns: names,
        rows,
        changes: 0,
    })
}
