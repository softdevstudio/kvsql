# KvSQL benchmark

- Host: linux x86_64, Intel(R) Xeon(R) Processor @ 2.80GHz, 2 hardware threads
- KvSQL 0.1.0, SQLite 3.50.2 (bundled with rusqlite)
- 100000 rows in a table `t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER)` and 100000 keys in a plain keyspace (8-byte key, 13-byte value)
- Real files in the temp directory, 8 MiB page cache on both sides, one thread
- Reads: warm, shuffled key order, every key once per round, median of 5 rounds
- SQLite: WAL journal with synchronous=FULL, so each commit is synced like a KvSQL commit; the plain keyspace is compared with a `WITHOUT ROWID` table `kv (k BLOB PRIMARY KEY, v BLOB)`
- KvSQL has no prepared statements yet, so every KvSQL SQL call parses its text. SQLite is shown both with a cached prepared statement and parsing each time

## Point reads by primary key

| Operation | Time per read | Reads per second |
|---|---|---|
| KvSQL `get` on a plain keyspace | 1.46 µs | 687285 |
| KvSQL `get_row` on the table (key door) | 1.76 µs | 569152 |
| KvSQL `SELECT name FROM t WHERE id = ?` (SQL door) | 3.31 µs | 302024 |
| SQLite `SELECT v FROM kv WHERE k = ?`, prepared | 4.91 µs | 203832 |
| SQLite `SELECT name FROM t WHERE id = ?`, prepared | 4.08 µs | 245098 |
| SQLite same query, parsed every time | 6.08 µs | 164555 |

## Point reads from 2 threads at once

Each thread reads every key once in its own shuffled order. SQLite gets one connection per thread.

| Operation | Reads per second, all threads |
|---|---|
| KvSQL `get_row` | 582582 |
| KvSQL SQL point select | 237120 |
| SQLite prepared point select | 573623 |

## Range scan of 1000 consecutive keys

| Operation | Time per scan | Scans per second |
|---|---|---|
| KvSQL `scan` on a plain keyspace | 232.2 µs | 4307 |
| KvSQL SQL `WHERE id >= ? AND id < ?` | 776.1 µs | 1289 |
| SQLite same query, prepared | 438.1 µs | 2283 |

## Aggregate over the whole table

| Operation | Time |
|---|---|
| KvSQL `SELECT score, count(*) FROM t GROUP BY score` | 68.56 ms |
| SQLite same query | 47.59 ms |

## One small write per transaction, synced on every commit

| Operation | Time per commit | Commits per second |
|---|---|---|
| KvSQL `put` and commit | 5437.4 µs | 184 |
| KvSQL SQL `INSERT` and commit | 5386.3 µs | 186 |
| SQLite prepared `INSERT INTO kv`, autocommit | 307.4 µs | 3253 |
| SQLite prepared `INSERT INTO t`, autocommit | 276.2 µs | 3621 |

## Bulk load in one transaction, and file size

| Operation | KvSQL | SQLite |
|---|---|---|
| Load 100000 table rows | 489.71 ms | 132.51 ms |
| Load 100000 plain keys | 465.86 ms | 135.15 ms |
| File size after the writes above | 7.0 MiB | 5.5 MiB |

## Summary

- KvSQL `get_row` is 1.9x faster than the same row read through KvSQL SQL.
- KvSQL `get_row` against SQLite's prepared point select: 2.32x the speed. KvSQL `get` against SQLite's prepared KV lookup: 3.37x.
- KvSQL SQL point select against SQLite parsing each time: 1.84x the speed; against SQLite prepared: 1.23x.
- A figure above 1x means KvSQL is faster.
