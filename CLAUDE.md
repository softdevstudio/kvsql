# KvSQL

KvSQL is a native hybrid of key-value and SQL in Rust, built as two products on one set of records: KvSQL Engine, an embeddable library crate, and KvSQL DB, a database server with a command-line shell built on the engine.

Domain: kvsql.com
Code repository: softdevstudio/kvsql
Site repository: photozeitgeist/kvsql.com
Language: Rust
Code license: Apache 2.0
Contact: info@kvsql.com

## Low maintenance workflow

YK supplies the go-ahead, language choice, repositories and access. The agent handles code, tests, documentation, CI, automated releases, site work and upkeep. Answer questions without treating them as instructions to implement. Update Next only when YK approves a new step; record actual progress without inventing approval.

Work on main only; main is the default branch. This CLAUDE.md is the sole plan and session handoff file. Do not create ROADMAP.md, RESUME.md or another status file. Session notes go in Where we are, never as files in docs/; docs/ holds reference material only. Pause and report after each session and wait for YK's go-ahead before the next.

README.md is the manual and plain-language explainer. Reference documentation belongs in docs/ as Markdown. No Word or PDF. Keep LICENSE, NOTICE and THIRD_PARTY_LICENSES.md current. KvSQL is an entirely separate project: never mix in code, names, modules or content from earlier projects. Do not discuss domain sale or domain value anywhere in project materials.

Write in plain words with varied sentence length. Avoid em dashes, formulaic contrasts, decorative vocabulary and forced lists of three.

## Screening record

Agreed on October 22, 2026.

Money: not in selling a database. Candidate paths around an Apache 2.0 core are a managed or sync service, commercial support and paid modules such as replication, encryption and tooling. Rust teams pick embedded stores carefully, so adoption comes first. No revenue model is approved.

Real problem: apps that need fast key lookups and real queries usually run two stores, such as RocksDB or Redis for keys and SQLite for queries. That means two copies of the data, sync code between them and transactions that cannot span both.

Plain-language technology: a key-value store is a coat check, give a ticket and get the coat back. SQL is asking the attendant for every blue coat left after 9pm. KvSQL is one cloakroom with both windows: the ticket window skips the query machinery, the question window goes through it, and both see the same coats at the same moment.

Differentiation: SQL plus KV alone is not a moat; GlueSQL, Turso's Rust rewrite of SQLite, redb and fjall are close neighbours. The aim is a direct KV path with no SQL parsing, sharing storage, transactions and indexes with SQL, kept unusually small. That has to be proven with recorded benchmarks against those neighbours, not claimed. IoT and edge gateways remain a named use case alongside general embedded use.

## Design

Every SQL table is also a keyspace: `get("users", key)` reads the row by primary key straight from storage, and `SELECT ... WHERE id = ...` reaches the same bytes through SQL. Plain keyspaces need no schema and read in SQL as a two-column table (key, value). One transaction covers both doors, and secondary indexes stay correct whichever door wrote the row.

Storage is a single file holding a copy-on-write B+tree per keyspace, a directory tree of keyspace roots, a persisted free list and two alternating meta pages. One writer at a time. The file format is in docs/file-format.md.

The engine crate has no external dependencies. Release binaries target Linux x86_64 and arm64, macOS arm64 and x86_64, and Windows x86_64. One release family, KvSQL vX.Y.Z, carries both binaries; the engine is used as a Git dependency (crates.io publishing would need YK's token and is not planned).

## Code and completion standard

Tests for every operation, plus failure tests aimed at the main promise: no lost or torn commits. The simulated disk in crates/kvsql-engine/src/sim.rs drops, reorders and tears unsynced writes at any point of a commit; tests/kill_writer.rs SIGKILLs a real writer process. Record results with scripts/record-results.sh, which replaces test/results/latest; git history keeps earlier runs. Every number on the site comes from recorded results with its conditions.

CI runs on every push to main on Linux and macOS: rustfmt, clippy with warnings denied, all tests in release mode. Before calling a release complete, get an independent review from a separate agent that has not seen the implementation, fix findings, rerun checks and record again.

Releases go out through an automated workflow after CI succeeds on the exact commit (session 5). Never ask YK to publish a release.

## Site standard

photozeitgeist/kvsql.com uses the unchanged Hugo Launch template. It currently holds a copy of another project's site; on the site session keep themes/ and archetypes/ byte for byte and replace every page, post, image and config value. Content and configuration values only: no new layouts, CSS, _index.md files or public/ folder. Pages: about, contact (info@kvsql.com), quick start, download and reference. Posts: launch, plain-language explainer, how it was tested and the name. Post front matter: quoted title in normal title case with no special characters, date only, draft: false, tags; no description or categories; never repeat the title in the body; featured_image plus the same image inline after the second or third paragraph when a post has one. Images drawn for the project live in static/images.

## Development plan

Approved by YK on October 22, 2026. Pause and report after each session.

1. Foundation: Cargo workspace, license files, CI, recording script, file format, pager, B+tree, KV get/put/delete/range scan, crash tests that kill a writer thousands of times.
2. Transactions: atomic commit, snapshot readers that run beside the writer, recovery, and tests aimed at lost or torn commits.
3. SQL: parser and catalog, CREATE/INSERT/SELECT/UPDATE/DELETE, WHERE, ORDER BY, LIMIT, aggregates, GROUP BY and secondary indexes on the shared records; random SQL checked against SQLite for identical answers.
4. KvSQL DB: kvsqld with an HTTP/JSON API for KV and SQL, the kvsql shell, concurrent clients, tests that kill the server mid-load.
5. Release: benchmarks against SQLite (key reads through get against the same read through SQL), recorded results, independent review, fixes, the automated release workflow and v0.1.0 published by CI.
6. Site: kvsql.com rebuilt on the Launch template with numbers from the recorded results.

## Where we are

October 22, 2026, session 1 (foundation) done:

- Workspace with one crate, crates/kvsql-engine, no dependencies. Modules: page (header, CRC-32C seal and check), storage (FileStorage with an exclusive file lock, Unix and Windows positional I/O), tree (copy-on-write B+tree, overflow values, merge on underflow, verify walker), db (Db, WriteTx, commit, open and recovery, free list, page cache, verify), sim (simulated disk and a seeded RNG for tests, doc-hidden).
- API: Db::open, open_with, get, scan(from, to, limit), keyspaces, put, delete, begin_write; WriteTx get, scan, put (creates the keyspace), delete, drop_keyspace, commit; Db::verify walks every page and accounts for all of them. Keys 1 to 512 bytes, values up to 1 GiB.
- A failed commit poisons the handle; reopening recovers the last good state.
- Tests: tests/kv.rs (operations, edges, model check against a BTreeMap with verify after every commit, deep tree grow and shrink, page reuse, file lock, damaged pages), tests/power_cut.rs, tests/kill_writer.rs. Mutation checks during development: removing the sync before the meta write, or reusing pages still reachable from the last commit, both fail power_cut at once.
- Rough single-thread numbers on the dev container (examples/quick_bench.rs, in-memory disk, 60,000 keys in one transaction): about 4 µs per insert and under 1 µs per get. Not recorded, not for the site.

October 22, 2026, session 2 (transactions) done:

- db.rs split into Shared state: storage, a 16-shard page cache, committed State behind a short mutex, and a separate writer mutex held for a write transaction's life. ReadTx (Db::begin_read) pins the commit it started on; Db::get, scan and keyspaces use short read transactions. WriteTx gained keyspaces().
- Free pages carry the commit that freed them (Free { id, freed_at }). begin_write splits them into reusable (freed_at <= oldest open snapshot) and held. The tag is in memory only; on open everything listed is reusable.
- Storage now requires Send + Sync.
- Tests: tests/snapshots.rs (isolation, reader does not block writer, open reader holds pages then releases them, four reader threads checking a constant total and self-checking values twice per snapshot while a writer moves money and churns junk keys). power_cut.rs holds readers across commits in half the rounds and checks their view after each commit. Mutation checks: ignoring open readers when reusing pages, and tagging freed pages one commit early, both fail the churn test at once.
- Rough numbers on the dev container (not recorded): about 5 µs per insert and 1.4 µs per get. Get got slower than session 1 because each one now registers a snapshot under the state mutex; worth revisiting in session 5 if benchmarks show contention.

Next session starts at session 3: SQL. Tables stored as keyspaces with rows keyed by primary key, a catalog keyspace, parser, executor, secondary indexes kept in the same transaction, and random SQL compared against SQLite (rusqlite as a dev-dependency only).

## Next

Session 3, after YK's go-ahead.
