# KvSQL against its neighbours

A benchmark of KvSQL against three Rust stores people pick for the same jobs, with SQLite as the reference: redb 4.4.0, fjall 3.1.12 and GlueSQL 0.20.0. `scripts/record-results.sh` runs it and saves the output as `test/results/latest/neighbours.md`; every figure quoted elsewhere comes from that file, with the host it names.

This is its own crate with its own `Cargo.lock`, outside the KvSQL workspace. The engine stays free of dependencies, and the KvSQL CI and release builds never compile the neighbours. A CI job builds and runs it at a small size so it keeps working; that run's numbers are not recorded.

```
cd bench/neighbours
cargo run --release --locked
KVSQL_NEIGHBOURS_ROWS=2000 KVSQL_NEIGHBOURS_COMMITS=20 cargo run --release --locked   # quick
```

## Method

- One machine, one thread, real files in one temp folder. The output names the host and the temp folder's file system. Record on Linux: on macOS Rust's `sync_data` uses F_FULLFSYNC (KvSQL, redb and fjall go through it) while SQLite only does that with `PRAGMA fullfsync`, so a macOS run would favour SQLite's commits.
- The same data in every store: 100,000 keys, each an 8-byte big-endian integer with a 13-byte value (`user-00000042`). The SQL stores also get 100,000 rows in `t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER)`.
- Cache: 8 MiB for KvSQL (2048 pages of 4 KiB), redb (`set_cache_size`), fjall (block cache) and SQLite (`cache_size=-8192`). GlueSQL's redb storage has no cache setting, so it runs with redb's default.
- fjall keeps new writes in a 64 MiB memtable by default, which would hold this whole data set in memory. After the load the benchmark flushes it to on-disk tables and compacts them (`rotate_memtable_and_wait`, `major_compact`), so fjall's reads go through its tables and block cache. After a major compaction the data sits in one fully compacted table, which is fjall's best case for reads. Both calls are hidden from fjall's documentation (`rotate_memtable_and_wait` is marked as used in tests), so they may change in a later release. The flush time is printed with the loads.
- Reads are warm and go in a shuffled order, every key once per round, and every value read is compared with the one written. Scans check their first key, every key in order and the count.
- Five rounds, interleaved: each round runs every store once, starting with a different store each round, so a slow stretch on a shared machine is spread over the stores. Each row gives the median round and the fastest and slowest round.
- Synced commits write one small value per transaction and wait for the sync: KvSQL `Db::put` (two `fdatasync` calls per commit), redb with `Durability::Immediate`, fjall `insert` then `persist(PersistMode::SyncData)`, GlueSQL autocommit on its redb storage, SQLite in WAL mode with `synchronous=FULL`.
- GlueSQL wraps every autocommit statement in a storage transaction, and its redb storage makes that a redb write transaction committed at redb's default `Durability::Immediate`. So an autocommit SELECT syncs to disk. The SQL table runs GlueSQL's selects inside one `BEGIN` ... `COMMIT` per round, which is its query path without the sync, and shows the autocommit form on a separate row over 500 keys.
- Sizes are read after all the writes, with every store closed. Both file length and allocated bytes are shown, because fjall preallocates its journal to 64 MiB as a sparse file.

## What is measured

| Operation | KvSQL | redb | fjall | GlueSQL | SQLite |
|---|---|---|---|---|---|
| Key get, one call per read | `Db::get` | read transaction per get | `Keyspace::get` | | prepared select on a `WITHOUT ROWID` table |
| Key get in one open snapshot | `ReadTx::get` | `ReadOnlyTable::get` | `Snapshot::get` | | |
| Range scan of 100 keys | `Db::scan` | `range` in a read transaction per scan | `Keyspace::range` | | prepared `WHERE k >= ? ORDER BY k LIMIT ?` |
| Point select by primary key in SQL | `Db::execute`, `Db::execute_prepared`, and `ReadTx::query`, `ReadTx::query_prepared` in one snapshot | | | `execute_with_params`, in a transaction and autocommit | prepared, and parsed each call |
| Synced single-key commit | yes | yes | yes | yes, a table row | yes |
| Bulk load in one transaction | keys and rows | keys | keys | rows | keys and rows |

## Where it isn't like for like

- fjall is an LSM tree. Its writes go to a journal and memory first and are compacted in the background on other threads, so a synced commit does less work at commit time and more later. It has no SQL.
- redb is a copy-on-write B-tree like KvSQL. It has no SQL.
- GlueSQL is a SQL engine over a choice of storages; here it runs on its redb storage, which pins redb 2.6, not 4.4. It has no prepared statements, so every call plans the statement again. Its bulk load goes through `INSERT` statements of 1000 rows, the others through API calls.
- KvSQL is shown both from text, where a cache of parsed statements for texts up to 2 KiB saves the parse, and prepared with `Db::prepare` (from 0.2), which also keeps the compiled plan.
- SQLite is written in C and has had decades of tuning. It is here as the reference the 0.1 benchmark already used.
- Sizes can't be compared across stores: KvSQL and SQLite hold the keys, the table and the space left by the synced commits (one key each); redb and fjall hold only the keys; GlueSQL only the table.
- None of this says how the stores behave with many threads, larger values, data bigger than the cache, or a different disk. The figures move by 20 to 30 percent between runs on a shared cloud machine, which is why each row shows its spread.

The recording states numbers only. What they mean for KvSQL is decided outside this file.
