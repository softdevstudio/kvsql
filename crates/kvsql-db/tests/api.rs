//! The HTTP API, endpoint by endpoint, against an in-process server.

use std::net::TcpListener;
use std::sync::Arc;

use kvsql_db::http::{percent_encode, Client};
use kvsql_db::json::{self, Json};
use kvsql_db::server::{self, Config};
use kvsql_engine::sim::SimStorage;
use kvsql_engine::{Db, Options};

fn start(cfg: Config) -> String {
    let db = Arc::new(Db::open_with(Box::new(SimStorage::new()), Options::default()).unwrap());
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    server::spawn(l, db, cfg);
    url
}

fn req(c: &mut Client, m: &str, path: &str, body: &str) -> (u16, Json) {
    let (s, b) = c.request(m, path, body.as_bytes()).unwrap();
    let j = json::parse(&String::from_utf8_lossy(&b))
        .unwrap_or(Json::Str(String::from_utf8_lossy(&b).into()));
    (s, j)
}

fn sql(c: &mut Client, q: &str) -> (u16, Json) {
    let body = Json::obj(vec![("sql", Json::Str(q.into()))]).to_string_compact();
    req(c, "POST", "/sql", &body)
}

#[test]
fn health_and_routing() {
    let url = start(Config::default());
    let mut c = Client::new(&url, None).unwrap();
    let (s, j) = req(&mut c, "GET", "/health", "");
    assert_eq!(s, 200);
    assert_eq!(j.get("status"), Some(&Json::Str("ok".into())));
    assert_eq!(req(&mut c, "GET", "/nope", "").0, 404);
    assert_eq!(req(&mut c, "POST", "/health", "").0, 405);
    assert_eq!(req(&mut c, "PATCH", "/kv/a/b", "").0, 405);
}

#[test]
fn kv_endpoints_with_binary_keys() {
    let url = start(Config::default());
    let mut c = Client::new(&url, None).unwrap();
    let key = b"a/b c\x00\xff";
    let path = format!("/kv/space/{}", percent_encode(key));
    let (s, _) = c.request("PUT", &path, &[0, 1, 2, 255]).unwrap();
    assert_eq!(s, 200);
    let (s, body) = c.request("GET", &path, b"").unwrap();
    assert_eq!((s, body), (200, vec![0, 1, 2, 255]));
    c.request("PUT", "/kv/space/zz", b"text value").unwrap();
    let (s, j) = req(&mut c, "GET", "/kv/space?limit=10", "");
    assert_eq!(s, 200);
    let Some(Json::Arr(items)) = j.get("items") else {
        panic!("{j:?}")
    };
    assert_eq!(items.len(), 2);
    assert_eq!(items[1].get("value"), Some(&Json::Str("text value".into())));
    assert!(items[0].get("key").unwrap().get("$blob").is_some());
    let (_, j) = req(&mut c, "GET", "/kv/space?from=z", "");
    let Some(Json::Arr(items)) = j.get("items") else {
        panic!()
    };
    assert_eq!(items.len(), 1);
    assert_eq!(
        req(&mut c, "GET", "/kv", "").1.get("keyspaces"),
        Some(&Json::Arr(vec![Json::Str("space".into())]))
    );
    let (s, j) = req(&mut c, "DELETE", &path, "");
    assert_eq!((s, j.get("deleted")), (200, Some(&Json::Bool(true))));
    assert_eq!(req(&mut c, "GET", &path, "").0, 404);
}

#[test]
fn sql_and_rows_endpoints() {
    let url = start(Config::default());
    let mut c = Client::new(&url, None).unwrap();
    let (s, j) = sql(
        &mut c,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL, data BLOB)",
    );
    assert_eq!(s, 200, "{j:?}");
    let body =
        r#"{"sql":"INSERT INTO t VALUES (?, ?, ?, ?)","params":[1,"Ada",2.0,{"$blob":"AP8="}]}"#;
    let (s, j) = req(&mut c, "POST", "/sql", body);
    assert_eq!((s, j.get("changes")), (200, Some(&Json::Int(1))));
    let (_, j) = sql(&mut c, "SELECT * FROM t");
    assert_eq!(
        j.to_string_compact(),
        r#"{"columns":["id","name","score","data"],"rows":[[1,"Ada",2.0,{"$blob":"AP8="}]],"changes":0}"#
    );
    // Plain-text body works too.
    let (s, _) = req(
        &mut c,
        "POST",
        "/sql",
        "INSERT INTO t (id, name) VALUES (2, 'Grace')",
    );
    assert_eq!(s, 200);
    // The KV door for rows.
    let (s, j) = req(&mut c, "GET", "/rows/t/2", "");
    assert_eq!(s, 200);
    assert_eq!(
        j.get("row").unwrap().to_string_compact(),
        r#"[2,"Grace",null,null]"#
    );
    assert_eq!(
        req(&mut c, "PUT", "/rows/t", r#"[3,"Alan",1.5,null]"#).0,
        200
    );
    assert_eq!(
        req(&mut c, "PUT", "/rows/t", r#"{"row":[4,"Ken",null,null]}"#).0,
        200
    );
    let (_, j) = sql(&mut c, "SELECT count(*) FROM t");
    assert_eq!(j.get("rows").unwrap().to_string_compact(), "[[4]]");
    assert_eq!(
        req(&mut c, "DELETE", "/rows/t/3", "").1.get("deleted"),
        Some(&Json::Bool(true))
    );
    assert_eq!(req(&mut c, "GET", "/rows/t/3", "").0, 404);
    // Errors carry a kind and the right status.
    let (s, j) = sql(&mut c, "SELECT nope FROM t");
    assert_eq!((s, j.get("kind")), (400, Some(&Json::Str("sql".into()))));
    let (s, j) = sql(&mut c, "INSERT INTO t VALUES (1, 'dup', NULL, NULL)");
    assert_eq!(
        (s, j.get("kind")),
        (409, Some(&Json::Str("constraint".into())))
    );
    assert_eq!(
        req(&mut c, "PUT", "/rows/t", r#"[5,"x","not a number",null]"#).0,
        409
    );
    assert_eq!(req(&mut c, "POST", "/sql", "{bad json").0, 400);
    assert_eq!(
        req(&mut c, "POST", "/sql", r#"{"sql":"SELECT ?","params":5}"#).0,
        400
    );
    // A failed multi-statement request leaves nothing behind.
    let (s, _) = sql(
        &mut c,
        "INSERT INTO t VALUES (10, 'a', NULL, NULL); INSERT INTO t VALUES (1, 'dup', NULL, NULL)",
    );
    assert_eq!(s, 409);
    assert_eq!(req(&mut c, "GET", "/rows/t/10", "").0, 404);
    let (s, j) = req(&mut c, "GET", "/verify", "");
    assert_eq!((s, j.get("ok")), (200, Some(&Json::Bool(true))));
}

#[test]
fn token_is_required_when_set() {
    let url = start(Config {
        token: Some("s3cret".into()),
        ..Config::default()
    });
    let mut anon = Client::new(&url, None).unwrap();
    assert_eq!(req(&mut anon, "GET", "/health", "").0, 401);
    let mut wrong = Client::new(&url, Some("s3creT".into())).unwrap();
    assert_eq!(req(&mut wrong, "GET", "/health", "").0, 401);
    let mut ok = Client::new(&url, Some("s3cret".into())).unwrap();
    assert_eq!(req(&mut ok, "GET", "/health", "").0, 200);
}

#[test]
fn limits_are_enforced() {
    let url = start(Config {
        max_body: 1000,
        max_connections: 2,
        ..Config::default()
    });
    let mut c = Client::new(&url, None).unwrap();
    let (s, _) = c.request("PUT", "/kv/a/b", &[0u8; 2000]).unwrap();
    assert_eq!(s, 413);
    // Two held connections fill the server; a third is turned away.
    let mut a = Client::new(&url, None).unwrap();
    let mut b = Client::new(&url, None).unwrap();
    assert_eq!(req(&mut a, "GET", "/health", "").0, 200);
    assert_eq!(req(&mut b, "GET", "/health", "").0, 200);
    let mut third = Client::new(&url, None).unwrap();
    assert_eq!(req(&mut third, "GET", "/health", "").0, 503);
    drop(a);
    // Once a slot frees, new connections are served again.
    let mut ok = false;
    for _ in 0..50 {
        let mut d = Client::new(&url, None).unwrap();
        if req(&mut d, "GET", "/health", "").0 == 200 {
            ok = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(ok);
}

fn raw(url: &str, request: &[u8]) -> String {
    use std::io::{Read, Write};
    let addr = url.trim_start_matches("http://");
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.write_all(request).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

#[test]
fn malformed_requests_get_clear_errors() {
    let url = start(Config::default());
    assert!(raw(&url, b"NONSENSE\r\n\r\n").starts_with("HTTP/1.1 400"));
    assert!(raw(
        &url,
        b"POST /sql HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"
    )
    .starts_with("HTTP/1.1 411"));
    assert!(
        raw(&url, b"POST /sql HTTP/1.1\r\nContent-Length: x\r\n\r\n").starts_with("HTTP/1.1 400")
    );
    let long = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(20_000));
    assert!(raw(&url, long.as_bytes()).starts_with("HTTP/1.1 4"));
    // HTTP/1.0 and Connection: close both end the connection after one answer.
    let r = raw(&url, b"GET /health HTTP/1.0\r\n\r\n");
    assert!(
        r.starts_with("HTTP/1.1 200") && r.contains("\"status\":\"ok\""),
        "{r}"
    );
    let r = raw(&url, b"GET /health HTTP/1.1\r\nConnection: close\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    // Two pipelined requests on one connection are both answered.
    let r = raw(
        &url,
        b"PUT /kv/p/k HTTP/1.1\r\nContent-Length: 2\r\n\r\nhiGET /kv/p/k HTTP/1.1\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(r.matches("HTTP/1.1 200").count(), 2, "{r}");
    assert!(r.ends_with("hi"), "{r}");
    // The server still works afterwards.
    let mut c = Client::new(&url, None).unwrap();
    assert_eq!(req(&mut c, "GET", "/health", "").0, 200);
}

#[test]
fn slow_requests_lose_their_slot() {
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};
    let url = start(Config {
        max_connections: 1,
        request_timeout: Duration::from_millis(400),
        ..Config::default()
    });
    let addr = url.trim_start_matches("http://").to_string();
    // A client that trickles a header one byte at a time.
    let mut slow = std::net::TcpStream::connect(&addr).unwrap();
    let t = Instant::now();
    let mut closed = false;
    for b in b"GET /health HTTP/1.1\r\nX-Slow: yes"
        .iter()
        .cycle()
        .take(200)
    {
        if slow.write_all(&[*b]).is_err() {
            closed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
        let mut buf = [0u8; 64];
        slow.set_nonblocking(true).unwrap();
        let r = slow.read(&mut buf);
        slow.set_nonblocking(false).unwrap();
        if matches!(r, Ok(0)) || matches!(&r, Err(e) if e.kind() != std::io::ErrorKind::WouldBlock)
        {
            closed = true;
            break;
        }
    }
    assert!(closed, "slow client kept its connection");
    assert!(t.elapsed() < Duration::from_secs(3));
    // The slot is free again.
    let mut ok = false;
    for _ in 0..50 {
        let mut c = Client::new(&url, None).unwrap();
        if req(&mut c, "GET", "/health", "").0 == 200 {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ok);
}

#[test]
fn hostile_sql_gets_an_error_not_a_crash() {
    let url = start(Config::default());
    let mut c = Client::new(&url, None).unwrap();
    for body in [
        format!("SELECT {}", vec!["1"; 50_000].join("+")),
        format!("SELECT {}1{}", "(".repeat(50_000), ")".repeat(50_000)),
    ] {
        let (s, j) = req(&mut c, "POST", "/sql", &body);
        assert_eq!(s, 400, "{j:?}");
    }
    assert_eq!(req(&mut c, "GET", "/health", "").0, 200);
}
