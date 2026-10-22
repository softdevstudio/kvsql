//! Expression binding and evaluation, with SQLite's NULL and number rules.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use super::parser::{BinOp, Expr};
use super::value::{fmt_real, sql_cmp, total_cmp, ColType, Value};
use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Func {
    Length,
    Lower,
    Upper,
    Abs,
    Coalesce,
    IfNull,
    NullIf,
    Substr,
    Typeof,
    Round,
    Min,
    Max,
    Hex,
    Instr,
    Replace,
    Trim,
    LTrim,
    RTrim,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggKind {
    Count,
    Sum,
    Total,
    Avg,
    Min,
    Max,
    GroupConcat,
}

/// A bound expression: column references are positions in the row.
#[derive(Clone, Debug)]
pub enum E {
    Lit(Value),
    Col(usize),
    Agg(usize),
    Neg(Box<E>),
    Not(Box<E>),
    Bin(BinOp, Box<E>, Box<E>),
    IsNull(Box<E>, bool),
    In(Box<E>, Vec<E>, bool),
    Between(Box<E>, Box<E>, Box<E>, bool),
    Like(Box<E>, Box<E>, bool),
    Func(Func, Vec<E>),
    Cast(Box<E>, ColType),
    Case(Option<Box<E>>, Vec<(E, E)>, Option<Box<E>>),
    /// Apply an affinity before a comparison.
    Aff(Box<E>, Aff),
}

impl E {
    /// True when the expression reads no column and no aggregate.
    pub fn is_const(&self) -> bool {
        match self {
            E::Lit(_) => true,
            E::Col(_) | E::Agg(_) => false,
            E::Neg(a) | E::Not(a) | E::IsNull(a, _) | E::Cast(a, _) | E::Aff(a, _) => a.is_const(),
            E::Bin(_, a, b) | E::Like(a, b, _) => a.is_const() && b.is_const(),
            E::Between(a, b, c, _) => a.is_const() && b.is_const() && c.is_const(),
            E::In(a, l, _) => a.is_const() && l.iter().all(E::is_const),
            E::Func(_, args) => args.iter().all(E::is_const),
            E::Case(o, w, e) => {
                o.as_ref().is_none_or(|x| x.is_const())
                    && w.iter().all(|(a, b)| a.is_const() && b.is_const())
                    && e.as_ref().is_none_or(|x| x.is_const())
            }
        }
    }

    /// Highest column position read, if any.
    pub fn max_col(&self) -> Option<usize> {
        let mut m: Option<usize> = None;
        self.visit(&mut |e| {
            if let E::Col(i) = e {
                m = Some(m.map_or(*i, |x| x.max(*i)));
            }
        });
        m
    }

    fn visit(&self, f: &mut dyn FnMut(&E)) {
        f(self);
        match self {
            E::Lit(_) | E::Col(_) | E::Agg(_) => {}
            E::Neg(a) | E::Not(a) | E::IsNull(a, _) | E::Cast(a, _) | E::Aff(a, _) => a.visit(f),
            E::Bin(_, a, b) | E::Like(a, b, _) => {
                a.visit(f);
                b.visit(f);
            }
            E::Between(a, b, c, _) => {
                a.visit(f);
                b.visit(f);
                c.visit(f);
            }
            E::In(a, l, _) => {
                a.visit(f);
                l.iter().for_each(|x| x.visit(f));
            }
            E::Func(_, args) => args.iter().for_each(|x| x.visit(f)),
            E::Case(o, w, e) => {
                if let Some(o) = o {
                    o.visit(f);
                }
                for (a, b) in w {
                    a.visit(f);
                    b.visit(f);
                }
                if let Some(e) = e {
                    e.visit(f);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct AggSpec {
    pub kind: AggKind,
    pub arg: Option<E>,
    pub sep: Option<E>,
    pub distinct: bool,
}

/// Columns visible to an expression: (table alias, column name, type) per
/// position.
pub type Scope = Vec<(String, String, ColType)>;

/// SQLite's comparison affinity. A STRICT column of type INTEGER or REAL has
/// numeric affinity, TEXT has text affinity, BLOB and ANY have none. A CAST
/// has the affinity of its type. Every other expression has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Aff {
    Numeric,
    Text,
}

fn type_aff(t: ColType) -> Option<Aff> {
    match t {
        ColType::Integer | ColType::Real => Some(Aff::Numeric),
        ColType::Text => Some(Aff::Text),
        _ => None,
    }
}

/// Apply an affinity to a value the way SQLite does before a comparison.
pub fn apply_aff(a: Aff, v: Value) -> Value {
    match (a, v) {
        (Aff::Numeric, Value::Text(s)) => match text_to_number(&s) {
            Some(n) => n,
            None => Value::Text(s),
        },
        (Aff::Text, Value::Int(i)) => Value::Text(i.to_string()),
        (Aff::Text, Value::Real(r)) => Value::Text(fmt_real(r)),
        (_, v) => v,
    }
}

/// Text that is a well-formed number, as SQLite's numeric affinity sees it:
/// optional spaces around an optional sign, digits, an optional fraction and
/// an optional exponent.
fn text_to_number(s: &str) -> Option<Value> {
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    let (mant, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let (int_part, frac) = match mant.find('.') {
        Some(i) => (&mant[..i], Some(&mant[i + 1..])),
        None => (mant, None),
    };
    let digits = |x: &str| x.bytes().all(|b| b.is_ascii_digit());
    if !digits(int_part)
        || !frac.is_none_or(digits)
        || int_part.len() + frac.map_or(0, str::len) == 0
    {
        return None;
    }
    if let Some(e) = exp {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        if e.is_empty() || !digits(e) {
            return None;
        }
    }
    if frac.is_none() && exp.is_none() {
        if let Ok(i) = t.trim_start_matches('+').parse::<i64>() {
            return Some(Value::Int(i));
        }
    }
    t.trim_start_matches('+')
        .parse::<f64>()
        .ok()
        .map(Value::real)
}

pub struct Binder<'a> {
    pub scope: &'a Scope,
    pub params: &'a [Value],
    /// Where aggregate calls go; `None` where aggregates are not allowed.
    pub aggs: Option<&'a mut Vec<AggSpec>>,
    /// Result column aliases a bare name may refer to (HAVING, GROUP BY,
    /// ORDER BY), used when the name is not a source column.
    pub aliases: &'a [(String, Expr)],
}

fn is_comparison(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
            | BinOp::Is
            | BinOp::IsNot
    )
}

/// Wrap `e` so the affinity is applied when it is evaluated. Constants are
/// converted right away, which also lets the planner see a plain value.
fn wrap_aff(e: E, a: Aff) -> E {
    match e {
        E::Lit(v) => E::Lit(apply_aff(a, v)),
        e => E::Aff(Box::new(e), a),
    }
}

fn sql_err<T>(m: impl Into<String>) -> Result<T> {
    Err(Error::Sql(m.into()))
}

pub fn resolve(scope: &Scope, table: Option<&str>, name: &str) -> Result<usize> {
    let mut found = None;
    for (i, (t, c, _)) in scope.iter().enumerate() {
        let table_ok = table.is_none_or(|q| q.eq_ignore_ascii_case(t));
        if table_ok && c.eq_ignore_ascii_case(name) {
            if found.is_some() {
                return sql_err(format!("ambiguous column name: {name}"));
            }
            found = Some(i);
        }
    }
    match (found, table) {
        (Some(i), _) => Ok(i),
        (None, Some(t)) => sql_err(format!("no such column: {t}.{name}")),
        (None, None) => sql_err(format!("no such column: {name}")),
    }
}

impl Binder<'_> {
    pub fn bind(&mut self, e: &Expr) -> Result<E> {
        let b = |s: &mut Self, x: &Expr| s.bind(x).map(Box::new);
        Ok(match e {
            Expr::Lit(v) => E::Lit(v.clone()),
            Expr::Param(i) => E::Lit(self.params[*i].clone()),
            Expr::Column { table, name } => match resolve(self.scope, table.as_deref(), name) {
                Ok(i) => E::Col(i),
                Err(e) => {
                    let alias = table
                        .is_none()
                        .then(|| {
                            self.aliases
                                .iter()
                                .find(|(a, _)| a.eq_ignore_ascii_case(name))
                        })
                        .flatten()
                        .map(|(_, x)| x.clone());
                    match alias {
                        Some(x) if !matches!(&x, Expr::Column { table: None, name: n } if n.eq_ignore_ascii_case(name)) => {
                            return self.bind(&x)
                        }
                        _ => return Err(e),
                    }
                }
            },
            Expr::Neg(a) => E::Neg(b(self, a)?),
            Expr::Not(a) => E::Not(b(self, a)?),
            Expr::Bin(op, x, y) => {
                let (l, r) = (self.bind(x)?, self.bind(y)?);
                if is_comparison(*op) {
                    let (l, r) = self.compare_aff(l, r);
                    E::Bin(*op, Box::new(l), Box::new(r))
                } else {
                    E::Bin(*op, Box::new(l), Box::new(r))
                }
            }
            Expr::IsNull(a, n) => E::IsNull(b(self, a)?, *n),
            Expr::In {
                expr,
                list,
                negated,
            } => {
                let x = self.bind(expr)?;
                let mut items: Vec<E> = list.iter().map(|x| self.bind(x)).collect::<Result<_>>()?;
                // The left side's affinity applies to every value in the list.
                if let Some(a) = self.aff_of(&x) {
                    items = items.into_iter().map(|e| wrap_aff(e, a)).collect();
                }
                E::In(Box::new(x), items, *negated)
            }
            Expr::Between {
                expr,
                lo,
                hi,
                negated,
            } => {
                let (x, l, h) = (self.bind(expr)?, self.bind(lo)?, self.bind(hi)?);
                let (x1, l) = self.compare_aff(x.clone(), l);
                let (x2, h) = self.compare_aff(x.clone(), h);
                if matches!(x1, E::Aff(..)) || matches!(x2, E::Aff(..)) {
                    // The two halves convert the middle value differently:
                    // spell it out as x >= lo AND x <= hi.
                    let both = E::Bin(
                        BinOp::And,
                        Box::new(E::Bin(BinOp::Ge, Box::new(x1), Box::new(l))),
                        Box::new(E::Bin(BinOp::Le, Box::new(x2), Box::new(h))),
                    );
                    if *negated {
                        E::Not(Box::new(both))
                    } else {
                        both
                    }
                } else {
                    E::Between(Box::new(x), Box::new(l), Box::new(h), *negated)
                }
            }
            Expr::Like {
                expr,
                pattern,
                negated,
            } => E::Like(b(self, expr)?, b(self, pattern)?, *negated),
            Expr::Cast(a, t) => E::Cast(b(self, a)?, *t),
            Expr::Case {
                operand,
                whens,
                otherwise,
            } => E::Case(
                match operand {
                    Some(o) => Some(b(self, o)?),
                    None => None,
                },
                whens
                    .iter()
                    .map(|(c, r)| Ok((self.bind(c)?, self.bind(r)?)))
                    .collect::<Result<_>>()?,
                match otherwise {
                    Some(o) => Some(b(self, o)?),
                    None => None,
                },
            ),
            Expr::Func {
                name,
                args,
                star,
                distinct,
            } => return self.bind_func(name, args, *star, *distinct),
        })
    }

    /// Comparison affinity of a bound expression.
    fn aff_of(&self, e: &E) -> Option<Aff> {
        match e {
            E::Col(i) => self.scope.get(*i).and_then(|c| type_aff(c.2)),
            E::Cast(_, t) => type_aff(*t),
            E::Aff(_, a) => Some(*a),
            _ => None,
        }
    }

    /// SQLite's rules for the two sides of a comparison: if one side has
    /// numeric affinity and the other text or none, the other side gets
    /// numeric affinity; if one side has text affinity and the other none,
    /// the other side gets text affinity.
    fn compare_aff(&self, l: E, r: E) -> (E, E) {
        match (self.aff_of(&l), self.aff_of(&r)) {
            (Some(Aff::Numeric), Some(Aff::Text) | None) => (l, wrap_aff(r, Aff::Numeric)),
            (Some(Aff::Text) | None, Some(Aff::Numeric)) => (wrap_aff(l, Aff::Numeric), r),
            (Some(Aff::Text), None) => (l, wrap_aff(r, Aff::Text)),
            (None, Some(Aff::Text)) => (wrap_aff(l, Aff::Text), r),
            _ => (l, r),
        }
    }

    fn bind_func(&mut self, name: &str, args: &[Expr], star: bool, distinct: bool) -> Result<E> {
        let n = args.len();
        let agg = match (name, n, star) {
            ("count", 0, true) | ("count", 1, false) => Some(AggKind::Count),
            ("sum", 1, false) => Some(AggKind::Sum),
            ("total", 1, false) => Some(AggKind::Total),
            ("avg", 1, false) => Some(AggKind::Avg),
            ("min", 1, false) => Some(AggKind::Min),
            ("max", 1, false) => Some(AggKind::Max),
            ("group_concat", 1 | 2, false) => Some(AggKind::GroupConcat),
            _ => None,
        };
        if let Some(kind) = agg {
            let Some(aggs) = self.aggs.take() else {
                return sql_err(format!("misuse of aggregate function {name}()"));
            };
            let mut inner = Binder {
                scope: self.scope,
                params: self.params,
                aggs: None,
                aliases: self.aliases,
            };
            let arg = if star {
                None
            } else {
                Some(inner.bind(&args[0])?)
            };
            let sep = if n == 2 {
                Some(inner.bind(&args[1])?)
            } else {
                None
            };
            aggs.push(AggSpec {
                kind,
                arg,
                sep,
                distinct,
            });
            let idx = aggs.len() - 1;
            self.aggs = Some(aggs);
            return Ok(E::Agg(idx));
        }
        if star || distinct {
            return sql_err(format!("{name}() does not take * or DISTINCT"));
        }
        let f = match (name, n) {
            ("length", 1) => Func::Length,
            ("lower", 1) => Func::Lower,
            ("upper", 1) => Func::Upper,
            ("abs", 1) => Func::Abs,
            ("coalesce", 2..) => Func::Coalesce,
            ("ifnull", 2) => Func::IfNull,
            ("nullif", 2) => Func::NullIf,
            ("substr" | "substring", 2 | 3) => Func::Substr,
            ("typeof", 1) => Func::Typeof,
            ("round", 1 | 2) => Func::Round,
            ("min", 2..) => Func::Min,
            ("max", 2..) => Func::Max,
            ("hex", 1) => Func::Hex,
            ("instr", 2) => Func::Instr,
            ("replace", 3) => Func::Replace,
            ("trim", 1 | 2) => Func::Trim,
            ("ltrim", 1 | 2) => Func::LTrim,
            ("rtrim", 1 | 2) => Func::RTrim,
            _ => return sql_err(format!("no such function: {name} with {n} argument(s)")),
        };
        let args = args.iter().map(|a| self.bind(a)).collect::<Result<_>>()?;
        Ok(E::Func(f, args))
    }
}

fn bool_val(b: Option<bool>) -> Value {
    match b {
        None => Value::Null,
        Some(true) => Value::Int(1),
        Some(false) => Value::Int(0),
    }
}

fn arith(op: BinOp, a: Value, b: Value) -> Result<Value> {
    let (Some(a), Some(b)) = (a.to_number(), b.to_number()) else {
        return Ok(Value::Null);
    };
    Ok(match (a, b) {
        (Value::Int(x), Value::Int(y)) => match op {
            BinOp::Add => x
                .checked_add(y)
                .map_or(Value::real(x as f64 + y as f64), Value::Int),
            BinOp::Sub => x
                .checked_sub(y)
                .map_or(Value::real(x as f64 - y as f64), Value::Int),
            BinOp::Mul => x
                .checked_mul(y)
                .map_or(Value::real(x as f64 * y as f64), Value::Int),
            BinOp::Div => {
                if y == 0 {
                    Value::Null
                } else {
                    x.checked_div(y)
                        .map_or(Value::real(x as f64 / y as f64), Value::Int)
                }
            }
            BinOp::Rem => {
                if y == 0 {
                    Value::Null
                } else {
                    Value::Int(x.checked_rem(y).unwrap_or(0))
                }
            }
            _ => unreachable!(),
        },
        (a, b) => {
            let f = |v: Value| match v {
                Value::Int(i) => i as f64,
                Value::Real(r) => r,
                _ => 0.0,
            };
            let (x, y) = (f(a), f(b));
            match op {
                BinOp::Add => Value::real(x + y),
                BinOp::Sub => Value::real(x - y),
                BinOp::Mul => Value::real(x * y),
                BinOp::Div => {
                    if y == 0.0 {
                        Value::Null
                    } else {
                        Value::real(x / y)
                    }
                }
                BinOp::Rem => {
                    let (xi, yi) = (x as i64, y as i64);
                    if yi == 0 {
                        Value::Null
                    } else {
                        Value::Real(xi.checked_rem(yi).unwrap_or(0) as f64)
                    }
                }
                _ => unreachable!(),
            }
        }
    })
}

/// SQL LIKE: `%` any run, `_` one character, ASCII case-insensitive.
pub fn like(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().map(|c| c.to_ascii_lowercase()).collect();
    let t: Vec<char> = text.chars().map(|c| c.to_ascii_lowercase()).collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star_p, mut star_t) = (usize::MAX, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '_' || (p[pi] != '%' && p[pi] == t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star_p = pi;
            star_t = ti;
            pi += 1;
        } else if star_p != usize::MAX {
            pi = star_p + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    pi == p.len()
}

pub fn cast(v: Value, t: ColType) -> Value {
    if v.is_null() {
        return v;
    }
    match t {
        ColType::Integer => match v.to_number() {
            Some(Value::Int(i)) => Value::Int(i),
            Some(Value::Real(r)) => {
                if r >= 9.223_372_036_854_776e18 {
                    Value::Int(i64::MAX)
                } else if r <= -9.223_372_036_854_776e18 {
                    Value::Int(i64::MIN)
                } else {
                    Value::Int(r.trunc() as i64)
                }
            }
            _ => Value::Int(0),
        },
        ColType::Real => match v.to_number() {
            Some(Value::Int(i)) => Value::Real(i as f64),
            Some(Value::Real(r)) => Value::Real(r),
            _ => Value::Real(0.0),
        },
        ColType::Text => Value::Text(v.to_text().unwrap_or_default()),
        ColType::Blob => match v {
            Value::Blob(b) => Value::Blob(b),
            other => Value::Blob(other.to_text().unwrap_or_default().into_bytes()),
        },
        ColType::Any => v,
    }
}

fn text_arg(v: &Value) -> Option<String> {
    v.to_text()
}

fn trim_set(args: &[Value]) -> Option<Vec<char>> {
    match args.get(1) {
        None => Some(vec![' ']),
        Some(v) => Some(text_arg(v)?.chars().collect()),
    }
}

fn call(f: Func, args: Vec<Value>) -> Result<Value> {
    let a0 = args.first().cloned().unwrap_or(Value::Null);
    Ok(match f {
        Func::Length => match &a0 {
            Value::Null => Value::Null,
            Value::Blob(b) => Value::Int(b.len() as i64),
            v => Value::Int(v.to_text().unwrap().chars().count() as i64),
        },
        Func::Lower => match &a0 {
            Value::Null => Value::Null,
            v => Value::Text(v.to_text().unwrap().to_ascii_lowercase()),
        },
        Func::Upper => match &a0 {
            Value::Null => Value::Null,
            v => Value::Text(v.to_text().unwrap().to_ascii_uppercase()),
        },
        Func::Abs => match a0.to_number() {
            None => Value::Null,
            Some(Value::Int(i)) => Value::Int(
                i.checked_abs()
                    .ok_or_else(|| Error::Sql("integer overflow".into()))?,
            ),
            Some(Value::Real(r)) => Value::Real(r.abs()),
            Some(_) => Value::Int(0),
        },
        Func::Coalesce | Func::IfNull => args
            .into_iter()
            .find(|v| !v.is_null())
            .unwrap_or(Value::Null),
        Func::NullIf => {
            if sql_cmp(&a0, &args[1]) == Some(Ordering::Equal) {
                Value::Null
            } else {
                a0
            }
        }
        Func::Substr => {
            if a0.is_null() || args[1].is_null() || args.get(2).is_some_and(Value::is_null) {
                return Ok(Value::Null);
            }
            let as_i = |v: &Value| match cast(v.clone(), ColType::Integer) {
                Value::Int(i) => i,
                _ => 0,
            };
            let blob = matches!(a0, Value::Blob(_));
            let items: Vec<Vec<u8>> = match &a0 {
                Value::Blob(b) => b.iter().map(|x| vec![*x]).collect(),
                v => v
                    .to_text()
                    .unwrap()
                    .chars()
                    .map(|c| c.to_string().into_bytes())
                    .collect(),
            };
            let len = items.len() as i64;
            let mut p1 = as_i(&args[1]);
            let mut p2 = args.get(2).map_or(i64::MAX / 4, as_i);
            let neg = p2 < 0;
            if neg {
                p2 = -p2;
            }
            if p1 < 0 {
                p1 += len;
                if p1 < 0 {
                    p2 += p1;
                    if p2 < 0 {
                        p2 = 0;
                    }
                    p1 = 0;
                }
            } else if p1 > 0 {
                p1 -= 1;
            } else if p2 > 0 {
                p2 -= 1;
            }
            if neg {
                p1 -= p2;
                if p1 < 0 {
                    p2 += p1;
                    p1 = 0;
                }
            }
            let start = p1.clamp(0, len) as usize;
            let end = (p1.saturating_add(p2)).clamp(0, len) as usize;
            let bytes: Vec<u8> = items[start..end.max(start)].concat();
            if blob {
                Value::Blob(bytes)
            } else {
                Value::Text(String::from_utf8(bytes).unwrap_or_default())
            }
        }
        Func::Typeof => Value::Text(a0.type_name().into()),
        Func::Round => {
            let digits = match args.get(1) {
                Some(v) if v.is_null() => return Ok(Value::Null),
                Some(v) => match cast(v.clone(), ColType::Integer) {
                    Value::Int(i) => i.clamp(0, 30),
                    _ => 0,
                },
                None => 0,
            };
            match a0.to_number() {
                None => Value::Null,
                Some(Value::Int(i)) => Value::Real(i as f64),
                Some(Value::Real(r)) => {
                    let m = 10f64.powi(digits as i32);
                    Value::real((r * m).round() / m)
                }
                Some(_) => Value::Real(0.0),
            }
        }
        Func::Min | Func::Max => {
            if args.iter().any(Value::is_null) {
                return Ok(Value::Null);
            }
            let want = if f == Func::Min {
                Ordering::Less
            } else {
                Ordering::Greater
            };
            let mut best = a0;
            for v in args.into_iter().skip(1) {
                if total_cmp(&v, &best) == want {
                    best = v;
                }
            }
            best
        }
        Func::Hex => match &a0 {
            Value::Null => Value::Text(String::new()),
            Value::Blob(b) => Value::Text(b.iter().map(|x| format!("{x:02X}")).collect()),
            v => Value::Text(
                v.to_text()
                    .unwrap()
                    .bytes()
                    .map(|x| format!("{x:02X}"))
                    .collect(),
            ),
        },
        Func::Instr => {
            if a0.is_null() || args[1].is_null() {
                return Ok(Value::Null);
            }
            let (h, n) = (a0.to_text().unwrap(), args[1].to_text().unwrap());
            match h.find(&n) {
                Some(byte) => Value::Int(h[..byte].chars().count() as i64 + 1),
                None => Value::Int(0),
            }
        }
        Func::Replace => {
            if args.iter().any(Value::is_null) {
                return Ok(Value::Null);
            }
            let (s, from, to) = (
                a0.to_text().unwrap(),
                args[1].to_text().unwrap(),
                args[2].to_text().unwrap(),
            );
            if from.is_empty() {
                Value::Text(s)
            } else {
                Value::Text(s.replace(&from, &to))
            }
        }
        Func::Trim | Func::LTrim | Func::RTrim => {
            if a0.is_null() || args.get(1).is_some_and(Value::is_null) {
                return Ok(Value::Null);
            }
            let s = a0.to_text().unwrap();
            let set = trim_set(&args).unwrap_or_default();
            let t: &str = match f {
                Func::Trim => s.trim_matches(|c| set.contains(&c)),
                Func::LTrim => s.trim_start_matches(|c| set.contains(&c)),
                _ => s.trim_end_matches(|c| set.contains(&c)),
            };
            Value::Text(t.to_string())
        }
    })
}

pub fn eval(e: &E, row: &[Value], aggs: &[Value]) -> Result<Value> {
    Ok(match e {
        E::Lit(v) => v.clone(),
        E::Col(i) => row[*i].clone(),
        E::Agg(i) => aggs[*i].clone(),
        E::Neg(a) => match eval(a, row, aggs)?.to_number() {
            None => Value::Null,
            Some(Value::Int(i)) => i.checked_neg().map_or(Value::Real(-(i as f64)), Value::Int),
            Some(Value::Real(r)) => Value::Real(-r),
            Some(_) => Value::Int(0),
        },
        E::Not(a) => bool_val(eval(a, row, aggs)?.truth().map(|b| !b)),
        E::IsNull(a, negated) => Value::Int(i64::from(eval(a, row, aggs)?.is_null() != *negated)),
        E::Bin(op, a, b) => {
            match op {
                BinOp::And => {
                    let x = eval(a, row, aggs)?.truth();
                    if x == Some(false) {
                        return Ok(Value::Int(0));
                    }
                    let y = eval(b, row, aggs)?.truth();
                    return Ok(match (x, y) {
                        (_, Some(false)) => Value::Int(0),
                        (Some(true), Some(true)) => Value::Int(1),
                        _ => Value::Null,
                    });
                }
                BinOp::Or => {
                    let x = eval(a, row, aggs)?.truth();
                    if x == Some(true) {
                        return Ok(Value::Int(1));
                    }
                    let y = eval(b, row, aggs)?.truth();
                    return Ok(match (x, y) {
                        (_, Some(true)) => Value::Int(1),
                        (Some(false), Some(false)) => Value::Int(0),
                        _ => Value::Null,
                    });
                }
                _ => {}
            }
            let x = eval(a, row, aggs)?;
            let y = eval(b, row, aggs)?;
            match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => arith(*op, x, y)?,
                BinOp::Concat => match (x.to_text(), y.to_text()) {
                    (Some(s), Some(t)) => Value::Text(s + &t),
                    _ => Value::Null,
                },
                BinOp::Is => Value::Int(i64::from(
                    (x.is_null() && y.is_null()) || sql_cmp(&x, &y) == Some(Ordering::Equal),
                )),
                BinOp::IsNot => Value::Int(i64::from(
                    !((x.is_null() && y.is_null()) || sql_cmp(&x, &y) == Some(Ordering::Equal)),
                )),
                _ => {
                    let c = sql_cmp(&x, &y);
                    bool_val(c.map(|o| match op {
                        BinOp::Eq => o == Ordering::Equal,
                        BinOp::Ne => o != Ordering::Equal,
                        BinOp::Lt => o == Ordering::Less,
                        BinOp::Le => o != Ordering::Greater,
                        BinOp::Gt => o == Ordering::Greater,
                        BinOp::Ge => o != Ordering::Less,
                        _ => unreachable!(),
                    }))
                }
            }
        }
        E::In(a, list, negated) => {
            let x = eval(a, row, aggs)?;
            if x.is_null() {
                return Ok(Value::Null);
            }
            let mut saw_null = false;
            let mut found = false;
            for item in list {
                let v = eval(item, row, aggs)?;
                match sql_cmp(&x, &v) {
                    None => saw_null = true,
                    Some(Ordering::Equal) => {
                        found = true;
                        break;
                    }
                    _ => {}
                }
            }
            let r = if found {
                Some(true)
            } else if saw_null {
                None
            } else {
                Some(false)
            };
            bool_val(r.map(|b| b != *negated))
        }
        E::Between(a, lo, hi, negated) => {
            let x = eval(a, row, aggs)?;
            let l = eval(lo, row, aggs)?;
            let h = eval(hi, row, aggs)?;
            let ge = sql_cmp(&x, &l).map(|o| o != Ordering::Less);
            let le = sql_cmp(&x, &h).map(|o| o != Ordering::Greater);
            let r = match (ge, le) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            };
            bool_val(r.map(|b| b != *negated))
        }
        E::Like(a, p, negated) => {
            let x = eval(a, row, aggs)?;
            let pat = eval(p, row, aggs)?;
            match (x.to_text(), pat.to_text()) {
                (Some(s), Some(p)) => Value::Int(i64::from(like(&p, &s) != *negated)),
                _ => Value::Null,
            }
        }
        E::Func(f, args) => {
            let vals = args
                .iter()
                .map(|a| eval(a, row, aggs))
                .collect::<Result<Vec<_>>>()?;
            call(*f, vals)?
        }
        E::Cast(a, t) => cast(eval(a, row, aggs)?, *t),
        E::Aff(a, af) => apply_aff(*af, eval(a, row, aggs)?),
        E::Case(operand, whens, otherwise) => {
            let base = match operand {
                Some(o) => Some(eval(o, row, aggs)?),
                None => None,
            };
            for (c, r) in whens {
                let cv = eval(c, row, aggs)?;
                let hit = match &base {
                    Some(b) => sql_cmp(b, &cv) == Some(Ordering::Equal),
                    None => cv.truth() == Some(true),
                };
                if hit {
                    return eval(r, row, aggs);
                }
            }
            match otherwise {
                Some(o) => eval(o, row, aggs)?,
                None => Value::Null,
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Aggregates

/// A value ordered by [`total_cmp`], for DISTINCT sets and grouping.
#[derive(Clone, Debug)]
pub struct OrdVal(pub Value);

impl PartialEq for OrdVal {
    fn eq(&self, o: &Self) -> bool {
        total_cmp(&self.0, &o.0) == Ordering::Equal
    }
}
impl Eq for OrdVal {}
impl PartialOrd for OrdVal {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl std::cmp::Ord for OrdVal {
    fn cmp(&self, o: &Self) -> Ordering {
        total_cmp(&self.0, &o.0)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Acc {
    count: i64,
    int_sum: i64,
    real_sum: f64,
    any_real: bool,
    overflow: bool,
    best: Option<Value>,
    parts: Vec<String>,
    sep: Option<String>,
    seen: BTreeSet<OrdVal>,
}

impl Acc {
    pub fn update(&mut self, spec: &AggSpec, row: &[Value]) -> Result<()> {
        let Some(arg) = &spec.arg else {
            self.count += 1;
            return Ok(());
        };
        let v = eval(arg, row, &[])?;
        if v.is_null() {
            return Ok(());
        }
        if spec.distinct && !self.seen.insert(OrdVal(v.clone())) {
            return Ok(());
        }
        self.count += 1;
        match spec.kind {
            AggKind::Count => {}
            AggKind::Sum | AggKind::Total | AggKind::Avg => match v.to_number() {
                Some(Value::Int(i)) => {
                    self.real_sum += i as f64;
                    match self.int_sum.checked_add(i) {
                        Some(s) => self.int_sum = s,
                        None => self.overflow = true,
                    }
                }
                Some(Value::Real(r)) => {
                    self.any_real = true;
                    self.real_sum += r;
                }
                _ => {}
            },
            AggKind::Min | AggKind::Max => {
                let want = if spec.kind == AggKind::Min {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
                if self.best.as_ref().is_none_or(|b| total_cmp(&v, b) == want) {
                    self.best = Some(v);
                }
            }
            AggKind::GroupConcat => {
                if self.sep.is_none() {
                    let s = match &spec.sep {
                        Some(e) => eval(e, row, &[])?.to_text().unwrap_or_default(),
                        None => ",".into(),
                    };
                    self.sep = Some(s);
                }
                self.parts.push(v.to_text().unwrap_or_default());
            }
        }
        Ok(())
    }

    pub fn finish(&self, spec: &AggSpec) -> Result<Value> {
        Ok(match spec.kind {
            AggKind::Count => Value::Int(self.count),
            AggKind::Sum => {
                if self.count == 0 {
                    Value::Null
                } else if self.any_real {
                    Value::real(self.real_sum)
                } else if self.overflow {
                    return Err(Error::Sql("integer overflow".into()));
                } else {
                    Value::Int(self.int_sum)
                }
            }
            AggKind::Total => Value::real(self.real_sum),
            AggKind::Avg => {
                if self.count == 0 {
                    Value::Null
                } else {
                    Value::real(self.real_sum / self.count as f64)
                }
            }
            AggKind::Min | AggKind::Max => self.best.clone().unwrap_or(Value::Null),
            AggKind::GroupConcat => {
                if self.count == 0 {
                    Value::Null
                } else {
                    Value::Text(self.parts.join(self.sep.as_deref().unwrap_or(",")))
                }
            }
        })
    }
}

/// Short display name for an expression, used as a result column name.
pub fn display(e: &Expr) -> String {
    match e {
        Expr::Column { name, .. } => name.clone(),
        Expr::Lit(v) => match v {
            Value::Text(s) => format!("'{s}'"),
            Value::Real(r) => fmt_real(*r),
            v => v.to_string(),
        },
        Expr::Func {
            name, args, star, ..
        } => {
            if *star {
                format!("{name}(*)")
            } else {
                format!(
                    "{name}({})",
                    args.iter().map(display).collect::<Vec<_>>().join(", ")
                )
            }
        }
        _ => "expr".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_patterns() {
        assert!(like("a%", "abc"));
        assert!(like("%B%", "abc"));
        assert!(like("_b_", "abc"));
        assert!(!like("_b", "abc"));
        assert!(like("%", ""));
        assert!(like("a%c%e", "abcde"));
        assert!(!like("a%c%f", "abcde"));
    }
}
