# KvSQL against its neighbours

A benchmark of KvSQL against three Rust stores people pick for the same jobs, with SQLite as the reference: redb 4.4.0, fjall 3.1.12 and GlueSQL 0.20.0. `scripts/record-results.sh` runs it and saves the output as `test/results/latest/neighbours.md`; every figure quoted elsewhere comes from that file, with the host it names.

This is its own crate with its own `Cargo.lock`, outside the KvSQL workspace. The engine stays free of dependencies, and the KvSQL CI and release builds never compile the neighbours. A CI job builds and runs it at a small size so it keeps working; that run's numbers are not recorded.

```
cd bench/neighbours
cargo run --release --locked
KVSQL_NEIGHBOURS_ROWS=2000 KVSQL_NEIGHBOURS_COMMITS=20 cargo run --release --locked   # quick
```

## Method

- One machine, one thread, real files in one temp folder.
- The same data in every store: 100,000 keys, each an 8-byte big-endian integer with a 13-byte value (`user-00000042`). The SQL stores also get 100,000 rows in `t (id INTEGER PRIMARY KEY, name TEXT, score INTEGER)`.
- Cache: 8 MiB for KvSQL (2048 pages of 4 KiB), redb (`set_cache_size`), fjall (block cache) and SQLite (`cache_size=-8192`). GlueSQL's redb storage has no cache setting, so it runs with redb's default.
- Reads are warm and go in a shuffled order, every key once per round. Each figure is the median of five rounds.
- GlueSQL takes about a quarter of a millisecond per statement whatever the table size, so its SQL reads use the first 10,000 keys of the same shuffled order. That changes how long the run takes, not the time per read.
- Synced commits write one small value per transaction and wait for the sync: KvSQL `Db::put` (two `fdatasync` calls per commit), redb with `Durability::Immediate`, fjall `insert` then `persist(PersistMode::SyncData)`, GlueSQL autocommit on its redb storage, SQLite in WAL mode with `synchronous=FULL`.
- Sizes are read after all the writes, with every store closed. Both file length and allocated bytes are shown, because fjall preallocates its journal to 64 MiB as a sparse file.

## What is measured

| Operation | KvSQL | redb | fjall | GlueSQL | SQLite |
|---|---|---|---|---|---|
| Key get, one call per read | `Db::get` | read transaction per get | `Keyspace::get` | | prepared select on a `WITHOUT ROWID` table |
| Key get in one open read transaction | `ReadTx::get` | `ReadOnlyTable::get` | | | |
| Range scan of 100 keys | `Db::scan` | `range` in a read transaction per scan | `Keyspace::range` | | prepared `WHERE k >= ? ORDER BY k LIMIT ?` |
| Point select by primary key in SQL | `Db::execute` | | | `execute_with_params` | prepared, and parsed each call |
| Synced single-key commit | yes | yes | yes | yes, a table row | yes |
| Bulk load in one transaction | keys and rows | keys | keys | rows | keys and rows |

## Where it isn't like for like

- fjall is an LSM tree. Its writes go to a journal and memory first and are compacted in the background on other threads, so a synced commit does less work at commit time and more later. It has no SQL.
- redb is a copy-on-write B-tree like KvSQL. It has no SQL.
- GlueSQL is a SQL engine over a choice of storages; here it runs on its redb storage, which pins redb 2.6, not 4.4. It has no prepared statements, so every call plans the statement again.
- KvSQL has no prepared statements either; it keeps a cache of parsed statements for texts up to 2 KiB, which this benchmark hits.
- SQLite is written in C and has had decades of tuning. It is here as the reference the 0.1 benchmark already used.
- KvSQL and SQLite hold the keys and the table in one file; redb and fjall hold only the keys, GlueSQL only the table. File sizes are therefore a rough guide.
- None of this says how the stores behave with many threads, larger values, data bigger than the cache, or a different disk. The figures move by 20 to 30 percent between runs on a shared cloud machine.

The recording states numbers only. What they mean for KvSQL is decided outside this file.
