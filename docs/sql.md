# SQL reference

KvSQL runs a subset of SQL close to SQLite with STRICT tables. Where a query falls inside the subset, it gives the same answer SQLite gives; the test suite checks this on random statements, including comparisons between text and numbers (see tests/sqlite_diff.rs).

## Running SQL

```rust
use kvsql_engine::{Db, sql::Value};

db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)", &[])?;
db.execute("INSERT INTO users VALUES (?, ?)", &[Value::Int(1), Value::Text("Ada".into())])?;
let r = db.execute("SELECT name FROM users WHERE id = ?", &[Value::Int(1)])?;
// r.columns == ["name"], r.rows == [[Text("Ada")]], r.changes == 0
```

- `Db::execute` runs one or more statements separated by `;`. Read-only statements run on a snapshot. Anything else runs in one write transaction that commits only if every statement succeeds.
- `WriteTx::execute` runs statements inside your own transaction. A statement that fails is undone on its own, and the transaction carries on.
- `ReadTx::query` runs SELECT on a read snapshot.
- `?` placeholders take values from the `params` slice, in order.

## Tables and the key-value door

A table is a keyspace with the same name. Every row is stored under its primary key:

| Primary key type | Key bytes |
|---|---|
| INTEGER | 8 bytes, big-endian, sign bit flipped, so byte order is number order |
| TEXT | the UTF-8 bytes |
| BLOB | the bytes themselves |

The value is the row encoded with `sql::encode_row`. `sql::key_for(&value)` gives the key for a primary key value.

Reaching a row without SQL:

- `db.get_row("users", &Value::Int(1))` reads by primary key straight from storage.
- `tx.put_row("users", &[...])` inserts or replaces a whole row. Types are checked and indexes are updated.
- `tx.delete_row("users", &Value::Int(1))` deletes by primary key and removes index entries.
- `tx.put(table, key, encode_row(...))` and `tx.delete(table, key)` also work on a table keyspace. The row must decode, fit the columns and carry the same primary key as `key`; indexes are kept.
- Dropping a table's keyspace through the KV API is refused; use DROP TABLE.

A keyspace with no table definition reads in SQL as a table with two BLOB columns, `key` and `value`. INSERT, UPDATE and DELETE work on it too; TEXT values are stored as their bytes.

Names starting with `__kvsql` are reserved. The catalog lives in `__kvsql_catalog` and each index in `__kvsql_index:<name>`. Unquoted names are lower-cased; quote a name to keep its case.

## Statements

```sql
CREATE TABLE [IF NOT EXISTS] name (col TYPE [PRIMARY KEY] [NOT NULL], ..., [PRIMARY KEY (col)]) [STRICT]
DROP TABLE [IF EXISTS] name
CREATE INDEX [IF NOT EXISTS] name ON table (col)
DROP INDEX [IF EXISTS] name
INSERT INTO table [(cols)] VALUES (...), (...)
UPDATE table SET col = expr, ... [WHERE expr]
DELETE FROM table [WHERE expr]
SELECT [DISTINCT] items FROM t [AS a] [[INNER|LEFT|CROSS] JOIN t2 ON expr | , t2] ...
  [WHERE expr] [GROUP BY exprs] [HAVING expr]
  [ORDER BY expr|position|alias [ASC|DESC], ...] [LIMIT n [OFFSET m]]
```

`GROUP BY` and `ORDER BY` take result column positions (`GROUP BY 1`) and aliases as well as expressions.

Every table needs exactly one primary key column, of type INTEGER, TEXT or BLOB. An INTEGER primary key left out or given as NULL gets the largest existing key plus one.

Types: INTEGER (also INT, BIGINT), REAL (FLOAT, DOUBLE), TEXT (VARCHAR), BLOB, ANY. Values are converted only when nothing is lost, as SQLite does for STRICT tables: 3.0 goes into an INTEGER column as 3, 3.5 is refused.

Not supported yet: subqueries, UNION, multi-column indexes or primary keys, ALTER TABLE, DEFAULT values, UNIQUE and CHECK constraints, column affinity for non-STRICT tables.

## Expressions

- Literals: integers, reals, 'text' (with '' for a quote), x'hex' blobs, NULL, TRUE, FALSE.
- Operators: `+ - * / %`, `||`, `= == != <> < <= > >=`, `IS`, `IS NOT`, `IS NULL`, `NOTNULL`, `AND`, `OR`, `NOT`, `IN (...)`, `BETWEEN`, `LIKE` (`%` and `_`, ASCII case-insensitive), `CASE`, `CAST(x AS type)`.
- Functions: length, lower, upper, abs, coalesce, ifnull, nullif, substr, typeof, round, min and max with two or more arguments, hex, instr, replace, trim, ltrim, rtrim.
- Aggregates: count(*), count(x), sum, total, avg, min, max, group_concat, each with optional DISTINCT.

NULL and number rules follow SQLite: comparisons with NULL are unknown, integer division truncates, division or remainder by zero gives NULL, integer overflow in `+ - *` gives a REAL, and SUM of integers that overflows is an error.

Comparisons between text and numbers follow SQLite's affinity rules. An INTEGER or REAL column has numeric affinity, a TEXT column text affinity, a BLOB or ANY column none, and `CAST(x AS type)` the affinity of its type. When one side of `=`, `<`, `IN`, `BETWEEN` and the like has numeric affinity, text on the other side that reads as a number is compared as that number, so `id = '7'` finds row 7. When one side has text affinity and the other none, numbers are compared as text. Values in an ANY column are not converted: there, 1 and 1.0 are equal but 1 and '1' are not.

An expression can be nested at most 250 levels deep, and at most 64 levels of parentheses, function calls and signs; a chain of more than 250 of the same operator, such as `a + b + ...`, counts as that deep. Deeper SQL is refused with an error. Lists joined by AND or OR and `x IN (...)` lists have no such limit.

## Sizes

- Primary keys: an INTEGER of any value, or TEXT or BLOB of 1 to 512 bytes. An empty text or blob key is refused.
- An indexed TEXT or BLOB value is stored in its index entry next to the row's key, and the two together must fit in 512 bytes. A longer value fails the INSERT or UPDATE, and CREATE INDEX fails on a table that already holds one.
- Row values: up to 1 GiB per row.

## How queries find rows

The planner looks at the WHERE terms joined by AND (and the ON terms of a join) and picks one of:

- primary key equal to a value, or IN a list: direct key lookups;
- primary key compared with `<`, `<=`, `>`, `>=` or BETWEEN: a key range scan;
- an indexed column equal to a value: an index lookup;
- otherwise a full scan.

For a join, each table after the first can be reached by primary key or index from columns of the tables before it. The full WHERE clause is always applied to the rows found, so the access path only decides how many rows are read.
