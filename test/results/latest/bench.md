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
| KvSQL `get` on a plain keyspace | 1.13 µs | 888099 |
| KvSQL `get_row` on the table (key door) | 1.56 µs | 638978 |
| KvSQL `SELECT name FROM t WHERE id = ?` (SQL door) | 3.59 µs | 278242 |
| SQLite `SELECT v FROM kv WHERE k = ?`, prepared | 3.54 µs | 282646 |
| SQLite `SELECT name FROM t WHERE id = ?`, prepared | 2.66 µs | 376223 |
| SQLite same query, parsed every time | 5.04 µs | 198216 |

## Point reads from 2 threads at once

Each thread reads every key once in its own shuffled order. SQLite gets one connection per thread.

| Operation | Reads per second, all threads |
|---|---|
| KvSQL `get_row` | 786972 |
| KvSQL SQL point select | 331275 |
| SQLite prepared point select | 180134 |

## Range scan of 1000 consecutive keys

| Operation | Time per scan | Scans per second |
|---|---|---|
| KvSQL `scan` on a plain keyspace | 156.8 µs | 6379 |
| KvSQL SQL `WHERE id >= ? AND id < ?` | 605.7 µs | 1651 |
| SQLite same query, prepared | 346.6 µs | 2885 |

## Aggregate over the whole table

| Operation | Time |
|---|---|
| KvSQL `SELECT score, count(*) FROM t GROUP BY score` | 39.87 ms |
| SQLite same query | 27.27 ms |

## One small write per transaction, synced on every commit

| Operation | Time per commit | Commits per second |
|---|---|---|
| KvSQL `put` and commit | 509.1 µs | 1964 |
| KvSQL SQL `INSERT` and commit | 437.7 µs | 2284 |
| SQLite prepared `INSERT INTO kv`, autocommit | 237.4 µs | 4211 |
| SQLite prepared `INSERT INTO t`, autocommit | 225.5 µs | 4434 |

## Bulk load in one transaction, and file size

| Operation | KvSQL | SQLite |
|---|---|---|
| Load 100000 table rows | 425.49 ms | 87.48 ms |
| Load 100000 plain keys | 299.09 ms | 190.61 ms |
| File size after the writes above | 7.0 MiB | 5.5 MiB |

## Summary

- KvSQL `get_row` is 2.3x faster than the same row read through KvSQL SQL.
- KvSQL `get_row` against SQLite's prepared point select: 1.70x the speed. KvSQL `get` against SQLite's prepared KV lookup: 3.14x.
- KvSQL SQL point select against SQLite parsing each time: 1.40x the speed; against SQLite prepared: 0.74x.
- A figure above 1x means KvSQL is faster.
