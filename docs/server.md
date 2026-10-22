# KvSQL DB server and shell

KvSQL DB is two programs built on the engine. `kvsqld` serves one database file over HTTP with JSON. `kvsql` is a shell that talks to `kvsqld`, or opens a database file directly when no server is running.

## kvsqld

```
kvsqld --db PATH [--listen ADDR] [--token TOKEN] [--max-connections N]
```

| Option | Meaning |
|---|---|
| `--db PATH` | Database file. Created if it does not exist. Required. |
| `--listen ADDR` | Address and port. Default `127.0.0.1:7878`. Port 0 picks a free port. |
| `--token TOKEN` | Every request must carry `Authorization: Bearer TOKEN`. The `KVSQL_TOKEN` environment variable does the same. |
| `--max-connections N` | Connections served at once. Default 256. Further connections get a 503 and are closed. |
| `--version`, `--help` | Print and exit. |

On start it prints one line to standard output, for example

```
kvsqld 0.1.0 listening on 127.0.0.1:7878, database app.db
```

Scripts can read the address from that line. With no token and an address beyond localhost it prints a warning on standard error. The server holds the file lock, so other processes cannot open the file while it runs.

There is no TLS. Put a proxy in front of the server, or keep it on localhost or a private network.

### Transactions

Each request is its own transaction. A read runs on a snapshot and never waits for the writer. A write request runs in one write transaction, and the reply is sent only after the commit is on disk. A 200 answer to a write therefore means the data is durable. If the server is killed before it answers, the write is either fully there or fully absent.

A `POST /sql` request may hold several statements separated by `;`. They run in one transaction: if any statement fails, none of them is kept.

Writes from many connections are serialised, one at a time, through the engine's single writer.

### HTTP

HTTP/1.1 with keep-alive. Bodies must use `Content-Length`; chunked bodies get 411. The largest body is 64 MiB. Idle connections close after 60 seconds. Path segments are percent-encoded, so keys can hold any bytes.

### Values in JSON

| SQL value | JSON |
|---|---|
| NULL | `null` |
| INTEGER | number without a fraction |
| REAL | number with a fraction or exponent, such as `2.0` |
| TEXT | string |
| BLOB | `{"$blob": "<base64>"}` |

Keys and values in KV listings use a string when the bytes are valid UTF-8 and the `$blob` form otherwise.

### Endpoints

| Method and path | Does |
|---|---|
| `GET /health` | `{"status":"ok","version":"0.1.0","txid":N}` |
| `GET /verify` | Runs `Db::verify` over the whole file. `{"ok":true,"txid":..,"pages":..,"free_pages":..,"keyspaces":..,"keys":..}` |
| `GET /kv` | `{"keyspaces":[...]}` |
| `GET /kv/{ks}?from=&to=&limit=` | Keys in order from `from` (inclusive) up to `to` (exclusive). Default limit 1000. `{"items":[{"key":..,"value":..}]}` |
| `GET /kv/{ks}/{key}` | The raw value bytes, `application/octet-stream`. 404 if missing. |
| `PUT /kv/{ks}/{key}` | Body is the raw value. Creates the keyspace if needed. `{"ok":true}` |
| `DELETE /kv/{ks}/{key}` | `{"deleted":true}` or `{"deleted":false}` |
| `GET /rows/{table}/{pk}` | A table row by primary key, with no SQL parsing. `{"row":[...]}`. The key in the path is text and is converted to the key column's type. |
| `PUT /rows/{table}` | Insert or replace a whole row. Body is a JSON array of column values, or `{"row":[...]}`. Indexes are kept. |
| `DELETE /rows/{table}/{pk}` | `{"deleted":true}` or `{"deleted":false}` |
| `POST /sql` | Body `{"sql":"...","params":[...]}`, or plain SQL text. Answers `{"columns":[...],"rows":[[...]],"changes":N}`; for several statements, the result of the last. |

A `PUT` to `/kv/{table}/{key}` on a table keyspace must hold a valid encoded row, the same rule as the engine's `put`.

### Errors

Errors come back as `{"error":"message","kind":"..."}`.

| Status | kind | Cause |
|---|---|---|
| 400 | `sql` | SQL that does not parse or names something missing |
| 400 | `input` | Bad JSON, bad parameters, key or value too large, bad keyspace name |
| 400 | `request` | Malformed HTTP |
| 401 | `auth` | Missing or wrong bearer token |
| 404 | `not_found` | No such key, row or endpoint |
| 405 | `method` | Known path, wrong method |
| 409 | `constraint` | Primary key, NOT NULL or type rule broken |
| 411, 413, 431 | `request` | Chunked body, body too large, too many headers |
| 500 | `io`, `corrupt`, `poisoned` | Disk error, damaged file, or a failed commit. After `poisoned`, restart the server. |
| 503 | `busy` | Too many connections |

### Examples

```sh
curl -s localhost:7878/sql -d "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)"
curl -s localhost:7878/sql -d '{"sql":"INSERT INTO users VALUES (?, ?)","params":[1,"Ada"]}'
curl -s localhost:7878/rows/users/1
curl -s -X PUT localhost:7878/kv/sessions/s1 --data-binary 'hello'
curl -s localhost:7878/kv/sessions/s1
```

## kvsql

```
kvsql [--url URL | --file PATH] [--token TOKEN] [-c COMMAND]...
```

| Option | Meaning |
|---|---|
| `--url URL` | Server to use. Default `http://127.0.0.1:7878`, or `KVSQL_URL`. |
| `--file PATH` | Open a database file directly. No server needed. |
| `--token TOKEN` | Bearer token, or `KVSQL_TOKEN`. |
| `-c COMMAND` | Run one command. Repeat for more. The shell exits after the last. |

Without `-c` the shell reads commands from standard input. On a terminal it shows a prompt; from a pipe or a file it runs the script quietly. SQL may span several lines and runs when a line ends with `;`. Lines starting with `--` are comments. The exit code is 1 when any command failed and 2 for bad options.

Dot commands use the key-value side:

| Command | Does |
|---|---|
| `.keyspaces` | List keyspaces |
| `.get KS KEY` | Print a value |
| `.put KS KEY VALUE` | Store a value; the rest of the line is the value |
| `.del KS KEY` | Delete a key |
| `.scan KS [FROM] [LIMIT]` | List keys in order, 20 by default |
| `.row TABLE PK` | Read a table row by primary key, without SQL |
| `.verify` | Check every page, row and index |
| `.help`, `.quit` | Help, leave |

## How it is tested

- Every endpoint, status code and error kind, binary keys, bearer tokens, connection and body limits, malformed and pipelined requests (`crates/kvsql-db/tests/api.rs`).
- Six writer connections move money between accounts and append log rows while four reader connections check that the total never changes and the KV door sees the same rows. Afterwards every writer's rows are counted and the file verified (`tests/server_load.rs`).
- A real `kvsqld` process is put under load from four clients doing SQL transfers, row inserts and raw puts, then killed with SIGKILL at a random moment. The file is opened directly: it must verify, the money total must hold, and every write that got a 200 must be there. This repeats with the same file many times.
- The shell in file mode, server mode, `-c` and script input, exit codes and errors (`tests/shell.rs`).
