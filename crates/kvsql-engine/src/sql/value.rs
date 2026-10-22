//! SQL values, their ordering, and the byte encodings used for rows and keys.

use std::cmp::Ordering;
use std::fmt;

use crate::error::{Error, Result};

/// A SQL value. Reals are never NaN: a computation that would produce NaN
/// gives NULL instead, as SQLite does.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// A real, or NULL when it is NaN.
    pub fn real(f: f64) -> Value {
        if f.is_nan() {
            Value::Null
        } else {
            Value::Real(f)
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Int(_) => "integer",
            Value::Real(_) => "real",
            Value::Text(_) => "text",
            Value::Blob(_) => "blob",
        }
    }

    fn class(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Int(_) | Value::Real(_) => 1,
            Value::Text(_) => 2,
            Value::Blob(_) => 3,
        }
    }

    /// Text form used by `||` and CAST to TEXT.
    pub fn to_text(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Int(i) => Some(i.to_string()),
            Value::Real(r) => Some(fmt_real(*r)),
            Value::Text(s) => Some(s.clone()),
            Value::Blob(b) => Some(String::from_utf8_lossy(b).into_owned()),
        }
    }

    /// Numeric view for arithmetic: text and blobs read as numbers when they
    /// look like one, otherwise 0, as in SQLite.
    pub fn to_number(&self) -> Option<Value> {
        match self {
            Value::Null => None,
            Value::Int(_) | Value::Real(_) => Some(self.clone()),
            Value::Text(s) => Some(parse_number_prefix(s)),
            Value::Blob(b) => Some(parse_number_prefix(&String::from_utf8_lossy(b))),
        }
    }

    /// Truth value in WHERE: NULL is unknown, numbers are true when non-zero.
    pub fn truth(&self) -> Option<bool> {
        match self.to_number()? {
            Value::Int(i) => Some(i != 0),
            Value::Real(r) => Some(r != 0.0),
            _ => Some(false),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Real(r) => write!(f, "{}", fmt_real(*r)),
            Value::Text(s) => write!(f, "{s}"),
            Value::Blob(b) => {
                write!(f, "x'")?;
                for byte in b {
                    write!(f, "{byte:02x}")?;
                }
                write!(f, "'")
            }
        }
    }
}

pub fn fmt_real(r: f64) -> String {
    if r.is_finite() && r.fract() == 0.0 && r.abs() < 1e15 {
        format!("{r:.1}")
    } else {
        format!("{r}")
    }
}

/// Leading number in a string, like SQLite's text-to-number conversion.
fn parse_number_prefix(s: &str) -> Value {
    let t = s.trim_start();
    let bytes = t.as_bytes();
    let mut end = 0;
    if end < bytes.len() && (bytes[end] == b'+' || bytes[end] == b'-') {
        end += 1;
    }
    let digits_start = end;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    let mut is_real = false;
    if end < bytes.len() && bytes[end] == b'.' {
        is_real = true;
        end += 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
    }
    if end == digits_start {
        return Value::Int(0);
    }
    if end < bytes.len() && (bytes[end] == b'e' || bytes[end] == b'E') {
        let mut e = end + 1;
        if e < bytes.len() && (bytes[e] == b'+' || bytes[e] == b'-') {
            e += 1;
        }
        let ds = e;
        while e < bytes.len() && bytes[e].is_ascii_digit() {
            e += 1;
        }
        if e > ds {
            is_real = true;
            end = e;
        }
    }
    let num = &t[..end];
    if !is_real {
        if let Ok(i) = num.parse::<i64>() {
            return Value::Int(i);
        }
    }
    num.parse::<f64>().map(Value::real).unwrap_or(Value::Int(0))
}

/// Exact text-to-number conversion used when storing into a typed column:
/// the whole string must be a number.
pub fn parse_exact_number(s: &str) -> Option<Value> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(i) = t.parse::<i64>() {
        return Some(Value::Int(i));
    }
    let f: f64 = t.parse().ok()?;
    if f.is_finite() {
        Some(Value::Real(f))
    } else {
        None
    }
}

fn cmp_numeric(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Int(x), Value::Real(y)) => cmp_int_real(*x, *y),
        (Value::Real(x), Value::Int(y)) => cmp_int_real(*y, *x).reverse(),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        _ => Ordering::Equal,
    }
}

fn cmp_int_real(i: i64, r: f64) -> Ordering {
    if r >= 9.223_372_036_854_776e18 {
        return Ordering::Less;
    }
    if r < -9.223_372_036_854_776e18 {
        return Ordering::Greater;
    }
    let ri = r.trunc() as i64;
    match i.cmp(&ri) {
        Ordering::Equal => {
            let frac = r - r.trunc();
            if frac > 0.0 {
                Ordering::Less
            } else if frac < 0.0 {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        o => o,
    }
}

/// Total order used by ORDER BY, GROUP BY, DISTINCT, MIN and MAX:
/// NULL < numbers < text < blobs; text compares bytewise.
pub fn total_cmp(a: &Value, b: &Value) -> Ordering {
    match a.class().cmp(&b.class()) {
        Ordering::Equal => match (a, b) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Text(x), Value::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
            (Value::Blob(x), Value::Blob(y)) => x.cmp(y),
            _ => cmp_numeric(a, b),
        },
        o => o,
    }
}

/// Comparison in expressions: unknown (None) when either side is NULL.
pub fn sql_cmp(a: &Value, b: &Value) -> Option<Ordering> {
    if a.is_null() || b.is_null() {
        None
    } else {
        Some(total_cmp(a, b))
    }
}

// ---------------------------------------------------------------------------
// Column types

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColType {
    Integer,
    Real,
    Text,
    Blob,
    /// Any type; used for the columns of a plain keyspace's value.
    Any,
}

impl ColType {
    pub fn name(self) -> &'static str {
        match self {
            ColType::Integer => "INTEGER",
            ColType::Real => "REAL",
            ColType::Text => "TEXT",
            ColType::Blob => "BLOB",
            ColType::Any => "ANY",
        }
    }

    /// Convert a value for storage in a column of this type, as SQLite does
    /// for STRICT tables: lossless conversions only.
    pub fn coerce(self, v: Value, column: &str) -> Result<Value> {
        let bad = |v: &Value| {
            Err(Error::Constraint(format!(
                "cannot store {} value in {} column {column}",
                v.type_name().to_uppercase(),
                self.name()
            )))
        };
        Ok(match (self, v) {
            (_, Value::Null) => Value::Null,
            (ColType::Any, v) => v,
            (ColType::Integer, Value::Int(i)) => Value::Int(i),
            (ColType::Integer, Value::Real(r)) => {
                if r.fract() == 0.0 && r.abs() < 9.2e18 {
                    Value::Int(r as i64)
                } else {
                    return bad(&Value::Real(r));
                }
            }
            (ColType::Integer, Value::Text(s)) => match parse_exact_number(&s) {
                Some(Value::Int(i)) => Value::Int(i),
                Some(Value::Real(r)) if r.fract() == 0.0 && r.abs() < 9.2e18 => {
                    Value::Int(r as i64)
                }
                _ => return bad(&Value::Text(s)),
            },
            (ColType::Real, Value::Int(i)) => Value::Real(i as f64),
            (ColType::Real, Value::Real(r)) => Value::Real(r),
            (ColType::Real, Value::Text(s)) => match parse_exact_number(&s) {
                Some(Value::Int(i)) => Value::Real(i as f64),
                Some(Value::Real(r)) => Value::Real(r),
                _ => return bad(&Value::Text(s)),
            },
            (ColType::Text, Value::Text(s)) => Value::Text(s),
            (ColType::Text, v @ (Value::Int(_) | Value::Real(_))) => {
                Value::Text(v.to_text().unwrap())
            }
            (ColType::Blob, Value::Blob(b)) => Value::Blob(b),
            (_, v) => return bad(&v),
        })
    }
}

// ---------------------------------------------------------------------------
// Row encoding: one tag byte per column, then the data.

const T_NULL: u8 = 0;
const T_INT: u8 = 1;
const T_REAL: u8 = 2;
const T_TEXT: u8 = 3;
const T_BLOB: u8 = 4;

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn get_varint(b: &[u8], at: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*at).ok_or_else(bad_row)?;
        *at += 1;
        v |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(bad_row())
}

fn bad_row() -> Error {
    Error::Corrupt("row value is not a valid row encoding".into())
}

/// Encode a row: a varint column count, then each value.
pub fn encode_row(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 * values.len() + 2);
    put_varint(&mut out, values.len() as u64);
    for v in values {
        match v {
            Value::Null => out.push(T_NULL),
            Value::Int(i) => {
                out.push(T_INT);
                put_varint(&mut out, ((i << 1) ^ (i >> 63)) as u64);
            }
            Value::Real(r) => {
                out.push(T_REAL);
                out.extend_from_slice(&r.to_le_bytes());
            }
            Value::Text(s) => {
                out.push(T_TEXT);
                put_varint(&mut out, s.len() as u64);
                out.extend_from_slice(s.as_bytes());
            }
            Value::Blob(b) => {
                out.push(T_BLOB);
                put_varint(&mut out, b.len() as u64);
                out.extend_from_slice(b);
            }
        }
    }
    out
}

pub fn decode_row(b: &[u8]) -> Result<Vec<Value>> {
    let mut at = 0;
    let n = get_varint(b, &mut at)? as usize;
    if n > b.len() {
        return Err(bad_row());
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let tag = *b.get(at).ok_or_else(bad_row)?;
        at += 1;
        out.push(match tag {
            T_NULL => Value::Null,
            T_INT => {
                let z = get_varint(b, &mut at)?;
                Value::Int(((z >> 1) as i64) ^ -((z & 1) as i64))
            }
            T_REAL => {
                let s = b.get(at..at + 8).ok_or_else(bad_row)?;
                at += 8;
                Value::real(f64::from_le_bytes(s.try_into().unwrap()))
            }
            T_TEXT | T_BLOB => {
                let len = get_varint(b, &mut at)? as usize;
                let s = b.get(at..at + len).ok_or_else(bad_row)?;
                at += len;
                if tag == T_TEXT {
                    Value::Text(String::from_utf8(s.to_vec()).map_err(|_| bad_row())?)
                } else {
                    Value::Blob(s.to_vec())
                }
            }
            _ => return Err(bad_row()),
        });
    }
    if at != b.len() {
        return Err(bad_row());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Key encodings

/// The keyspace key for a primary key value: an INTEGER is 8 bytes
/// big-endian with the sign bit flipped (so byte order is numeric order),
/// TEXT is its UTF-8 bytes and BLOB is itself. This is the key the KV door
/// uses for the same row.
pub fn pk_key(v: &Value) -> Result<Vec<u8>> {
    match v {
        Value::Int(i) => Ok(((*i as u64) ^ (1 << 63)).to_be_bytes().to_vec()),
        Value::Text(s) if !s.is_empty() && s.len() <= crate::MAX_KEY_LEN => {
            Ok(s.as_bytes().to_vec())
        }
        Value::Blob(b) if !b.is_empty() && b.len() <= crate::MAX_KEY_LEN => Ok(b.clone()),
        Value::Text(s) => Err(Error::KeyTooLarge(s.len())),
        Value::Blob(b) => Err(Error::KeyTooLarge(b.len())),
        Value::Null => Err(Error::Constraint("primary key cannot be NULL".into())),
        Value::Real(_) => Err(Error::Constraint("primary key cannot be REAL".into())),
    }
}

/// Inverse of [`pk_key`] for a column type.
pub fn pk_value(t: ColType, key: &[u8]) -> Result<Value> {
    match t {
        ColType::Integer => {
            let b: [u8; 8] = key
                .try_into()
                .map_err(|_| Error::Corrupt("integer key is not 8 bytes".into()))?;
            Ok(Value::Int((u64::from_be_bytes(b) ^ (1 << 63)) as i64))
        }
        ColType::Text => Ok(Value::Text(
            String::from_utf8(key.to_vec())
                .map_err(|_| Error::Corrupt("text key is not UTF-8".into()))?,
        )),
        _ => Ok(Value::Blob(key.to_vec())),
    }
}

/// Prefix-free encoding of one value, used in index keys. Values that
/// compare equal in SQL encode the same: INTEGER and REAL share one numeric
/// form (the value as an ordered f64, with -0.0 folded into 0.0), so 1 and
/// 1.0 land on the same index entry prefix. Integers beyond 2^53 can share
/// a prefix with a neighbour; lookups re-check the WHERE clause, so that
/// only costs a row read.
pub fn index_part(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(0),
        Value::Int(i) => {
            out.push(1);
            out.extend_from_slice(&ordered_f64(*i as f64));
        }
        Value::Real(r) => {
            out.push(1);
            out.extend_from_slice(&ordered_f64(*r));
        }
        Value::Text(s) => {
            out.push(3);
            escape(s.as_bytes(), out);
        }
        Value::Blob(b) => {
            out.push(4);
            escape(b, out);
        }
    }
}

/// Bytes whose order is the numeric order of `r`.
fn ordered_f64(r: f64) -> [u8; 8] {
    let r = if r == 0.0 { 0.0 } else { r };
    let bits = r.to_bits();
    let k = if bits >> 63 == 1 {
        !bits
    } else {
        bits | (1 << 63)
    };
    k.to_be_bytes()
}

fn escape(b: &[u8], out: &mut Vec<u8>) {
    for &c in b {
        out.push(c);
        if c == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
}

/// Smallest byte string greater than every string starting with `prefix`.
pub fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut p = prefix.to_vec();
    while let Some(last) = p.pop() {
        if last < 0xFF {
            p.push(last + 1);
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_round_trip() {
        let row = vec![
            Value::Int(-5),
            Value::Int(i64::MIN),
            Value::Int(i64::MAX),
            Value::Real(1.5),
            Value::Null,
            Value::Text("héllo".into()),
            Value::Blob(vec![0, 1, 255]),
        ];
        assert_eq!(decode_row(&encode_row(&row)).unwrap(), row);
        assert!(decode_row(&[2, 9]).is_err());
    }

    #[test]
    fn keys_sort_like_values() {
        let ints = [i64::MIN, -100, -1, 0, 1, 99, i64::MAX];
        for w in ints.windows(2) {
            assert!(pk_key(&Value::Int(w[0])).unwrap() < pk_key(&Value::Int(w[1])).unwrap());
        }
        let reals = [-1e300, -2.5, -0.0, 0.5, 3.0, 1e300];
        let enc = |r: f64| {
            let mut o = Vec::new();
            index_part(&Value::Real(r), &mut o);
            o
        };
        for w in reals.windows(2) {
            assert!(enc(w[0]) <= enc(w[1]), "{} {}", w[0], w[1]);
        }
        let texts = ["", "a", "a\0", "a\0b", "ab", "b"];
        let enc_t = |s: &str| {
            let mut o = Vec::new();
            index_part(&Value::Text(s.into()), &mut o);
            o
        };
        for w in texts.windows(2) {
            assert!(enc_t(w[0]) < enc_t(w[1]), "{:?} {:?}", w[0], w[1]);
        }
        assert_eq!(
            pk_value(ColType::Integer, &pk_key(&Value::Int(-7)).unwrap()).unwrap(),
            Value::Int(-7)
        );
    }

    #[test]
    fn ordering_matches_sqlite_classes() {
        let vals = [
            Value::Null,
            Value::Int(-1),
            Value::Real(-0.5),
            Value::Int(0),
            Value::Real(2.5),
            Value::Int(3),
            Value::Text("A".into()),
            Value::Text("a".into()),
            Value::Blob(vec![0]),
        ];
        for w in vals.windows(2) {
            assert_eq!(
                total_cmp(&w[0], &w[1]),
                Ordering::Less,
                "{:?} {:?}",
                w[0],
                w[1]
            );
        }
        assert_eq!(
            total_cmp(&Value::Int(2), &Value::Real(2.0)),
            Ordering::Equal
        );
    }

    #[test]
    fn text_to_number() {
        assert_eq!(parse_number_prefix(" 12abc"), Value::Int(12));
        assert_eq!(parse_number_prefix("1.5e2x"), Value::Real(150.0));
        assert_eq!(parse_number_prefix("abc"), Value::Int(0));
        assert_eq!(parse_exact_number("12"), Some(Value::Int(12)));
        assert_eq!(parse_exact_number("12a"), None);
    }
}
