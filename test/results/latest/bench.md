# KvSQL benchmark

- Host: linux x86_64, Intel(R) Xeon(R) Processor @ 2.10GHz, 2 hardware threads
- KvSQL 0.2.0, SQLite 3.50.2 (bundled with rusqlite)
- 100000 rows in a table `t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER)` and 100000 keys in a plain keyspace (8-byte key, 13-byte value)
- Real files in the temp directory, 8 MiB page cache on both sides, one thread
- Reads: warm, shuffled key order, every key once per round, median of 5 rounds
- SQLite: WAL journal with synchronous=FULL, so each commit is synced like a KvSQL commit; the plain keyspace is compared with a `WITHOUT ROWID` table `kv (k BLOB PRIMARY KEY, v BLOB)`
- KvSQL SQL is shown both from text (its parsed-statement cache makes repeat texts cheap to parse) and prepared with `Db::prepare`, which also keeps the compiled plan. SQLite is shown both with a cached prepared statement and parsing each time

## Point reads by primary key

| Operation | Time per read | Reads per second |
|---|---|---|
| KvSQL `get` on a plain keyspace | 0.73 µs | 1369863 |
| KvSQL `get_row` on the table (key door) | 0.91 µs | 1095290 |
| KvSQL `SELECT name FROM t WHERE id = ?` (SQL door) | 1.86 µs | 537634 |
| KvSQL same query, prepared (`Db::execute_prepared`) | 1.32 µs | 758725 |
| SQLite `SELECT v FROM kv WHERE k = ?`, prepared | 1.75 µs | 571102 |
| SQLite `SELECT name FROM t WHERE id = ?`, prepared | 1.54 µs | 648929 |
| SQLite same query, parsed every time | 2.88 µs | 347826 |

## Point reads from 2 threads at once

Each thread reads every key once in its own shuffled order. SQLite gets one connection per thread.

| Operation | Reads per second, all threads |
|---|---|
| KvSQL `get_row` | 695500 |
| KvSQL SQL point select | 468227 |
| SQLite prepared point select | 229825 |

## Range scan of 1000 consecutive keys

| Operation | Time per scan | Scans per second |
|---|---|---|
| KvSQL `scan` on a plain keyspace | 107.9 µs | 9271 |
| KvSQL SQL `WHERE id >= ? AND id < ?` | 332.4 µs | 3009 |
| SQLite same query, prepared | 142.9 µs | 7000 |

## Aggregate over the whole table

| Operation | Time |
|---|---|
| KvSQL `SELECT score, count(*) FROM t GROUP BY score` | 25.72 ms |
| SQLite same query | 14.84 ms |

## One small write per transaction, synced on every commit

| Operation | Time per commit | Commits per second |
|---|---|---|
| KvSQL `put` and commit | 194.2 µs | 5150 |
| KvSQL SQL `INSERT` and commit | 213.6 µs | 4681 |
| SQLite prepared `INSERT INTO kv`, autocommit | 141.5 µs | 7070 |
| SQLite prepared `INSERT INTO t`, autocommit | 160.0 µs | 6249 |

## Bulk load in one transaction, and file size

| Operation | KvSQL | SQLite |
|---|---|---|
| Load 100000 table rows | 266.54 ms | 54.22 ms |
| Load 100000 plain keys | 189.92 ms | 128.14 ms |
| File size after the writes above | 7.0 MiB | 5.5 MiB |

## Summary

- KvSQL `get_row` is 2.0x faster than the same row read through KvSQL SQL.
- KvSQL `get_row` against SQLite's prepared point select: 1.69x the speed. KvSQL `get` against SQLite's prepared KV lookup: 2.40x.
- KvSQL SQL point select against SQLite parsing each time: 1.55x the speed; against SQLite prepared: 0.83x.
- KvSQL prepared point select against SQLite prepared: 1.17x the speed.
- A figure above 1x means KvSQL is faster.
