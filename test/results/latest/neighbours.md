# KvSQL and its neighbours

- Host: linux x86_64, Intel(R) Xeon(R) Processor @ 2.10GHz, 2 hardware threads
- Temp folder file system: ext2/ext3
- KvSQL 0.1.0, redb 4.4.0, fjall 3.1.12, GlueSQL 0.20.0 on its redb storage (which uses redb 2.6), SQLite 3.50.2 (bundled with rusqlite)
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
| KvSQL `Db::get` | 1.32 µs | 1.23 µs to 1.55 µs | 759878 |
| redb `begin_read`, `open_table`, `get` | 1.44 µs | 1.37 µs to 1.81 µs | 694444 |
| fjall `Keyspace::get` | 2.33 µs | 1.81 µs to 3.06 µs | 429738 |
| SQLite prepared `SELECT v FROM kv WHERE k = ?` | 2.73 µs | 2.64 µs to 3.28 µs | 366166 |

## Key get inside one open snapshot

All reads of a round go through one read transaction or snapshot, so the per-call snapshot cost is left out. SQLite is not in this table; its row above already reuses one prepared statement.

| Store and call | Median per read | Fastest to slowest round | Per second (median) |
|---|---|---|---|
| KvSQL `ReadTx::get` | 1.17 µs | 1.06 µs to 1.59 µs | 856898 |
| redb `ReadOnlyTable::get` | 0.86 µs | 0.79 µs to 1.00 µs | 1162791 |
| fjall `Snapshot::get` | 1.92 µs | 1.78 µs to 2.64 µs | 521376 |

## Range scan of 100 consecutive keys

| Store and call | Median per scan | Fastest to slowest round | Per second (median) |
|---|---|---|---|
| KvSQL `Db::scan` | 15.0 µs | 13.0 µs to 18.7 µs | 66450 |
| redb `range`, read transaction per scan | 6.14 µs | 5.38 µs to 7.53 µs | 162734 |
| fjall `Keyspace::range` | 20.6 µs | 20.1 µs to 25.5 µs | 48497 |
| SQLite prepared `WHERE k >= ? ORDER BY k LIMIT ?` | 20.1 µs | 18.8 µs to 24.5 µs | 49657 |

## Point select by primary key through SQL

`SELECT name FROM t WHERE id = ?` with the id as a parameter, and the name checked. KvSQL and GlueSQL parse the text on every call (KvSQL keeps a parsed-statement cache; GlueSQL has no prepared statements). GlueSQL runs each round's selects inside one `BEGIN` ... `COMMIT`: on its redb storage an autocommit statement, a SELECT included, opens and commits a redb write transaction and so syncs to disk. That autocommit form is shown on its own row, over 500 keys per round. SQLite is shown prepared once and parsing every time. The last row reads the same row through KvSQL's key door.

| Store and call | Median per select | Fastest to slowest round | Per second (median) |
|---|---|---|---|
| KvSQL `Db::execute` | 3.29 µs | 2.49 µs to 3.78 µs | 303674 |
| KvSQL `ReadTx::query`, one snapshot per round | 2.68 µs | 2.50 µs to 2.85 µs | 373692 |
| GlueSQL `execute_with_params` inside one transaction | 18.0 µs | 17.2 µs to 20.1 µs | 55445 |
| GlueSQL `execute_with_params`, autocommit (syncs per statement) | 205.3 µs | 187.3 µs to 230.0 µs | 4872 |
| SQLite, prepared once | 2.29 µs | 1.95 µs to 2.86 µs | 437637 |
| SQLite, parsed every call | 3.75 µs | 3.37 µs to 4.28 µs | 266667 |
| KvSQL `Db::get_row` (key door, no SQL) | 1.54 µs | 1.37 µs to 1.93 µs | 648088 |

## One small write per transaction, synced on every commit

| Store and call | Median per commit | Fastest to slowest round | Per second (median) |
|---|---|---|---|
| KvSQL `Db::put` | 338.5 µs | 331.8 µs to 538.8 µs | 2954 |
| redb write transaction, `Durability::Immediate` | 341.4 µs | 202.6 µs to 486.5 µs | 2929 |
| fjall `insert` then `persist(SyncData)` | 129.9 µs | 115.8 µs to 150.2 µs | 7699 |
| GlueSQL `INSERT INTO t`, autocommit | 417.6 µs | 264.8 µs to 616.6 µs | 2395 |
| SQLite prepared `INSERT INTO kv`, autocommit | 180.4 µs | 127.4 µs to 225.4 µs | 5542 |

## Bulk load in one transaction, and size on disk

Loads are timed once, before the tables above. KvSQL, redb, fjall and SQLite load through their APIs one key or row at a time; GlueSQL takes 100 `INSERT` statements of up to 1000 rows each in one transaction. fjall's load is one write batch and a sync; flushing it to on-disk tables and compacting them took 114.98 ms more.

Sizes are read after every write above, with each store closed (fjall: its whole folder; SQLite: the database file and its WAL). File length is what a directory listing shows; allocated is what the file system holds (fjall preallocates its journal to 64 MiB as a sparse file). The stores hold different things, so sizes can't be compared across rows: KvSQL and SQLite hold the keys, the table and space left by 1000 single-write commits; redb and fjall hold only the keys; GlueSQL only the table.

| Store | Load 100000 keys | Load 100000 table rows | File length | Allocated |
|---|---|---|---|---|
| KvSQL | 203.86 ms | 242.45 ms | 6.9 MiB | 6.9 MiB |
| redb | 84.43 ms | | 4.0 MiB | 2.9 MiB |
| fjall | 61.39 ms | | 64.9 MiB | 5.0 MiB |
| GlueSQL | | 714.83 ms | 32.8 MiB | 18.3 MiB |
| SQLite | 96.20 ms | 91.85 ms | 5.4 MiB | 5.4 MiB |
