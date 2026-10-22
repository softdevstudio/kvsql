# KvSQL

KvSQL puts key-value and SQL on the same records. Get a row by its key and it comes straight from storage, with no query parsing. Ask SQL about the same rows and it reads the same bytes. One transaction covers both.

There are two products:

- **KvSQL Engine**, a Rust library you embed in your program. One file on disk, no server.
- **KvSQL DB**, a database server and command-line shell built on the engine.

Status: early development. The storage engine and key-value API work and are tested. SQL and the server come next. Nothing is released yet.

## Why one store for both

Programs that need fast lookups by key and real queries often run two stores: a key-value store for the lookups and SQLite or similar for the queries. Then the data lives twice, code keeps the copies in step, and a change cannot be atomic across both.

KvSQL keeps one copy. Think of a cloakroom with two windows. At the ticket window you hand over a ticket and get your coat. At the question window you can ask for every blue coat left after 9pm. Both windows serve the same coats from the same racks, and nobody copies coats between them.

## Using the engine

Add it as a Git dependency:

```toml
[dependencies]
kvsql-engine = { git = "https://github.com/softdevstudio/kvsql" }
```

```rust
use kvsql_engine::Db;

let db = Db::open("app.kvsql")?;

// Several changes, committed together.
let mut tx = db.begin_write()?;
tx.put("users", b"42", b"Ada")?;
tx.put("users", b"43", b"Grace")?;
tx.delete("sessions", b"old")?;
tx.commit()?;

// Reads.
let name = db.get("users", b"42")?;            // Some(b"Ada")
let page = db.scan("users", b"40", Some(b"50"), 100)?;  // keys 40 up to 50
```

Data lives in keyspaces: named, ordered maps from byte keys to byte values. `put` creates a keyspace the first time it is used. Keys are 1 to 512 bytes. Values can be up to 1 GiB.

A write transaction sees its own changes. Dropping it without `commit` discards everything. One write transaction runs at a time.

`db.verify()` reads every page in the file and checks every rule the format depends on. It is the check the crash tests run after each simulated failure.

## What a commit promises

When `commit` returns, the transaction is on disk. If the program crashes or the power goes during a commit, reopening the file gives you either the whole transaction or none of it. Never part of one.

How that holds: changed pages are written to unused places in the file, the file is synced, and only then is a small meta page written that points at the new data. Until that meta page lands, the old one still points at the old, untouched data. Details are in [docs/file-format.md](docs/file-format.md).

If a commit fails part way (a disk error, say), the handle refuses further use. Reopen the file to continue from the last good commit.

Only one process can open a file at a time. A second open gets `Error::Locked`.

## How it is tested

- A model check runs thousands of random transactions (puts, deletes, large values, dropped keyspaces, rolled-back transactions) against an in-memory copy and verifies the whole file after every commit.
- A simulated disk cuts the power at a random write or sync inside commits. It then keeps, drops, reorders or tears the writes that were not yet synced, the way a real disk can. The reopened file must verify and hold exactly the last finished commit or the one in flight.
- A real writer process is killed with SIGKILL at random moments. Every commit it reported as done must be in the file.

Run them all and record the output:

```sh
bash scripts/record-results.sh
```

The output goes to `test/results/latest/checks.log`.

## License

Apache 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
