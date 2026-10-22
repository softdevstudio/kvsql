//! A small JSON reader and writer, plus the mapping between JSON and SQL
//! values used on the wire.
//!
//! SQL values travel as: NULL as `null`, INTEGER as a JSON integer, REAL
//! as a JSON number with a decimal point or exponent, TEXT as a string,
//! BLOB as `{"$blob": "<base64>"}`.

use std::fmt::Write as _;

use kvsql_engine::sql::Value;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn obj(fields: Vec<(&str, Json)>) -> Json {
        Json::Obj(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    pub fn to_string_compact(&self) -> String {
        let mut s = String::new();
        write_json(self, &mut s);
        s
    }
}

fn write_json(j: &Json, out: &mut String) {
    match j {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Json::Real(r) => {
            if !r.is_finite() {
                out.push_str("null");
            } else if r.fract() == 0.0 && r.abs() < 1e15 {
                let _ = write!(out, "{r:.1}");
            } else {
                let _ = write!(out, "{r:?}");
            }
        }
        Json::Str(s) => write_str(s, out),
        Json::Arr(items) => {
            out.push('[');
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(x, out);
            }
            out.push(']');
        }
        Json::Obj(fields) => {
            out.push('{');
            for (i, (k, v)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_str(k, out);
                out.push(':');
                write_json(v, out);
            }
            out.push('}');
        }
    }
}

fn write_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parse a JSON document. Nesting is limited to 64 levels.
pub fn parse(src: &str) -> Result<Json, String> {
    let mut p = P {
        b: src.as_bytes(),
        i: 0,
    };
    let v = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("unexpected data at byte {}", p.i));
    }
    Ok(v)
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\n' | b'\r' | b'\t') {
            self.i += 1;
        }
    }
    fn err<T>(&self, m: &str) -> Result<T, String> {
        Err(format!("{m} at byte {}", self.i))
    }
    fn lit(&mut self, word: &str, v: Json) -> Result<Json, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            self.err("bad literal")
        }
    }
    fn value(&mut self, depth: u32) -> Result<Json, String> {
        if depth > 64 {
            return self.err("nested too deeply");
        }
        self.ws();
        let Some(&c) = self.b.get(self.i) else {
            return self.err("unexpected end");
        };
        match c {
            b'n' => self.lit("null", Json::Null),
            b't' => self.lit("true", Json::Bool(true)),
            b'f' => self.lit("false", Json::Bool(false)),
            b'"' => Ok(Json::Str(self.string()?)),
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Json::Arr(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Json::Arr(items));
                        }
                        _ => return self.err("expected , or ]"),
                    }
                }
            }
            b'{' => {
                self.i += 1;
                let mut fields = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Json::Obj(fields));
                }
                loop {
                    self.ws();
                    if self.b.get(self.i) != Some(&b'"') {
                        return self.err("expected a key");
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        return self.err("expected :");
                    }
                    self.i += 1;
                    let v = self.value(depth + 1)?;
                    fields.push((k, v));
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Json::Obj(fields));
                        }
                        _ => return self.err("expected , or }"),
                    }
                }
            }
            b'-' | b'0'..=b'9' => self.number(),
            _ => self.err("unexpected character"),
        }
    }
    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        let mut real = false;
        if self.b[self.i] == b'-' {
            self.i += 1;
        }
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'0'..=b'9' => {}
                b'.' | b'e' | b'E' | b'+' | b'-' => real = true,
                _ => break,
            }
            self.i += 1;
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).unwrap();
        if !real {
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Json::Int(i));
            }
        }
        match text.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Json::Real(f)),
            _ => self.err("bad number"),
        }
    }
    fn hex4(&mut self) -> Result<u32, String> {
        let s = self
            .b
            .get(self.i..self.i + 4)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u32::from_str_radix(h, 16).ok());
        match s {
            Some(v) => {
                self.i += 4;
                Ok(v)
            }
            None => self.err("bad \\u escape"),
        }
    }
    fn string(&mut self) -> Result<String, String> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(&c) = self.b.get(self.i) else {
                return self.err("unterminated string");
            };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    let Some(&e) = self.b.get(self.i) else {
                        return self.err("unterminated escape");
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) {
                                if self.b.get(self.i..self.i + 2) != Some(b"\\u") {
                                    return self.err("lone surrogate");
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return self.err("bad surrogate pair");
                                }
                                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                            }
                            match char::from_u32(cp) {
                                Some(ch) => out.push(ch),
                                None => return self.err("bad code point"),
                            }
                        }
                        _ => return self.err("bad escape"),
                    }
                }
                _ => {
                    // Copy a run of plain bytes; the input is valid UTF-8.
                    let start = self.i;
                    while self.i < self.b.len() && self.b[self.i] != b'"' && self.b[self.i] != b'\\'
                    {
                        if self.b[self.i] < 0x20 {
                            return self.err("control character in string");
                        }
                        self.i += 1;
                    }
                    out.push_str(std::str::from_utf8(&self.b[start..self.i]).unwrap());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Base64 (standard alphabet, padded)

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.len();
        let v = (chunk[0] as u32) << 16
            | (chunk.get(1).copied().unwrap_or(0) as u32) << 8
            | chunk.get(2).copied().unwrap_or(0) as u32;
        for k in 0..4 {
            if k <= n {
                out.push(B64[((v >> (18 - 6 * k)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return Err("bad base64".into()),
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// SQL values on the wire

pub fn from_value(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Int(i) => Json::Int(*i),
        Value::Real(r) => Json::Real(*r),
        Value::Text(s) => Json::Str(s.clone()),
        Value::Blob(b) => Json::obj(vec![("$blob", Json::Str(b64_encode(b)))]),
    }
}

pub fn to_value(j: &Json) -> Result<Value, String> {
    Ok(match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Int(i64::from(*b)),
        Json::Int(i) => Value::Int(*i),
        Json::Real(r) => Value::Real(*r),
        Json::Str(s) => Value::Text(s.clone()),
        Json::Obj(_) => match j.get("$blob").and_then(Json::as_str) {
            Some(b) => Value::Blob(b64_decode(b)?),
            None => return Err("objects other than {\"$blob\": ...} are not values".into()),
        },
        Json::Arr(_) => return Err("arrays are not values".into()),
    })
}

/// Bytes as JSON: a string when they are UTF-8, otherwise `{"$blob": ...}`.
pub fn from_bytes(b: &[u8]) -> Json {
    match std::str::from_utf8(b) {
        Ok(s) => Json::Str(s.to_string()),
        Err(_) => Json::obj(vec![("$blob", Json::Str(b64_encode(b)))]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let src = r#"{"a":[1,-2,3.5,1e3,true,null,"x\"\né😀"],"b":{}}"#;
        let j = parse(src).unwrap();
        let again = parse(&j.to_string_compact()).unwrap();
        assert_eq!(j, again);
        assert_eq!(
            j.get("a").unwrap(),
            &Json::Arr(vec![
                Json::Int(1),
                Json::Int(-2),
                Json::Real(3.5),
                Json::Real(1000.0),
                Json::Bool(true),
                Json::Null,
                Json::Str("x\"\né😀".into())
            ])
        );
        assert!(parse("[1,]").is_err());
        assert!(parse("{\"a\" 1}").is_err());
        assert!(parse(&"[".repeat(100)).is_err());
    }

    #[test]
    fn values() {
        for v in [
            Value::Null,
            Value::Int(-7),
            Value::Real(2.0),
            Value::Real(0.1),
            Value::Text("hé".into()),
            Value::Blob(vec![0, 255, 7]),
        ] {
            let wire = from_value(&v).to_string_compact();
            assert_eq!(to_value(&parse(&wire).unwrap()).unwrap(), v, "{wire}");
        }
    }

    #[test]
    fn base64() {
        for n in 0..20 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37) as u8).collect();
            assert_eq!(b64_decode(&b64_encode(&data)).unwrap(), data);
        }
        assert_eq!(b64_encode(b"hello"), "aGVsbG8=");
    }
}
