//! kvsqld: the HTTP/JSON front of a KvSQL database.
//!
//! One thread per connection, capped. Every request runs in its own
//! transaction: reads on a snapshot, writes in one write transaction that
//! commits before the response is sent. A 2xx answer to a write therefore
//! means the write is on disk.

use std::io::{BufReader, BufWriter};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use kvsql_engine::sql::{QueryResult, Value};
use kvsql_engine::{Db, Error};

use crate::http::{read_request, write_response, ReadError, Request, Response};
use crate::json::{self, Json};

#[derive(Clone, Debug)]
pub struct Config {
    /// When set, every request needs `Authorization: Bearer <token>`.
    pub token: Option<String>,
    /// Connections served at once; more are answered 503 and closed.
    pub max_connections: usize,
    /// Largest request body accepted.
    pub max_body: usize,
    /// Idle time before a keep-alive connection is closed.
    pub idle_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            token: None,
            max_connections: 256,
            max_body: 64 << 20,
            idle_timeout: Duration::from_secs(60),
        }
    }
}

/// Accept connections forever.
pub fn serve(listener: TcpListener, db: Arc<Db>, cfg: Config) {
    let active = Arc::new(AtomicUsize::new(0));
    let cfg = Arc::new(cfg);
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if active.fetch_add(1, Ordering::SeqCst) >= cfg.max_connections {
            active.fetch_sub(1, Ordering::SeqCst);
            let mut w = BufWriter::new(&stream);
            let _ = write_response(&mut w, &error(503, "busy", "too many connections"), false);
            continue;
        }
        let (db, cfg, active) = (db.clone(), cfg.clone(), active.clone());
        thread::spawn(move || {
            connection(stream, &db, &cfg);
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// Start `serve` on its own thread.
pub fn spawn(listener: TcpListener, db: Arc<Db>, cfg: Config) -> JoinHandle<()> {
    thread::spawn(move || serve(listener, db, cfg))
}

fn connection(stream: TcpStream, db: &Db, cfg: &Config) {
    let _ = stream.set_read_timeout(Some(cfg.idle_timeout));
    let _ = stream.set_nodelay(true);
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut r = BufReader::new(stream);
    let mut w = BufWriter::new(write_half);
    loop {
        let req = match read_request(&mut r, cfg.max_body) {
            Ok(req) => req,
            Err(ReadError::Bad(status, msg)) => {
                let _ = write_response(&mut w, &error(status, "request", &msg), false);
                return;
            }
            Err(ReadError::Closed) | Err(ReadError::Io(_)) => return,
        };
        let resp = if authorized(&req, cfg) {
            handle(&req, db)
        } else {
            error(401, "auth", "missing or wrong bearer token")
        };
        if write_response(&mut w, &resp, req.keep_alive).is_err() || !req.keep_alive {
            return;
        }
    }
}

fn authorized(req: &Request, cfg: &Config) -> bool {
    let Some(token) = &cfg.token else {
        return true;
    };
    let got = req
        .header("authorization")
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    // Compare in constant time.
    got.len() == token.len()
        && got
            .bytes()
            .zip(token.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn error(status: u16, kind: &str, msg: &str) -> Response {
    Response::json(
        status,
        &Json::obj(vec![
            ("error", Json::Str(msg.to_string())),
            ("kind", Json::Str(kind.to_string())),
        ]),
    )
}

fn engine_error(e: Error) -> Response {
    let (status, kind) = match &e {
        Error::Sql(_) => (400, "sql"),
        Error::Constraint(_) => (409, "constraint"),
        Error::KeyTooLarge(_) | Error::ValueTooLarge(_) | Error::BadKeyspaceName => (400, "input"),
        Error::Locked => (503, "locked"),
        Error::Io(_) => (500, "io"),
        Error::Corrupt(_) => (500, "corrupt"),
        Error::Poisoned => (500, "poisoned"),
    };
    error(status, kind, &e.to_string())
}

fn text(seg: &[u8], what: &str) -> Result<String, Response> {
    String::from_utf8(seg.to_vec())
        .map_err(|_| error(400, "input", &format!("{what} is not UTF-8")))
}

fn ok_json(j: Json) -> Response {
    Response::json(200, &j)
}

fn result_json(r: &QueryResult) -> Json {
    Json::obj(vec![
        (
            "columns",
            Json::Arr(r.columns.iter().map(|c| Json::Str(c.clone())).collect()),
        ),
        (
            "rows",
            Json::Arr(
                r.rows
                    .iter()
                    .map(|row| Json::Arr(row.iter().map(json::from_value).collect()))
                    .collect(),
            ),
        ),
        ("changes", Json::Int(r.changes as i64)),
    ])
}

fn handle(req: &Request, db: &Db) -> Response {
    match route(req, db) {
        Ok(r) => r,
        Err(r) => r,
    }
}

fn route(req: &Request, db: &Db) -> Result<Response, Response> {
    let seg: Vec<&[u8]> = req.segments.iter().map(|s| s.as_slice()).collect();
    let m = req.method.as_str();
    match (m, seg.as_slice()) {
        ("GET", [b"health"]) => Ok(ok_json(Json::obj(vec![
            ("status", Json::Str("ok".into())),
            ("version", Json::Str(kvsql_engine::VERSION.into())),
            ("txid", Json::Int(db.txid() as i64)),
        ]))),
        ("GET", [b"verify"]) => {
            let r = db.verify().map_err(engine_error)?;
            Ok(ok_json(Json::obj(vec![
                ("ok", Json::Bool(true)),
                ("txid", Json::Int(r.txid as i64)),
                ("pages", Json::Int(r.page_count as i64)),
                ("free_pages", Json::Int(r.free_pages as i64)),
                ("keyspaces", Json::Int(r.keyspaces as i64)),
                ("keys", Json::Int(r.keys as i64)),
            ])))
        }
        ("GET", [b"kv"]) => {
            let names = db.keyspaces().map_err(engine_error)?;
            Ok(ok_json(Json::obj(vec![(
                "keyspaces",
                Json::Arr(names.into_iter().map(Json::Str).collect()),
            )])))
        }
        ("GET", [b"kv", ks]) => {
            let ks = text(ks, "keyspace")?;
            let from = req.param("from").unwrap_or(b"").to_vec();
            let to = req.param("to").map(|t| t.to_vec());
            let limit = match req.param("limit") {
                Some(l) => std::str::from_utf8(l)
                    .ok()
                    .and_then(|l| l.parse().ok())
                    .ok_or_else(|| error(400, "input", "limit must be a number"))?,
                None => 1000,
            };
            let items = db
                .scan(&ks, &from, to.as_deref(), limit)
                .map_err(engine_error)?;
            Ok(ok_json(Json::obj(vec![(
                "items",
                Json::Arr(
                    items
                        .iter()
                        .map(|(k, v)| {
                            Json::obj(vec![
                                ("key", json::from_bytes(k)),
                                ("value", json::from_bytes(v)),
                            ])
                        })
                        .collect(),
                ),
            )])))
        }
        ("GET", [b"kv", ks, key]) => {
            let ks = text(ks, "keyspace")?;
            match db.get(&ks, key).map_err(engine_error)? {
                Some(v) => Ok(Response::bytes(200, v)),
                None => Err(error(404, "not_found", "no such key")),
            }
        }
        ("PUT", [b"kv", ks, key]) => {
            let ks = text(ks, "keyspace")?;
            db.put(&ks, key, &req.body).map_err(engine_error)?;
            Ok(ok_json(Json::obj(vec![("ok", Json::Bool(true))])))
        }
        ("DELETE", [b"kv", ks, key]) => {
            let ks = text(ks, "keyspace")?;
            let found = db.delete(&ks, key).map_err(engine_error)?;
            Ok(ok_json(Json::obj(vec![("deleted", Json::Bool(found))])))
        }
        ("GET", [b"rows", table, pk]) => {
            let table = text(table, "table")?;
            let pk = Value::Text(text(pk, "primary key")?);
            match db.get_row(&table, &pk).map_err(engine_error)? {
                Some(row) => Ok(ok_json(Json::obj(vec![(
                    "row",
                    Json::Arr(row.iter().map(json::from_value).collect()),
                )]))),
                None => Err(error(404, "not_found", "no such row")),
            }
        }
        ("PUT", [b"rows", table]) => {
            let table = text(table, "table")?;
            let body = parse_body(req)?;
            let Json::Arr(items) = body.get("row").cloned().unwrap_or(body) else {
                return Err(error(400, "input", "send the row as a JSON array"));
            };
            let row = items
                .iter()
                .map(json::to_value)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| error(400, "input", &e))?;
            let mut tx = db.begin_write().map_err(engine_error)?;
            tx.put_row(&table, &row).map_err(engine_error)?;
            tx.commit().map_err(engine_error)?;
            Ok(ok_json(Json::obj(vec![("ok", Json::Bool(true))])))
        }
        ("DELETE", [b"rows", table, pk]) => {
            let table = text(table, "table")?;
            let pk = Value::Text(text(pk, "primary key")?);
            let mut tx = db.begin_write().map_err(engine_error)?;
            let found = tx.delete_row(&table, &pk).map_err(engine_error)?;
            tx.commit().map_err(engine_error)?;
            Ok(ok_json(Json::obj(vec![("deleted", Json::Bool(found))])))
        }
        ("POST", [b"sql"]) => {
            let (sql, params) = sql_body(req)?;
            let r = db.execute(&sql, &params).map_err(engine_error)?;
            Ok(ok_json(result_json(&r)))
        }
        (_, [b"health"] | [b"verify"] | [b"kv", ..] | [b"rows", ..] | [b"sql"]) => {
            Err(error(405, "method", "method not allowed here"))
        }
        _ => Err(error(404, "not_found", "no such endpoint")),
    }
}

fn parse_body(req: &Request) -> Result<Json, Response> {
    let s = std::str::from_utf8(&req.body).map_err(|_| error(400, "input", "body is not UTF-8"))?;
    json::parse(s).map_err(|e| error(400, "input", &format!("bad JSON: {e}")))
}

/// `{"sql": "...", "params": [...]}`, or the SQL text itself as the body.
fn sql_body(req: &Request) -> Result<(String, Vec<Value>), Response> {
    let s = std::str::from_utf8(&req.body).map_err(|_| error(400, "input", "body is not UTF-8"))?;
    if !s.trim_start().starts_with('{') {
        return Ok((s.to_string(), Vec::new()));
    }
    let j = parse_body(req)?;
    let sql = j
        .get("sql")
        .and_then(Json::as_str)
        .ok_or_else(|| error(400, "input", "missing \"sql\""))?
        .to_string();
    let params = match j.get("params") {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Arr(items)) => items
            .iter()
            .map(json::to_value)
            .collect::<Result<_, _>>()
            .map_err(|e| error(400, "input", &e))?,
        Some(_) => return Err(error(400, "input", "\"params\" must be an array")),
    };
    Ok((sql, params))
}
