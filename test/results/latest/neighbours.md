# KvSQL and its neighbours

- Host: linux x86_64, Intel(R) Xeon(R) Processor @ 2.10GHz, 2 hardware threads
- Temp folder file system: ext2/ext3
- KvSQL 0.2.0, redb 4.4.0, fjall 3.1.12, GlueSQL 0.20.0 on its redb storage (which uses redb 2.6), SQLite 3.50.2 (bundled with rusqlite)
- 100000 keys (8-byte big-endian key, 13-byte value) and, for the SQL stores, 100000 rows in `t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER)`
- Real files in one temp folder, one thread. Cache: 8 MiB for KvSQL (2048 pages), redb, fjall (block cache) and SQLite; GlueSQL's redb storage has no cache setting and runs with redb's default
- fjall's data is flushed from its memtable to on-disk tables and compacted after the load, so its reads go through its tables and block cache
- Reads: warm, shuffled key order, every key once per round, and every value read is checked. 5 rounds, interleaved across the stores (each round runs every store once, starting with a different one); each row gives the median and the fastest and slowest round
- Scans: 500 scans of 100 consecutive keys from shuffled start keys, first key and count checked
- Synced commits: 200 per round, each one small write synced to disk before the call returns (KvSQL commit, redb Durability::Immediate, fjall insert then persist(SyncData), GlueSQL autocommit on redb, SQLite WAL with synchronous=FULL)

## Key get, one call per read

Each call reads one key through the store's plain key-value door and sees the latest commit (KvSQL `Db::get`, a redb read transaction per get, fjall `Keyspace::get`, SQLite autocommit prepared select).

| Store and call | Median per read | Fastest to slowest round | Per second (median) |
|---|---|---|---|
| KvSQL `Db::get` | 1.19 µs | 1.16 µs to 1.29 µs | 841751 |
| redb `begin_read`, `open_table`, `get` | 1.32 µs | 1.20 µs to 1.60 µs | 755858 |
| fjall `Keyspace::get` | 1.77 µs | 1.74 µs to 1.92 µs | 564334 |
| SQLite prepared `SELECT v FROM kv WHERE k = ?` | 2.29 µs | 2.17 µs to 2.44 µs | 437254 |

## Key get inside one open snapshot

All reads of a round go through one read transaction or snapshot, so the per-call snapshot cost is left out. SQLite is not in this table; its row above already reuses one prepared statement.

| Store and call | Median per read | Fastest to slowest round | Per second (median) |
|---|---|---|---|
| KvSQL `ReadTx::get` | 1.17 µs | 1.03 µs to 1.27 µs | 855432 |
| redb `ReadOnlyTable::get` | 0.78 µs | 0.75 µs to 0.84 µs | 1280410 |
| fjall `Snapshot::get` | 1.98 µs | 1.77 µs to 2.05 µs | 504032 |

## Range scan of 100 consecutive keys

| Store and call | Median per scan | Fastest to slowest round | Per second (median) |
|---|---|---|---|
| KvSQL `Db::scan` | 13.3 µs | 12.9 µs to 16.0 µs | 74957 |
| redb `range`, read transaction per scan | 6.52 µs | 5.30 µs to 7.17 µs | 153304 |
| fjall `Keyspace::range` | 21.3 µs | 19.8 µs to 24.4 µs | 46992 |
| SQLite prepared `WHERE k >= ? ORDER BY k LIMIT ?` | 22.9 µs | 20.7 µs to 29.1 µs | 43594 |

## Point select by primary key through SQL

`SELECT name FROM t WHERE id = ?` with the id as a parameter, and the name checked. KvSQL is shown from text (its parsed-statement cache makes a repeated text cheap to parse) and prepared (`Db::prepare`, which also keeps the compiled plan); GlueSQL has no prepared statements and plans every call. GlueSQL runs each round's selects inside one `BEGIN` ... `COMMIT`: on its redb storage an autocommit statement, a SELECT included, opens and commits a redb write transaction and so syncs to disk. That autocommit form is shown on its own row, over 500 keys per round. SQLite is shown prepared once and parsing every time. The last row reads the same row through KvSQL's key door.

| Store and call | Median per select | Fastest to slowest round | Per second (median) |
|---|---|---|---|
| KvSQL `Db::execute` | 2.65 µs | 2.58 µs to 2.86 µs | 377929 |
| KvSQL `Db::execute_prepared` | 1.86 µs | 1.73 µs to 1.89 µs | 538213 |
| KvSQL `ReadTx::query_prepared`, one snapshot per round | 1.81 µs | 1.75 µs to 1.91 µs | 552792 |
| KvSQL `ReadTx::query`, one snapshot per round | 2.60 µs | 2.50 µs to 2.77 µs | 384911 |
| GlueSQL `execute_with_params` inside one transaction | 16.3 µs | 15.6 µs to 17.1 µs | 61380 |
| GlueSQL `execute_with_params`, autocommit (syncs per statement) | 183.1 µs | 132.5 µs to 232.9 µs | 5460 |
| SQLite, prepared once | 2.10 µs | 1.93 µs to 2.36 µs | 475737 |
| SQLite, parsed every call | 3.41 µs | 3.17 µs to 4.08 µs | 293427 |
| KvSQL `Db::get_row` (key door, no SQL) | 1.41 µs | 1.28 µs to 1.50 µs | 708717 |

## One small write per transaction, synced on every commit

| Store and call | Median per commit | Fastest to slowest round | Per second (median) |
|---|---|---|---|
| KvSQL `Db::put` | 280.4 µs | 206.1 µs to 324.6 µs | 3567 |
| redb write transaction, `Durability::Immediate` | 328.4 µs | 247.6 µs to 392.6 µs | 3045 |
| fjall `insert` then `persist(SyncData)` | 130.6 µs | 126.5 µs to 146.3 µs | 7660 |
| GlueSQL `INSERT INTO t`, autocommit | 378.4 µs | 312.3 µs to 481.9 µs | 2643 |
| SQLite prepared `INSERT INTO kv`, autocommit | 161.3 µs | 125.8 µs to 182.0 µs | 6200 |

## Bulk load in one transaction, and size on disk

Loads are timed once, before the tables above. KvSQL, redb, fjall and SQLite load through their APIs one key or row at a time; GlueSQL takes 100 `INSERT` statements of up to 1000 rows each in one transaction. fjall's load is one write batch and a sync; flushing it to on-disk tables and compacting them took 74.47 ms more.

Sizes are read after every write above, with each store closed (fjall: its whole folder; SQLite: the database file and its WAL). File length is what a directory listing shows; allocated is what the file system holds (fjall preallocates its journal to 64 MiB as a sparse file). The stores hold different things, so sizes can't be compared across rows: KvSQL and SQLite hold the keys, the table and space left by 1000 single-write commits; redb and fjall hold only the keys; GlueSQL only the table.

| Store | Load 100000 keys | Load 100000 table rows | File length | Allocated |
|---|---|---|---|---|
| KvSQL | 200.84 ms | 271.51 ms | 6.9 MiB | 6.9 MiB |
| redb | 75.46 ms | | 4.0 MiB | 2.9 MiB |
| fjall | 56.77 ms | | 64.9 MiB | 5.0 MiB |
| GlueSQL | | 727.78 ms | 32.8 MiB | 18.3 MiB |
| SQLite | 66.23 ms | 74.45 ms | 5.4 MiB | 5.4 MiB |
