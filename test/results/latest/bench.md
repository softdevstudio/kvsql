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
| KvSQL `get` on a plain keyspace | 1.67 µs | 598802 |
| KvSQL `get_row` on the table (key door) | 2.32 µs | 430478 |
| KvSQL `SELECT name FROM t WHERE id = ?` (SQL door) | 2.84 µs | 351617 |
| SQLite `SELECT v FROM kv WHERE k = ?`, prepared | 3.09 µs | 323206 |
| SQLite `SELECT name FROM t WHERE id = ?`, prepared | 3.55 µs | 282008 |
| SQLite same query, parsed every time | 5.46 µs | 183318 |

## Point reads from 2 threads at once

Each thread reads every key once in its own shuffled order. SQLite gets one connection per thread.

| Operation | Reads per second, all threads |
|---|---|
| KvSQL `get_row` | 665071 |
| KvSQL SQL point select | 331054 |
| SQLite prepared point select | 176392 |

## Range scan of 1000 consecutive keys

| Operation | Time per scan | Scans per second |
|---|---|---|
| KvSQL `scan` on a plain keyspace | 152.5 µs | 6558 |
| KvSQL SQL `WHERE id >= ? AND id < ?` | 630.6 µs | 1586 |
| SQLite same query, prepared | 231.6 µs | 4317 |

## Aggregate over the whole table

| Operation | Time |
|---|---|
| KvSQL `SELECT score, count(*) FROM t GROUP BY score` | 41.14 ms |
| SQLite same query | 34.22 ms |

## One small write per transaction, synced on every commit

| Operation | Time per commit | Commits per second |
|---|---|---|
| KvSQL `put` and commit | 488.5 µs | 2047 |
| KvSQL SQL `INSERT` and commit | 491.8 µs | 2033 |
| SQLite prepared `INSERT INTO kv`, autocommit | 219.2 µs | 4563 |
| SQLite prepared `INSERT INTO t`, autocommit | 204.9 µs | 4881 |

## Bulk load in one transaction, and file size

| Operation | KvSQL | SQLite |
|---|---|---|
| Load 100000 table rows | 404.79 ms | 74.92 ms |
| Load 100000 plain keys | 343.25 ms | 156.95 ms |
| File size after the writes above | 7.0 MiB | 5.5 MiB |

## Summary

- KvSQL `get_row` is 1.2x faster than the same row read through KvSQL SQL.
- KvSQL `get_row` against SQLite's prepared point select: 1.53x the speed. KvSQL `get` against SQLite's prepared KV lookup: 1.85x.
- KvSQL SQL point select against SQLite parsing each time: 1.92x the speed; against SQLite prepared: 1.25x.
- A figure above 1x means KvSQL is faster.
