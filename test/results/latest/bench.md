# KvSQL benchmark

- Host: linux x86_64, Intel(R) Xeon(R) Processor @ 2.10GHz, 2 hardware threads
- KvSQL 0.1.0, SQLite 3.50.2 (bundled with rusqlite)
- 100000 rows in a table `t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER)` and 100000 keys in a plain keyspace (8-byte key, 13-byte value)
- Real files in the temp directory, 8 MiB page cache on both sides, one thread
- Reads: warm, shuffled key order, every key once per round, median of 5 rounds
- SQLite: WAL journal with synchronous=FULL, so each commit is synced like a KvSQL commit; the plain keyspace is compared with a `WITHOUT ROWID` table `kv (k BLOB PRIMARY KEY, v BLOB)`
- KvSQL has no prepared statements yet, so every KvSQL SQL call parses its text. SQLite is shown both with a cached prepared statement and parsing each time

## Point reads by primary key

| Operation | Time per read | Reads per second |
|---|---|---|
| KvSQL `get` on a plain keyspace | 0.75 µs | 1340483 |
| KvSQL `get_row` on the table (key door) | 1.02 µs | 979432 |
| KvSQL `SELECT name FROM t WHERE id = ?` (SQL door) | 2.25 µs | 445434 |
| SQLite `SELECT v FROM kv WHERE k = ?`, prepared | 1.84 µs | 544366 |
| SQLite `SELECT name FROM t WHERE id = ?`, prepared | 1.52 µs | 657895 |
| SQLite same query, parsed every time | 3.15 µs | 317662 |

## Point reads from 2 threads at once

Each thread reads every key once in its own shuffled order. SQLite gets one connection per thread.

| Operation | Reads per second, all threads |
|---|---|
| KvSQL `get_row` | 649595 |
| KvSQL SQL point select | 435235 |
| SQLite prepared point select | 224350 |

## Range scan of 1000 consecutive keys

| Operation | Time per scan | Scans per second |
|---|---|---|
| KvSQL `scan` on a plain keyspace | 139.0 µs | 7193 |
| KvSQL SQL `WHERE id >= ? AND id < ?` | 527.5 µs | 1896 |
| SQLite same query, prepared | 243.3 µs | 4110 |

## Aggregate over the whole table

| Operation | Time |
|---|---|
| KvSQL `SELECT score, count(*) FROM t GROUP BY score` | 34.25 ms |
| SQLite same query | 22.86 ms |

## One small write per transaction, synced on every commit

| Operation | Time per commit | Commits per second |
|---|---|---|
| KvSQL `put` and commit | 423.9 µs | 2359 |
| KvSQL SQL `INSERT` and commit | 670.0 µs | 1493 |
| SQLite prepared `INSERT INTO kv`, autocommit | 257.2 µs | 3888 |
| SQLite prepared `INSERT INTO t`, autocommit | 180.3 µs | 5545 |

## Bulk load in one transaction, and file size

| Operation | KvSQL | SQLite |
|---|---|---|
| Load 100000 table rows | 246.99 ms | 62.77 ms |
| Load 100000 plain keys | 215.54 ms | 90.47 ms |
| File size after the writes above | 7.0 MiB | 5.5 MiB |

## Summary

- KvSQL `get_row` is 2.2x faster than the same row read through KvSQL SQL.
- KvSQL `get_row` against SQLite's prepared point select: 1.49x the speed. KvSQL `get` against SQLite's prepared KV lookup: 2.46x.
- KvSQL SQL point select against SQLite parsing each time: 1.40x the speed; against SQLite prepared: 0.68x.
- A figure above 1x means KvSQL is faster.
