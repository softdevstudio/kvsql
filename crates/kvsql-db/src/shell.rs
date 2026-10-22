//! The `kvsql` shell: SQL and key-value commands against a running kvsqld
//! or straight against a database file.

use std::io::{self, BufRead, Write};

use kvsql_engine::sql::{QueryResult, Value};
use kvsql_engine::Db;

use crate::http::{percent_encode, Client};
use crate::json::{self, Json};

/// Where commands go.
pub enum Backend {
    Remote(Client),
    Local(Db),
}

type Res<T> = Result<T, String>;

fn remote_err(status: u16, body: &[u8]) -> String {
    let msg = json::parse(&String::from_utf8_lossy(body))
        .ok()
        .and_then(|j| j.get("error").and_then(Json::as_str).map(str::to_string))
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    format!("error ({status}): {msg}")
}

fn call(c: &mut Client, method: &str, path: &str, body: &[u8]) -> Res<(u16, Vec<u8>)> {
    c.request(method, path, body)
        .map_err(|e| format!("cannot reach server: {e}"))
}

fn json_ok(c: &mut Client, method: &str, path: &str, body: &[u8]) -> Res<Json> {
    let (status, b) = call(c, method, path, body)?;
    if !(200..300).contains(&status) {
        return Err(remote_err(status, &b));
    }
    json::parse(&String::from_utf8_lossy(&b)).map_err(|e| format!("bad reply: {e}"))
}

fn result_from_json(j: &Json) -> Res<QueryResult> {
    let strs = |j: Option<&Json>| match j {
        Some(Json::Arr(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    let rows = match j.get("rows") {
        Some(Json::Arr(rows)) => rows
            .iter()
            .map(|r| match r {
                Json::Arr(vals) => vals
                    .iter()
                    .map(json::to_value)
                    .collect::<Result<Vec<_>, _>>(),
                _ => Err("row is not an array".to_string()),
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => Vec::new(),
    };
    let changes = match j.get("changes") {
        Some(Json::Int(n)) => *n as u64,
        _ => 0,
    };
    Ok(QueryResult {
        columns: strs(j.get("columns")),
        rows,
        changes,
    })
}

impl Backend {
    pub fn sql(&mut self, sql: &str, params: &[Value]) -> Res<QueryResult> {
        match self {
            Backend::Local(db) => db.execute(sql, params).map_err(|e| e.to_string()),
            Backend::Remote(c) => {
                let body = Json::obj(vec![
                    ("sql", Json::Str(sql.to_string())),
                    (
                        "params",
                        Json::Arr(params.iter().map(json::from_value).collect()),
                    ),
                ]);
                let j = json_ok(c, "POST", "/sql", body.to_string_compact().as_bytes())?;
                result_from_json(&j)
            }
        }
    }

    pub fn get(&mut self, ks: &str, key: &[u8]) -> Res<Option<Vec<u8>>> {
        match self {
            Backend::Local(db) => db.get(ks, key).map_err(|e| e.to_string()),
            Backend::Remote(c) => {
                let path = format!(
                    "/kv/{}/{}",
                    percent_encode(ks.as_bytes()),
                    percent_encode(key)
                );
                let (status, b) = call(c, "GET", &path, b"")?;
                match status {
                    200 => Ok(Some(b)),
                    404 => Ok(None),
                    s => Err(remote_err(s, &b)),
                }
            }
        }
    }

    pub fn put(&mut self, ks: &str, key: &[u8], value: &[u8]) -> Res<()> {
        match self {
            Backend::Local(db) => db.put(ks, key, value).map_err(|e| e.to_string()),
            Backend::Remote(c) => {
                let path = format!(
                    "/kv/{}/{}",
                    percent_encode(ks.as_bytes()),
                    percent_encode(key)
                );
                json_ok(c, "PUT", &path, value).map(|_| ())
            }
        }
    }

    pub fn delete(&mut self, ks: &str, key: &[u8]) -> Res<bool> {
        match self {
            Backend::Local(db) => db.delete(ks, key).map_err(|e| e.to_string()),
            Backend::Remote(c) => {
                let path = format!(
                    "/kv/{}/{}",
                    percent_encode(ks.as_bytes()),
                    percent_encode(key)
                );
                let j = json_ok(c, "DELETE", &path, b"")?;
                Ok(j.get("deleted") == Some(&Json::Bool(true)))
            }
        }
    }

    pub fn scan(&mut self, ks: &str, from: &[u8], limit: usize) -> Res<Vec<(Vec<u8>, Vec<u8>)>> {
        match self {
            Backend::Local(db) => db.scan(ks, from, None, limit).map_err(|e| e.to_string()),
            Backend::Remote(c) => {
                let path = format!(
                    "/kv/{}?from={}&limit={limit}",
                    percent_encode(ks.as_bytes()),
                    percent_encode(from)
                );
                let j = json_ok(c, "GET", &path, b"")?;
                let bytes = |x: Option<&Json>| match x {
                    Some(Json::Str(s)) => Ok(s.clone().into_bytes()),
                    Some(o) => match json::to_value(o) {
                        Ok(Value::Blob(b)) => Ok(b),
                        _ => Err("bad item".to_string()),
                    },
                    None => Err("bad item".to_string()),
                };
                match j.get("items") {
                    Some(Json::Arr(items)) => items
                        .iter()
                        .map(|it| Ok((bytes(it.get("key"))?, bytes(it.get("value"))?)))
                        .collect(),
                    _ => Err("bad reply".into()),
                }
            }
        }
    }

    pub fn keyspaces(&mut self) -> Res<Vec<String>> {
        match self {
            Backend::Local(db) => db.keyspaces().map_err(|e| e.to_string()),
            Backend::Remote(c) => {
                let j = json_ok(c, "GET", "/kv", b"")?;
                match j.get("keyspaces") {
                    Some(Json::Arr(a)) => Ok(a
                        .iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()),
                    _ => Err("bad reply".into()),
                }
            }
        }
    }

    pub fn row(&mut self, table: &str, pk: &str) -> Res<Option<Vec<Value>>> {
        match self {
            Backend::Local(db) => db
                .get_row(table, &Value::Text(pk.to_string()))
                .map_err(|e| e.to_string()),
            Backend::Remote(c) => {
                let path = format!(
                    "/rows/{}/{}",
                    percent_encode(table.as_bytes()),
                    percent_encode(pk.as_bytes())
                );
                let (status, b) = call(c, "GET", &path, b"")?;
                match status {
                    200 => {
                        let j =
                            json::parse(&String::from_utf8_lossy(&b)).map_err(|e| e.to_string())?;
                        match j.get("row") {
                            Some(Json::Arr(vals)) => Ok(Some(
                                vals.iter().map(json::to_value).collect::<Result<_, _>>()?,
                            )),
                            _ => Err("bad reply".into()),
                        }
                    }
                    404 => Ok(None),
                    s => Err(remote_err(s, &b)),
                }
            }
        }
    }

    pub fn verify(&mut self) -> Res<String> {
        match self {
            Backend::Local(db) => db
                .verify()
                .map(|r| {
                    format!(
                        "ok: commit {}, {} pages ({} free), {} keyspaces, {} keys",
                        r.txid, r.page_count, r.free_pages, r.keyspaces, r.keys
                    )
                })
                .map_err(|e| e.to_string()),
            Backend::Remote(c) => {
                let j = json_ok(c, "GET", "/verify", b"")?;
                Ok(format!("ok: {}", j.to_string_compact()))
            }
        }
    }
}

fn show_bytes(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => Value::Blob(b.to_vec()).to_string(),
    }
}

/// Render a result as an aligned text table.
pub fn render(r: &QueryResult) -> String {
    if r.columns.is_empty() {
        return format!(
            "{} row{} changed",
            r.changes,
            if r.changes == 1 { "" } else { "s" }
        );
    }
    let cells: Vec<Vec<String>> = r
        .rows
        .iter()
        .map(|row| row.iter().map(|v| v.to_string()).collect())
        .collect();
    let mut w: Vec<usize> = r.columns.iter().map(|c| c.chars().count()).collect();
    for row in &cells {
        for (i, c) in row.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let line = |vals: &[String]| {
        vals.iter()
            .enumerate()
            .map(|(i, v)| format!("{v:<width$}", width = w[i]))
            .collect::<Vec<_>>()
            .join(" | ")
            .trim_end()
            .to_string()
    };
    let mut out = line(&r.columns);
    out.push('\n');
    out.push_str(
        &w.iter()
            .map(|n| "-".repeat(*n))
            .collect::<Vec<_>>()
            .join("-+-"),
    );
    for row in &cells {
        out.push('\n');
        out.push_str(&line(row));
    }
    out.push_str(&format!(
        "\n({} row{})",
        cells.len(),
        if cells.len() == 1 { "" } else { "s" }
    ));
    out
}

pub const HELP: &str = "\
SQL statements end with ';'. Commands:
  .keyspaces               list keyspaces
  .get KS KEY              read a key
  .put KS KEY VALUE...     write a key (the rest of the line is the value)
  .del KS KEY              delete a key
  .scan KS [FROM] [LIMIT]  list keys in order (default limit 20)
  .row TABLE PK            read a table row by primary key, no SQL
  .verify                  check every page, row and index
  .help                    this text
  .quit                    leave";

/// Run one command line (SQL or a dot command). Returns the text to print,
/// or `None` for `.quit`.
pub fn run_line(b: &mut Backend, line: &str) -> Option<Res<String>> {
    let t = line.trim();
    if !t.starts_with('.') {
        return Some(b.sql(t, &[]).map(|r| render(&r)));
    }
    let mut words = t.splitn(4, char::is_whitespace).filter(|w| !w.is_empty());
    let cmd = words.next().unwrap_or("");
    let args: Vec<&str> = words.collect();
    let need = |n: usize, usage: &str| -> Res<()> {
        if args.len() < n {
            Err(format!("usage: {usage}"))
        } else {
            Ok(())
        }
    };
    let out = match cmd {
        ".quit" | ".exit" => return None,
        ".help" => Ok(HELP.to_string()),
        ".keyspaces" => b.keyspaces().map(|k| k.join("\n")),
        ".get" => need(2, ".get KS KEY").and_then(|_| {
            b.get(args[0], args[1].as_bytes())
                .map(|v| v.map_or("(not found)".into(), |v| show_bytes(&v)))
        }),
        ".put" => need(3, ".put KS KEY VALUE").and_then(|_| {
            b.put(args[0], args[1].as_bytes(), args[2].as_bytes())
                .map(|_| "ok".into())
        }),
        ".del" => need(2, ".del KS KEY").and_then(|_| {
            b.delete(args[0], args[1].as_bytes())
                .map(|f| if f { "deleted" } else { "(not found)" }.into())
        }),
        ".scan" => need(1, ".scan KS [FROM] [LIMIT]").and_then(|_| {
            let from = args.get(1).copied().unwrap_or("");
            let limit = args.get(2).and_then(|l| l.parse().ok()).unwrap_or(20);
            b.scan(args[0], from.as_bytes(), limit).map(|items| {
                items
                    .iter()
                    .map(|(k, v)| format!("{} = {}", show_bytes(k), show_bytes(v)))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }),
        ".row" => need(2, ".row TABLE PK").and_then(|_| {
            b.row(args[0], args[1]).map(|r| match r {
                Some(vals) => vals
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(" | "),
                None => "(not found)".into(),
            })
        }),
        ".verify" => b.verify(),
        other => Err(format!("unknown command {other}; try .help")),
    };
    Some(out)
}

/// Read commands from `input` until end or `.quit`. SQL may span lines and
/// runs when a line ends with ';'. Returns the number of commands that failed.
pub fn repl(
    b: &mut Backend,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    prompt: bool,
) -> io::Result<u64> {
    let mut pending = String::new();
    let mut failures = 0;
    loop {
        if prompt {
            write!(
                out,
                "{}",
                if pending.is_empty() {
                    "kvsql> "
                } else {
                    "   ...> "
                }
            )?;
            out.flush()?;
        }
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        let t = line.trim();
        if pending.is_empty() && (t.is_empty() || t.starts_with("--")) {
            continue;
        }
        let command = if pending.is_empty() && t.starts_with('.') {
            t.to_string()
        } else {
            pending.push_str(&line);
            if !t.ends_with(';') {
                continue;
            }
            std::mem::take(&mut pending)
        };
        match run_line(b, &command) {
            None => break,
            Some(Ok(s)) => {
                if !s.is_empty() {
                    writeln!(out, "{s}")?;
                }
            }
            Some(Err(e)) => {
                failures += 1;
                writeln!(out, "{e}")?;
            }
        }
    }
    if !pending.trim().is_empty() {
        match run_line(b, &pending) {
            Some(Ok(s)) => writeln!(out, "{s}")?,
            Some(Err(e)) => {
                failures += 1;
                writeln!(out, "{e}")?;
            }
            None => {}
        }
    }
    Ok(failures)
}
