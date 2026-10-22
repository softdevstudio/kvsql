# KvSQL

KvSQL puts key-value and SQL on the same records. Get a row by its key and it comes straight from storage, with no query parsing. Ask SQL about the same rows and it reads the same bytes. One transaction covers both.

There are two products:

- **KvSQL Engine**, a Rust library you embed in your program. One file on disk, no server.
- **KvSQL DB**, a database server and command-line shell built on the engine.

Status: early development. The storage engine, key-value API, snapshot readers, SQL, the server and the shell work and are tested. Nothing is released yet.

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

A write transaction sees its own changes. Dropping it without `commit` discards everything. One write transaction runs at a time; a second `begin_write` waits for the first to finish.

## Readers run beside the writer

Reads never wait for the writer. Each read works on a snapshot: the database as of the last commit before the read began.

```rust
let mut snap = db.begin_read()?;
let a = snap.get("accounts", b"alice")?;
// ... the writer commits a transfer here ...
let b = snap.get("accounts", b"bob")?;   // still the same snapshot
```

Both reads come from the same commit, so totals add up even while money moves. `db.get` and `db.scan` open a short snapshot of their own.

Committed data is never changed in place, which is what makes this work. Pages a commit stops using are reused only after every reader that could still see them has finished. A reader kept open for a long time therefore makes the file grow; close readers when you are done with them.

`db.verify()` reads every page in the file and checks every rule the format depends on. It is the check the crash tests run after each simulated failure.

## SQL on the same rows

```rust
use kvsql_engine::sql::Value;

db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, city TEXT);
            CREATE INDEX users_city ON users(city)", &[])?;
db.execute("INSERT INTO users VALUES (1, 'Ada', 'London'), (2, 'Grace', 'New York')", &[])?;

// The SQL door.
let r = db.execute("SELECT name FROM users WHERE city = ?", &[Value::Text("London".into())])?;

// The key door: same row, no parsing, no query plan.
let row = db.get_row("users", &Value::Int(1))?;

// Writes through the key door keep the index right too.
let mut tx = db.begin_write()?;
tx.put_row("users", &[Value::Int(3), Value::Text("Alan".into()), Value::Text("London".into())])?;
tx.commit()?;
```

A table is a keyspace of the same name, and each row sits under its primary key. That is why both doors see the same bytes. A plain keyspace with no table definition reads in SQL as a two-column table, `key` and `value`.

The SQL is a subset close to SQLite with STRICT tables: CREATE/DROP TABLE and INDEX, INSERT, UPDATE, DELETE, and SELECT with joins, WHERE, GROUP BY, HAVING, ORDER BY, LIMIT, aggregates and the common functions. The full list, and what is missing, is in [docs/sql.md](docs/sql.md).

## KvSQL DB: the server and the shell

`kvsqld` serves one database file over HTTP with JSON, so programs in any language can use it. `kvsql` is a shell for people.

```sh
kvsqld --db app.db                      # listens on 127.0.0.1:7878
kvsql -c "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)"
kvsql -c "INSERT INTO users VALUES (1, 'Ada')" -c ".row users 1"
kvsql < schema.sql                      # run a script
kvsql --file app.db                     # no server: open the file directly
```

The same two doors are there. `POST /sql` takes SQL; `GET /rows/users/1` and `GET /kv/sessions/s1` read straight from storage with no SQL in between.

```sh
curl -s localhost:7878/sql -d '{"sql":"SELECT name FROM users WHERE id = ?","params":[1]}'
curl -s localhost:7878/rows/users/1
curl -s -X PUT localhost:7878/kv/sessions/s1 --data-binary 'hello'
```

Each request is one transaction, and a write is answered only once its commit is on disk. A 200 means the data is safe even if the server is killed a moment later. Several SQL statements in one request succeed or fail together.

Set `--token` (or `KVSQL_TOKEN`) to require a bearer token. There is no TLS, so keep the server on localhost or a private network, or put a proxy in front. Every endpoint, option and error is listed in [docs/server.md](docs/server.md).

## What a commit promises

When `commit` returns, the transaction is on disk. If the program crashes or the power goes during a commit, reopening the file gives you either the whole transaction or none of it. Never part of one.

How that holds: changed pages are written to unused places in the file, the file is synced, and only then is a small meta page written that points at the new data. Until that meta page lands, the old one still points at the old, untouched data. Details are in [docs/file-format.md](docs/file-format.md).

If a commit fails part way (a disk error, say), the handle refuses further use. Reopen the file to continue from the last good commit.

Only one process can open a file at a time. A second open gets `Error::Locked`.

## How it is tested

- A model check runs thousands of random transactions (puts, deletes, large values, dropped keyspaces, rolled-back transactions) against an in-memory copy and verifies the whole file after every commit.
- A simulated disk cuts the power at a random write or sync inside commits. It then keeps, drops, reorders or tears the writes that were not yet synced, the way a real disk can. The reopened file must verify and hold exactly the last finished commit or the one in flight.
- Half the power-cut rounds keep a reader open across the commits; its view must not change.
- Reader threads hold snapshots for random times while a writer moves money between accounts and churns pages. Every snapshot must show the same total twice and every value intact.
- A real writer process is killed with SIGKILL at random moments. Every commit it reported as done must be in the file.
- Random SQL statements run on KvSQL and on SQLite side by side, and every answer, row count and error must match. The generator steers some WHERE clauses toward key and index lookups so the planner's shortcuts get checked too.
- Power cuts during SQL writes: after each one the file verifies, every index matches its table exactly, and the table holds the state before the statement or after it.
- Many HTTP clients write and read at once; readers must always see the same money total.
- A real `kvsqld` is killed with SIGKILL while clients write to it. Every write that got a 200 must be in the file afterwards, and the file must verify.

Run them all and record the output:

```sh
bash scripts/record-results.sh
```

The output goes to `test/results/latest/checks.log`.

## License

Apache 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
