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

The engine crate has no external dependencies (rusqlite is a dev-dependency for tests only). Release binaries target Linux x86_64 and arm64, macOS arm64 and x86_64, and Windows x86_64. One release family, KvSQL vX.Y.Z, carries both binaries; the engine is used as a Git dependency (crates.io publishing would need YK's token and is not planned).

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

October 22, 2026, session 3 (SQL) done:

- crates/kvsql-engine/src/sql: value.rs (Value, SQLite ordering, row encoding, primary-key and index-key encodings), parser.rs (tokenizer, AST, recursive descent), catalog.rs (table and index definitions in __kvsql_catalog, index keyspaces __kvsql_index:<name>, plain keyspaces as key/value tables), eval.rs (binding, SQLite NULL and number rules, functions, aggregates), exec.rs (access paths, nested-loop joins with key and index lookups, grouping, ordering, shared row writes), mod.rs (API glue, KV door hooks, verify).
- API: Db::execute, Db::get_row, ReadTx::query and get_row, WriteTx::execute, get_row, put_row, delete_row. WriteTx put, delete and drop_keyspace now check table keyspaces (rows must fit, indexes kept, reserved names refused). Statements roll back alone through a savepoint (clones of the transaction's page maps).
- Db::verify also checks every table row against its definition and every index entry against its rows.
- Tests: tests/sql.rs (statements, constraints, statement rollback, joins, SQLite expression rules, the two doors on the same rows, plain keyspaces, snapshots, error messages); tests/sqlite_diff.rs (random statements on KvSQL and SQLite STRICT tables, answers, change counts and errors compared; rusqlite is a dev-dependency only); power_cut.rs gained SQL writes with two indexes under power cuts. 120,000 random statements over six seeds matched SQLite during development. Mutation checks: an off-by-one key range bound and stale index entries were both caught.
- Rough numbers on the dev container (not recorded): get_row 1.7 µs, the same read through SQL 7.8 µs, put_row 13 µs, GROUP BY over 50,000 rows 18 ms.

October 22, 2026, session 4 (KvSQL DB) done:

- Second crate crates/kvsql-db, std only, depends on the engine by path. Modules: json (parser with a depth limit, writer, base64, Value mapping with BLOB as {"$blob": base64}), http (HTTP/1.1 request reader with keep-alive and Content-Length only, response writer, percent coding, a small keep-alive client), server (routes, error mapping, bearer token compared in constant time, thread per connection with a cap), shell (Backend over a server or a local file, dot commands, text tables, a multi-line script reader).
- Binaries: kvsqld (--db, --listen, --token or KVSQL_TOKEN, --max-connections; prints its address on the first stdout line) and kvsql (--url or KVSQL_URL, --file, --token, repeatable -c, script input on stdin, exit 1 on any failure).
- API: GET /health, GET /verify, GET /kv, GET /kv/{ks}?from&to&limit, GET/PUT/DELETE /kv/{ks}/{key}, GET /rows/{t}/{pk}, PUT /rows/{t}, DELETE /rows/{t}/{pk}, POST /sql. Each request is one transaction and writes answer after commit. Reference in docs/server.md; README has a server and shell section.
- Tests: tests/api.rs (endpoints, statuses, binary keys, tokens, limits, malformed and pipelined requests), tests/server_load.rs (six writer and four reader connections with a constant money total; a real kvsqld SIGKILLed under load from four clients, then every acknowledged write checked in the file), tests/shell.rs (file and server mode, -c, stdin scripts, exit codes, kvsqld flags). record-results.sh gained KVSQL_SERVER_SECS (10) and KVSQL_SERVER_KILLS (100). Mutation check: a server that acknowledges some puts without writing them fails the kill test in the first round.
- Recorded run (test/results/latest): 100 kvsqld SIGKILLs under load with 13,900 acknowledged writes all present afterwards; 10 s of six writer and four reader connections (5,451 transfer rounds, 113,335 total checks); all engine suites at the session 3 settings passed. Throughput there is a by-product of a stress test and not a benchmark.

October 22, 2026, session 5 (release) done:

- Speed work driven by a callgrind profile and the new benchmark: statement savepoints became an undo log (they used to clone the whole page map per statement), hardware CRC-32C on x86_64 and aarch64 with a slicing-by-8 fallback, a clock page cache with a page-id hasher, table definitions shared across transactions per schema generation, a parsed-statement cache for SQL texts up to 2 KiB, full pages on in-order inserts (fill_left splits), sync_data and consecutive pages written in one call. The power-cut tests now aim their cuts at the shorter commits.
- crates/kvsql-engine/examples/bench.rs: KvSQL against SQLite 3.50.2 on real files with equal 8 MiB caches; record-results.sh saves it as test/results/latest/bench.md. Recorded on the dev container (2 vCPU, noisy, so figures move by 20 to 30 percent between runs): get 1.67 µs, get_row 2.32 µs, KvSQL SQL point select 2.84 µs, SQLite prepared point select 3.55 µs, SQLite parsing each time 5.46 µs; synced single-row commits about 0.5 ms for KvSQL against about 0.2 ms for SQLite WAL (KvSQL syncs twice per commit); bulk load and GROUP BY slower than SQLite. CI uploads the same benchmark from its Linux and macOS runners as artifacts.
- Independent review by a separate agent found and confirmed: deep SQL crashing kvsqld (stack overflow), a failed KV-door put leaving an index out of step, ANY/REAL index lookups disagreeing with scans (1 vs 1.0, 0.0 vs -0.0), GROUP BY 1 not read as a position, small foreign files overwritten on open, the statement cache holding huge texts, comparison affinity missing, slow clients holding connection slots, plus suspicions about free-list ids, free_tree depth and read_value reservations. All fixed: expression depth limits (250 tree, 64 nesting), KV-door put/delete/drop_keyspace all-or-nothing, one numeric index encoding, GROUP BY positions, foreign-file check, 2 KiB cache cap, SQLite comparison affinity in the binder (the planner only uses key or index paths on unconverted columns), a 30 s whole-request deadline and write timeout with 8 MiB server thread stacks, free-list id checks, depth guard and capped reservation. Tests for each, and the SQLite differential test now compares text with numbers and covers an ANY/REAL indexed table and GROUP BY positions; disabling affinity or reverting the index encoding fails it. The reviewer re-ran its probes on the fixes: all eight confirmed fixed. Its re-check found two side effects, both fixed: long AND/OR lists hit the depth limit (they are now built as balanced trees), and KV-door puts slowed with the number of keyspaces touched (savepoints now log root changes instead of copying the root map).
- CI and the release workflow pin Rust 1.97.0, the toolchain results are recorded with (CI on the floating stable failed in seconds and its logs were unreachable from the session).
- .github/workflows/release.yml: started by a successful CI run on main; when the workspace version has no tag it builds kvsqld and kvsql for Linux x86_64 and arm64, macOS arm64 and x86_64 (macos-15-intel) and Windows x86_64, runs the engine tests and a smoke test (file mode, server, curl, shell against the server) on each, then tags vX.Y.Z and publishes "KvSQL vX.Y.Z" with SHA256SUMS. Archive names carry no version. A manual run with a tag rebuilds an existing release's files.
- v0.1.0 published by the workflow on commit cfc1310 after CI passed: all five builds passed their engine tests and smoke tests, release "KvSQL v0.1.0" (not a pre-release) with five archives and SHA256SUMS. The Linux x86_64 archive was downloaded through releases/latest/download, its checksum matched, and its binaries ran.
- Open points for later: benchmarks against the Rust neighbours (redb, fjall, GlueSQL) named in the screening record; Windows readers may wait behind a commit's flush because synchronous file handles serialise I/O there (delay only, not wrong data); single-sync commits would close most of the commit gap with SQLite.

October 22, 2026, session 6 (site) done, at YK's word "go, including case studies and use examples, listing and notes to softdevstudio.com":

- crates/kvsql-db/examples/case_studies.rs: the site's three case studies (sessions and reporting on one table, an edge gateway with kvsqld over HTTP, a checkout mixing both doors in one transaction with a power cut at every step of its commit, each cut replayed 200 ways) and three use examples (cache with expiry, settings in a plain keyspace, the shell). record-results.sh saves its output as test/results/latest/case-studies.md, recorded in the same run as checks.log and bench.md.
- photozeitgeist/kvsql.com rebuilt: themes/, archetypes/, .gitignore and content/archive/_index.md left as they were (the leftover root README.md, a copy of the theme manual with the old site's examples, was deleted afterwards at YK's word "Delete all leftovers"; the only trace left is the copyright line in themes/launch/LICENSE.md, part of the unchanged template); every other page, post, image, the old AltSql demo apps and every config value replaced. Pages: about, contact, quick start, download, reference. Posts: launch, explainer, how it was tested, the name, the SQLite benchmark, three case studies (tag "case studies") and three use examples (tag "use examples"), with menu entries to both tag pages. Eight images drawn from the recorded results in static/images. Built with Hugo 0.147.9 (the live sites' version), every internal link checked, titles free of special characters, and the live feed confirmed the URLs after deploy. The quick start snippets were compiled and run against the engine.
- softdevstudio.com: KvSQL added as the twenty-seventh project (data/studio.toml entry, CONTENT-PLAN brief, names, tag, footer, feeds row, session log row 2m, NEXT SESSION) and a short note on 0.1.0 linking the kvsql.com posts and the release.

The approved six-session plan is complete. Open points for later, each needing YK's approval as a new step: benchmarks against redb, fjall and GlueSQL; a single-sync commit; Windows readers waiting behind a commit's flush; prepared statements and faster SQL execution.

## KvSQL 0.2 Phase A: prove it or park it (softdev-plan item 25, approved by YK 2026-11-03)

What and why: the screening record says the differentiation has to be proven with recorded benchmarks against the Rust neighbours, and only SQLite has been benchmarked. Phase A is a go/no-go gate: YK decides on the recorded numbers whether KvSQL continues. Phase B (single-sync commit, prepared statements) waits for that decision. Windows engine tests run only in the release workflow today.

Design decisions:
- A separate benchmark crate, bench/neighbours, outside the workspace with its own Cargo.lock, so the engine stays dependency-free and CI and releases don't build the neighbours. It depends on the engine by path and on redb 4.4.0, fjall 3.1.12 and GlueSQL 0.20.0 with its redb storage (each release at least two weeks old), and rusqlite as the existing reference.
- Same machine, same data and real files for all: 100,000 keys (the existing bench's key and value shapes), loaded in one transaction, then timed warm in a shuffled order, median of five rounds. Measured where each store has the operation: key get; range scan of 100 keys; synced single-key commit (every store set to sync on commit: redb Immediate durability, fjall persist with SyncData, the fdatasync KvSQL and SQLite use, GlueSQL autocommit, KvSQL and SQLite as in the existing bench); point SQL select by primary key (KvSQL, GlueSQL, SQLite prepared); bulk load time and file size. Each store's cache set to 8 MiB where it has a setting, otherwise its default, stated in the output.
- The output, bench/neighbours/README.md for method and test/results/latest/neighbours.md for numbers, says what each figure measures and where the comparison isn't like for like (fjall is an LSM tree without SQL; GlueSQL is SQL over its own storage; redb is a B-tree without SQL). No verdict in the recording; the report to YK states the numbers and what they mean for the differentiation claim.
- record-results.sh runs it; a CI job builds and runs it at a small size so it can't rot, on Linux.
- Upkeep: a Windows CI job running the engine tests on every push.
- No release in Phase A (no user-facing change); the site gets the numbers only after YK's decision.

Steps:
1. [x] Phase 0: this plan.
2. [x] bench/neighbours crate and its README (GlueSQL's SQL reads use 10,000 of the shuffled keys, as each statement costs about 0.25 ms whatever the table size; sizes show file length and allocated bytes, since fjall preallocates a sparse 64 MiB journal).
3. [x] record-results.sh and the CI jobs (neighbours at small size, Windows engine tests).
4. [x] Independent review of the method, two rounds. Round 1 blocker: every GlueSQL autocommit SELECT opened and committed a redb write transaction, so it synced to disk and the SQL row measured fsyncs (now its selects run in one transaction per round; autocommit kept as a labelled row over 500 keys). Should-fix, all fixed: fjall never left its 64 MiB memtable (now flushed and compacted after the load, its best case for reads), fixed store order on a noisy host (rounds interleaved, spread shown), checks on presence only (every value and scan key checked), sizes not comparable (said plainly). Nits fixed: Linux-only recording and the file system printed, fjall Snapshot::get, a drop guard, no ratio lines. Round 2: no blockers or should-fix; nits fixed (equal non-allocating checks, a KvSQL ReadTx::query row, commit count, fjall best-case wording).
5. [x] Recorded run (test/results/latest/neighbours.md, Linux x86_64, 2 vCPUs, ext4, all checks passed). Reported to YK for the go/no-go.

## KvSQL 0.2 Phase B: one sync per commit, prepared statements (YK said go on the Phase A numbers, 2026-11-03)

What and why: Phase A showed KvSQL's synced commit at about 340 µs against SQLite WAL's 180 µs and fjall's 130 µs, because a commit syncs twice (pages, then meta). SQL point selects are 1.2 to 1.4x slower than SQLite's prepared statements; a callgrind profile puts most of the gap in per-call allocation (column scope, names, cloned expressions, table lookup), not in parsing, which is already cached.

Design decisions:
- Single-sync commit, file format version 2. A commit writes its pages, a commit record (one or more record pages listing every page id the commit wrote with that page's CRC-32C) and the meta page, then syncs once. The meta page gains the record's first page and entry count. On open, the newest meta page is accepted only if every page in its record reads back with a valid seal and the recorded CRC; otherwise open falls back to the other meta page (whose commit was fully synced before this one began, because a commit only returns after its sync). Record pages come from the same pool as other new pages and are freed by the next commit, like the old free-list pages. Version 1 files open as before (their commits were synced twice) and become version 2 at their first commit; 0.1 cannot open a file after that, which the README and CHANGELOG say.
- Prepared statements: `Db::prepare(sql)` returns a `Statement` tied to that handle (parameter count checked up front). `Db::execute_prepared`, `ReadTx::query_prepared` and `WriteTx::execute_prepared` run it without the parse-cache lookup. For SELECT, a statement keeps its compiled plan (tables, column scope, bound expressions with parameter slots, access path) for the schema generation it was compiled at and reuses it while the schema stays the same; parameters are put into a copy of the plan's expressions at each run, with the same affinity rules as binding them directly. Writes, and reads inside a write transaction, use the parsed form only. Plain `execute` and `query` compile and run the same way with the values bound as literals, so their behaviour is unchanged.
- Tests: power-cut and SIGKILL suites unchanged and passing; new tests for a commit whose data pages never reached the disk but whose meta page did (open falls back), a torn record page, a version 1 file opened, written and reopened, Db::verify accounting for record pages; prepared statements give the same answers, errors and change counts as `execute` (the SQLite differential test runs each statement both ways), plans are recompiled after a schema change, a statement from another handle is refused.
- Benchmarks: bench.md and neighbours.md gain prepared-statement rows.
- Version 0.2.0. Site: a release post, the reference page, the benchmark numbers from Phase A and 0.2.0. Studio note.

Steps:
1. [x] Phase 0: this plan.
2. [x] Single-sync commit, format version 2, docs/file-format.md, tests (tests/commit_record.rs with a version 1 fixture written by the 0.1.0 shell; making open accept any record fails the power-cut tests at once). One fdatasync and three writes per small commit, checked with strace.
3. [x] Prepared statements with cached SELECT plans, tests (sql.rs; sqlite_diff runs every query again prepared with its literals as parameters, twice; filling slots with NULL, or dropping the affinity on a filled slot, fails it at once). Point select 1.73 to 1.06 µs in a local loop.
4. [x] Benchmarks (prepared rows in bench.md and neighbours.md), README (upgrade note, prepared statements, commit promise, tests), docs/sql.md.
5. [ ] Independent review (at most two rounds), fixes, fresh test/results.
   Review round 1: blocker, open trusted a commit that was never synced: a process killed inside commit T+1 left its writes in the OS page cache, reopen accepted T+1, the next commit reused T's pages, and a power cut then lost T, a commit that had returned (0.1 had the same hole for the meta page). Fixed: open syncs the file once it has chosen the meta page; new test kill_then_reopen_then_power_cut_keeps_returned_commits in power_cut.rs (SimStorage::revive keeps unsynced writes readable after a kill) failed before the fix ("no valid meta page") and passes after. Should-fix: after one 0.2 commit on a 0.1 file, 0.1 still opened the other slot's version 1 meta page and silently lost that commit; now the first version 2 commit clears that slot and syncs again (test, and the 0.1.0 shell now refuses an upgraded file). Nits fixed: docs on the open sync and the open-time read cost, a comment on the one-plan cache. Left: verify does not re-check the current record against the disk.
6. [ ] 0.2.0 released by the workflow; assets checked at the tag and latest; downloaded binary run.
7. [ ] kvsql.com post and pages; softdevstudio.com note; softdev-plan item 25 done.

## Next

Phase B of KvSQL 0.2 is in progress (above).
