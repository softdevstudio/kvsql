//! Just enough HTTP/1.1 for kvsqld and its clients: Content-Length bodies,
//! keep-alive, percent-encoded paths and query strings.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct Request {
    pub method: String,
    /// Path segments, percent-decoded to raw bytes.
    pub segments: Vec<Vec<u8>>,
    pub query: Vec<(String, Vec<u8>)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub keep_alive: bool,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    pub fn param(&self, name: &str) -> Option<&[u8]> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_slice())
    }
}

pub enum ReadError {
    /// The peer closed the connection between requests.
    Closed,
    Io(io::Error),
    /// A malformed request; answer with this status and close.
    Bad(u16, String),
}

impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self {
        ReadError::Io(e)
    }
}

/// Decode `%XX` escapes. In query strings `+` also means a space.
pub fn percent_decode(s: &str, plus_is_space: bool) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if plus_is_space && b[i] == b'+' {
            b' '
        } else {
            b[i]
        });
        i += 1;
    }
    out
}

/// Percent-encode bytes for a path segment or query value.
pub fn percent_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len());
    for &c in b {
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~') {
            s.push(c as char);
        } else {
            s.push_str(&format!("%{c:02X}"));
        }
    }
    s
}

fn read_line(r: &mut impl BufRead, limit: usize) -> Result<Option<String>, ReadError> {
    let mut buf = Vec::new();
    let n = r
        .by_ref()
        .take(limit as u64 + 1)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() > limit {
        return Err(ReadError::Bad(431, "header line too long".into()));
    }
    while buf.last().is_some_and(|c| *c == b'\n' || *c == b'\r') {
        buf.pop();
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| ReadError::Bad(400, "header is not UTF-8".into()))
}

pub fn read_request(r: &mut impl BufRead, max_body: usize) -> Result<Request, ReadError> {
    let line = loop {
        match read_line(r, 16 * 1024)? {
            None => return Err(ReadError::Closed),
            Some(l) if l.is_empty() => continue,
            Some(l) => break l,
        }
    };
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(ReadError::Bad(400, "bad request line".into()));
    };
    let http10 = version == "HTTP/1.0";
    let mut headers = Vec::new();
    loop {
        let Some(h) = read_line(r, 16 * 1024)? else {
            return Err(ReadError::Closed);
        };
        if h.is_empty() {
            break;
        }
        if headers.len() > 100 {
            return Err(ReadError::Bad(431, "too many headers".into()));
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let get = |n: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(n))
            .map(|(_, v)| v.clone())
    };
    if get("transfer-encoding").is_some() {
        return Err(ReadError::Bad(411, "send a Content-Length body".into()));
    }
    let len = match get("content-length") {
        Some(v) => v
            .parse::<usize>()
            .map_err(|_| ReadError::Bad(400, "bad Content-Length".into()))?,
        None => 0,
    };
    if len > max_body {
        return Err(ReadError::Bad(413, format!("body over {max_body} bytes")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let conn = get("connection").unwrap_or_default().to_ascii_lowercase();
    let keep_alive = if http10 {
        conn == "keep-alive"
    } else {
        conn != "close"
    };
    let (path, qs) = target.split_once('?').unwrap_or((target, ""));
    let segments = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|seg| percent_decode(seg, false))
        .collect();
    let query = qs
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (
                String::from_utf8_lossy(&percent_decode(k, true)).into_owned(),
                percent_decode(v, true),
            )
        })
        .collect();
    Ok(Request {
        method: method.to_string(),
        segments,
        query,
        headers,
        body,
        keep_alive,
    })
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, j: &crate::json::Json) -> Response {
        Response {
            status,
            content_type: "application/json",
            body: j.to_string_compact().into_bytes(),
        }
    }
    pub fn bytes(status: u16, body: Vec<u8>) -> Response {
        Response {
            status,
            content_type: "application/octet-stream",
            body,
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

pub fn write_response(w: &mut impl Write, r: &Response, keep_alive: bool) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
        r.status,
        reason(r.status),
        r.content_type,
        r.body.len(),
        if keep_alive { "keep-alive" } else { "close" }
    );
    w.write_all(head.as_bytes())?;
    w.write_all(&r.body)?;
    w.flush()
}

/// A small blocking client that keeps one connection open.
pub struct Client {
    host: String,
    token: Option<String>,
    conn: Option<BufReader<TcpStream>>,
    pub timeout: Duration,
}

impl Client {
    /// `url` is `http://host:port` (a trailing slash is fine).
    pub fn new(url: &str, token: Option<String>) -> io::Result<Client> {
        let rest = url.strip_prefix("http://").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "only http:// URLs are supported",
            )
        })?;
        Ok(Client {
            host: rest.trim_end_matches('/').to_string(),
            token,
            conn: None,
            timeout: Duration::from_secs(60),
        })
    }

    /// Send one request; returns status and body. Retries once on a stale
    /// keep-alive connection.
    pub fn request(&mut self, method: &str, path: &str, body: &[u8]) -> io::Result<(u16, Vec<u8>)> {
        for attempt in 0..2 {
            let fresh = self.conn.is_none();
            if fresh {
                let s = TcpStream::connect(&self.host)?;
                s.set_read_timeout(Some(self.timeout))?;
                s.set_nodelay(true)?;
                self.conn = Some(BufReader::new(s));
            }
            match self.try_request(method, path, body) {
                Ok(r) => return Ok(r),
                Err(e) => {
                    self.conn = None;
                    if fresh || attempt == 1 {
                        return Err(e);
                    }
                }
            }
        }
        unreachable!()
    }

    fn try_request(&mut self, method: &str, path: &str, body: &[u8]) -> io::Result<(u16, Vec<u8>)> {
        let conn = self.conn.as_mut().unwrap();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\n",
            self.host,
            body.len()
        );
        if let Some(t) = &self.token {
            head.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        head.push_str("\r\n");
        let s = conn.get_mut();
        s.write_all(head.as_bytes())?;
        s.write_all(body)?;
        s.flush()?;
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
        let mut line = String::new();
        if conn.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed the connection",
            ));
        }
        let status: u16 = line
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| bad("bad status line"))?;
        let mut len = 0usize;
        let mut close = false;
        loop {
            let mut h = String::new();
            conn.read_line(&mut h)?;
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse().map_err(|_| bad("bad Content-Length"))?;
                } else if k.eq_ignore_ascii_case("connection")
                    && v.trim().eq_ignore_ascii_case("close")
                {
                    close = true;
                }
            }
        }
        let mut out = vec![0u8; len];
        conn.read_exact(&mut out)?;
        if close {
            self.conn = None;
        }
        Ok((status, out))
    }
}
