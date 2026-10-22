//! Tokenizer and recursive-descent parser for the supported SQL subset.

use crate::error::{Error, Result};
use crate::sql::value::{ColType, Value};

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    /// Unquoted identifier or keyword, lowercased.
    Word(String),
    /// "quoted" or `quoted` identifier, kept as written.
    Quoted(String),
    Str(String),
    Int(i64),
    Real(f64),
    Blob(Vec<u8>),
    Param,
    Sym(&'static str),
    Eof,
}

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Sql(msg.into()))
}

const SYMS: [&str; 21] = [
    "||", "<=", ">=", "<>", "!=", "==", "(", ")", ",", ";", "*", "+", "-", "/", "%", "<", ">", "=",
    ".", "?", "~",
];

fn tokenize(src: &str) -> Result<Vec<Tok>> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        if (c == b'x' || c == b'X') && b.get(i + 1) == Some(&b'\'') {
            let start = i + 2;
            let mut j = start;
            while j < b.len() && b[j] != b'\'' {
                j += 1;
            }
            if j >= b.len() {
                return err("unterminated blob literal");
            }
            let hex = &src[start..j];
            if !hex.len().is_multiple_of(2) {
                return err("blob literal needs an even number of hex digits");
            }
            let mut bytes = Vec::with_capacity(hex.len() / 2);
            for k in (0..hex.len()).step_by(2) {
                bytes.push(
                    u8::from_str_radix(&hex[k..k + 2], 16)
                        .map_err(|_| Error::Sql("bad hex digit in blob literal".into()))?,
                );
            }
            out.push(Tok::Blob(bytes));
            i = j + 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$') {
                i += 1;
            }
            out.push(Tok::Word(src[start..i].to_ascii_lowercase()));
            continue;
        }
        if c == b'"' || c == b'`' || c == b'[' {
            let close = if c == b'[' { b']' } else { c };
            let mut s = Vec::new();
            i += 1;
            loop {
                if i >= b.len() {
                    return err("unterminated quoted identifier");
                }
                if b[i] == close {
                    if close != b']' && b.get(i + 1) == Some(&close) {
                        s.push(close);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                s.push(b[i]);
                i += 1;
            }
            out.push(Tok::Quoted(String::from_utf8(s).unwrap_or_default()));
            continue;
        }
        if c == b'\'' {
            let mut s = Vec::new();
            i += 1;
            loop {
                if i >= b.len() {
                    return err("unterminated string literal");
                }
                if b[i] == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        s.push(b'\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                s.push(b[i]);
                i += 1;
            }
            out.push(Tok::Str(
                String::from_utf8(s).map_err(|_| Error::Sql("string is not UTF-8".into()))?,
            ));
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let start = i;
            let mut real = false;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i < b.len() && b[i] == b'.' {
                real = true;
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                let mut j = i + 1;
                if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
                    j += 1;
                }
                if j < b.len() && b[j].is_ascii_digit() {
                    real = true;
                    i = j;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let text = &src[start..i];
            if !real {
                match text.parse::<i64>() {
                    Ok(v) => out.push(Tok::Int(v)),
                    Err(_) => out.push(Tok::Real(text.parse().unwrap_or(f64::INFINITY))),
                }
            } else {
                out.push(Tok::Real(
                    text.parse()
                        .map_err(|_| Error::Sql(format!("bad number {text}")))?,
                ));
            }
            continue;
        }
        let mut matched = false;
        for s in SYMS {
            if src[i..].starts_with(s) {
                out.push(if s == "?" { Tok::Param } else { Tok::Sym(s) });
                i += s.len();
                matched = true;
                break;
            }
        }
        if !matched {
            return err(format!("unexpected character {:?}", c as char));
        }
    }
    out.push(Tok::Eof);
    Ok(out)
}

// ---------------------------------------------------------------------------
// AST

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Lit(Value),
    Param(usize),
    Column {
        table: Option<String>,
        name: String,
    },
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    IsNull(Box<Expr>, bool),
    In {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        lo: Box<Expr>,
        hi: Box<Expr>,
        negated: bool,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
    },
    Func {
        name: String,
        args: Vec<Expr>,
        star: bool,
        distinct: bool,
    },
    Cast(Box<Expr>, ColType),
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Is,
    IsNot,
    And,
    Or,
}

#[derive(Clone, Debug)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColType,
    pub not_null: bool,
    pub primary_key: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
}

#[derive(Clone, Debug)]
pub struct FromItem {
    pub table: String,
    pub alias: Option<String>,
    pub join: JoinKind,
    pub on: Option<Expr>,
}

#[derive(Clone, Debug)]
pub enum SelectItem {
    Star,
    TableStar(String),
    Expr(Expr, Option<String>),
}

#[derive(Clone, Debug)]
pub struct Select {
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    pub from: Vec<FromItem>,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<(Expr, bool)>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    CreateTable {
        name: String,
        if_not_exists: bool,
        columns: Vec<ColumnDef>,
    },
    DropTable {
        name: String,
        if_exists: bool,
    },
    CreateIndex {
        name: String,
        table: String,
        column: String,
        if_not_exists: bool,
    },
    DropIndex {
        name: String,
        if_exists: bool,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
    },
    Update {
        table: String,
        sets: Vec<(String, Expr)>,
        filter: Option<Expr>,
    },
    Delete {
        table: String,
        filter: Option<Expr>,
    },
    Select(Box<Select>),
}

/// Parse one or more statements separated by semicolons. Returns the
/// statements and the number of `?` parameters they use.
pub fn parse(src: &str) -> Result<(Vec<Stmt>, usize)> {
    let mut p = Parser {
        toks: tokenize(src)?,
        pos: 0,
        params: 0,
    };
    let mut out = Vec::new();
    loop {
        while p.eat_sym(";") {}
        if p.peek() == &Tok::Eof {
            break;
        }
        out.push(p.statement()?);
        if !p.eat_sym(";") && p.peek() != &Tok::Eof {
            return err(format!("unexpected {} after statement", p.describe()));
        }
    }
    if out.is_empty() {
        return err("no statement");
    }
    Ok((out, p.params))
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    params: usize,
}

const RESERVED: &[&str] = &[
    "select", "from", "where", "group", "by", "having", "order", "limit", "offset", "insert",
    "into", "values", "update", "set", "delete", "create", "table", "drop", "index", "on", "and",
    "or", "not", "is", "in", "like", "between", "as", "join", "inner", "left", "outer", "cross",
    "case", "when", "then", "else", "end", "null", "distinct", "asc", "desc", "primary", "exists",
    "if", "cast", "union",
];

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos]
    }
    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)]
    }
    fn next(&mut self) -> Tok {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }
    fn describe(&self) -> String {
        match self.peek() {
            Tok::Word(w) => format!("'{w}'"),
            Tok::Quoted(w) => format!("\"{w}\""),
            Tok::Str(s) => format!("'{s}'"),
            Tok::Int(i) => i.to_string(),
            Tok::Real(r) => r.to_string(),
            Tok::Blob(_) => "blob".into(),
            Tok::Param => "?".into(),
            Tok::Sym(s) => format!("'{s}'"),
            Tok::Eof => "end of input".into(),
        }
    }
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Word(w) if w == kw)
    }
    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.next();
            true
        } else {
            false
        }
    }
    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            err(format!(
                "expected {} but found {}",
                kw.to_uppercase(),
                self.describe()
            ))
        }
    }
    fn eat_sym(&mut self, s: &str) -> bool {
        if matches!(self.peek(), Tok::Sym(x) if *x == s) {
            self.next();
            true
        } else {
            false
        }
    }
    fn expect_sym(&mut self, s: &str) -> Result<()> {
        if self.eat_sym(s) {
            Ok(())
        } else {
            err(format!("expected '{s}' but found {}", self.describe()))
        }
    }
    fn ident(&mut self) -> Result<String> {
        match self.peek().clone() {
            Tok::Word(w) if !RESERVED.contains(&w.as_str()) => {
                self.next();
                Ok(w)
            }
            Tok::Quoted(w) => {
                self.next();
                Ok(w)
            }
            _ => err(format!("expected a name but found {}", self.describe())),
        }
    }

    fn statement(&mut self) -> Result<Stmt> {
        if self.eat_kw("create") {
            if self.eat_kw("table") {
                return self.create_table();
            }
            if self.eat_kw("index") {
                return self.create_index();
            }
            return err("expected TABLE or INDEX after CREATE");
        }
        if self.eat_kw("drop") {
            let table = if self.eat_kw("table") {
                true
            } else if self.eat_kw("index") {
                false
            } else {
                return err("expected TABLE or INDEX after DROP");
            };
            let if_exists = if self.eat_kw("if") {
                self.expect_kw("exists")?;
                true
            } else {
                false
            };
            let name = self.ident()?;
            return Ok(if table {
                Stmt::DropTable { name, if_exists }
            } else {
                Stmt::DropIndex { name, if_exists }
            });
        }
        if self.eat_kw("insert") {
            return self.insert();
        }
        if self.eat_kw("update") {
            let table = self.ident()?;
            self.expect_kw("set")?;
            let mut sets = Vec::new();
            loop {
                let col = self.ident()?;
                self.expect_sym("=")?;
                sets.push((col, self.expr()?));
                if !self.eat_sym(",") {
                    break;
                }
            }
            let filter = if self.eat_kw("where") {
                Some(self.expr()?)
            } else {
                None
            };
            return Ok(Stmt::Update {
                table,
                sets,
                filter,
            });
        }
        if self.eat_kw("delete") {
            self.expect_kw("from")?;
            let table = self.ident()?;
            let filter = if self.eat_kw("where") {
                Some(self.expr()?)
            } else {
                None
            };
            return Ok(Stmt::Delete { table, filter });
        }
        if self.is_kw("select") {
            return Ok(Stmt::Select(Box::new(self.select()?)));
        }
        err(format!(
            "unsupported statement starting with {}",
            self.describe()
        ))
    }

    fn if_not_exists(&mut self) -> Result<bool> {
        if self.eat_kw("if") {
            self.expect_kw("not")?;
            self.expect_kw("exists")?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn create_table(&mut self) -> Result<Stmt> {
        let if_not_exists = self.if_not_exists()?;
        let name = self.ident()?;
        self.expect_sym("(")?;
        let mut columns: Vec<ColumnDef> = Vec::new();
        let mut table_pk: Option<String> = None;
        loop {
            if self.eat_kw("primary") {
                self.expect_kw("key")?;
                self.expect_sym("(")?;
                let c = self.ident()?;
                if self.eat_sym(",") {
                    return err("composite primary keys are not supported");
                }
                self.expect_sym(")")?;
                table_pk = Some(c);
            } else {
                let cname = self.ident()?;
                let ty = match self.peek().clone() {
                    Tok::Word(w) => {
                        let t = match w.as_str() {
                            "integer" | "int" | "bigint" => Some(ColType::Integer),
                            "real" | "float" | "double" => Some(ColType::Real),
                            "text" | "varchar" | "string" => Some(ColType::Text),
                            "blob" => Some(ColType::Blob),
                            "any" => Some(ColType::Any),
                            _ => None,
                        };
                        if t.is_some() {
                            self.next();
                            // Accept and ignore a length such as VARCHAR(40).
                            if self.eat_sym("(") {
                                while !self.eat_sym(")") {
                                    if self.next() == Tok::Eof {
                                        return err("unterminated type length");
                                    }
                                }
                            }
                        }
                        t
                    }
                    _ => None,
                };
                let Some(ty) = ty else {
                    return err(format!(
                        "column {cname} needs a type: INTEGER, REAL, TEXT, BLOB or ANY"
                    ));
                };
                let mut def = ColumnDef {
                    name: cname,
                    ty,
                    not_null: false,
                    primary_key: false,
                };
                loop {
                    if self.eat_kw("primary") {
                        self.expect_kw("key")?;
                        def.primary_key = true;
                    } else if self.eat_kw("not") {
                        self.expect_kw("null")?;
                        def.not_null = true;
                    } else if self.eat_kw("null") {
                    } else {
                        break;
                    }
                }
                columns.push(def);
            }
            if !self.eat_sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        // Accept SQLite's table options.
        while self.eat_kw("strict") || self.eat_kw("without") && self.eat_kw("rowid") {
            self.eat_sym(",");
        }
        if let Some(pk) = table_pk {
            match columns.iter_mut().find(|c| c.name == pk) {
                Some(c) => c.primary_key = true,
                None => return err(format!("primary key column {pk} is not defined")),
            }
        }
        Ok(Stmt::CreateTable {
            name,
            if_not_exists,
            columns,
        })
    }

    fn create_index(&mut self) -> Result<Stmt> {
        let if_not_exists = self.if_not_exists()?;
        let name = self.ident()?;
        self.expect_kw("on")?;
        let table = self.ident()?;
        self.expect_sym("(")?;
        let column = self.ident()?;
        if self.eat_sym(",") {
            return err("indexes on more than one column are not supported yet");
        }
        self.expect_sym(")")?;
        Ok(Stmt::CreateIndex {
            name,
            table,
            column,
            if_not_exists,
        })
    }

    fn insert(&mut self) -> Result<Stmt> {
        self.expect_kw("into")?;
        let table = self.ident()?;
        let columns = if self.eat_sym("(") {
            let mut cols = vec![self.ident()?];
            while self.eat_sym(",") {
                cols.push(self.ident()?);
            }
            self.expect_sym(")")?;
            Some(cols)
        } else {
            None
        };
        self.expect_kw("values")?;
        let mut rows = Vec::new();
        loop {
            self.expect_sym("(")?;
            let mut row = vec![self.expr()?];
            while self.eat_sym(",") {
                row.push(self.expr()?);
            }
            self.expect_sym(")")?;
            rows.push(row);
            if !self.eat_sym(",") {
                break;
            }
        }
        Ok(Stmt::Insert {
            table,
            columns,
            rows,
        })
    }

    fn select(&mut self) -> Result<Select> {
        self.expect_kw("select")?;
        let distinct = self.eat_kw("distinct");
        if !distinct {
            self.eat_kw("all");
        }
        let mut items = Vec::new();
        loop {
            if self.eat_sym("*") {
                items.push(SelectItem::Star);
            } else if matches!(self.peek(), Tok::Word(_) | Tok::Quoted(_))
                && self.peek_at(1) == &Tok::Sym(".")
                && self.peek_at(2) == &Tok::Sym("*")
            {
                let t = self.ident()?;
                self.next();
                self.next();
                items.push(SelectItem::TableStar(t));
            } else {
                let e = self.expr()?;
                let alias = self.alias()?;
                items.push(SelectItem::Expr(e, alias));
            }
            if !self.eat_sym(",") {
                break;
            }
        }
        let mut from = Vec::new();
        if self.eat_kw("from") {
            from.push(self.table_ref(JoinKind::Inner)?);
            loop {
                if self.eat_sym(",") {
                    from.push(self.table_ref(JoinKind::Inner)?);
                } else if self.eat_kw("join")
                    || (self.eat_kw("inner") && self.expect_kw("join").is_ok())
                {
                    let mut f = self.table_ref(JoinKind::Inner)?;
                    if self.eat_kw("on") {
                        f.on = Some(self.expr()?);
                    }
                    from.push(f);
                } else if self.eat_kw("cross") {
                    self.expect_kw("join")?;
                    from.push(self.table_ref(JoinKind::Inner)?);
                } else if self.eat_kw("left") {
                    self.eat_kw("outer");
                    self.expect_kw("join")?;
                    let mut f = self.table_ref(JoinKind::Left)?;
                    self.expect_kw("on")?;
                    f.on = Some(self.expr()?);
                    from.push(f);
                } else {
                    break;
                }
            }
        }
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        let mut group_by = Vec::new();
        if self.eat_kw("group") {
            self.expect_kw("by")?;
            group_by.push(self.expr()?);
            while self.eat_sym(",") {
                group_by.push(self.expr()?);
            }
        }
        let having = if self.eat_kw("having") {
            Some(self.expr()?)
        } else {
            None
        };
        let mut order_by = Vec::new();
        if self.eat_kw("order") {
            self.expect_kw("by")?;
            loop {
                let e = self.expr()?;
                let desc = if self.eat_kw("desc") {
                    true
                } else {
                    self.eat_kw("asc");
                    false
                };
                order_by.push((e, desc));
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        let (mut limit, mut offset) = (None, None);
        if self.eat_kw("limit") {
            limit = Some(self.expr()?);
            if self.eat_kw("offset") {
                offset = Some(self.expr()?);
            } else if self.eat_sym(",") {
                // LIMIT offset, count
                offset = limit.take();
                limit = Some(self.expr()?);
            }
        }
        Ok(Select {
            distinct,
            items,
            from,
            filter,
            group_by,
            having,
            order_by,
            limit,
            offset,
        })
    }

    /// Optional `AS name` or bare name after an expression or table.
    fn alias(&mut self) -> Result<Option<String>> {
        let bare = matches!(self.peek(), Tok::Word(w) if !RESERVED.contains(&w.as_str()))
            || matches!(self.peek(), Tok::Quoted(_));
        if self.eat_kw("as") || bare {
            Ok(Some(self.ident()?))
        } else {
            Ok(None)
        }
    }

    fn table_ref(&mut self, join: JoinKind) -> Result<FromItem> {
        let table = self.ident()?;
        let alias = self.alias()?;
        Ok(FromItem {
            table,
            alias,
            join,
            on: None,
        })
    }

    // Expressions, lowest precedence first.

    pub fn expr(&mut self) -> Result<Expr> {
        self.or()
    }

    fn or(&mut self) -> Result<Expr> {
        let mut l = self.and()?;
        while self.eat_kw("or") {
            let r = self.and()?;
            l = Expr::Bin(BinOp::Or, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn and(&mut self) -> Result<Expr> {
        let mut l = self.not()?;
        while self.eat_kw("and") {
            let r = self.not()?;
            l = Expr::Bin(BinOp::And, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn not(&mut self) -> Result<Expr> {
        if self.eat_kw("not") {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.equality()
    }

    fn equality(&mut self) -> Result<Expr> {
        let mut l = self.relational()?;
        loop {
            if self.eat_sym("=") || self.eat_sym("==") {
                let r = self.relational()?;
                l = Expr::Bin(BinOp::Eq, Box::new(l), Box::new(r));
            } else if self.eat_sym("!=") || self.eat_sym("<>") {
                let r = self.relational()?;
                l = Expr::Bin(BinOp::Ne, Box::new(l), Box::new(r));
            } else if self.eat_kw("is") {
                let negated = self.eat_kw("not");
                if self.eat_kw("null") {
                    l = Expr::IsNull(Box::new(l), negated);
                } else {
                    let r = self.relational()?;
                    let op = if negated { BinOp::IsNot } else { BinOp::Is };
                    l = Expr::Bin(op, Box::new(l), Box::new(r));
                }
            } else if self.eat_kw("isnull") {
                l = Expr::IsNull(Box::new(l), false);
            } else if self.eat_kw("notnull") {
                l = Expr::IsNull(Box::new(l), true);
            } else {
                let negated = if self.is_kw("not")
                    && matches!(self.peek_at(1), Tok::Word(w) if w == "in" || w == "like" || w == "between" || w == "null")
                {
                    self.next();
                    true
                } else {
                    false
                };
                if self.eat_kw("null") {
                    l = Expr::IsNull(Box::new(l), true);
                } else if self.eat_kw("in") {
                    self.expect_sym("(")?;
                    let mut list = Vec::new();
                    if !self.eat_sym(")") {
                        list.push(self.expr()?);
                        while self.eat_sym(",") {
                            list.push(self.expr()?);
                        }
                        self.expect_sym(")")?;
                    }
                    l = Expr::In {
                        expr: Box::new(l),
                        list,
                        negated,
                    };
                } else if self.eat_kw("like") {
                    let pattern = self.relational()?;
                    l = Expr::Like {
                        expr: Box::new(l),
                        pattern: Box::new(pattern),
                        negated,
                    };
                } else if self.eat_kw("between") {
                    let lo = self.relational()?;
                    self.expect_kw("and")?;
                    let hi = self.relational()?;
                    l = Expr::Between {
                        expr: Box::new(l),
                        lo: Box::new(lo),
                        hi: Box::new(hi),
                        negated,
                    };
                } else if negated {
                    return err("expected IN, LIKE or BETWEEN after NOT");
                } else {
                    break;
                }
            }
        }
        Ok(l)
    }

    fn relational(&mut self) -> Result<Expr> {
        let mut l = self.additive()?;
        loop {
            let op = if self.eat_sym("<=") {
                BinOp::Le
            } else if self.eat_sym(">=") {
                BinOp::Ge
            } else if self.eat_sym("<") {
                BinOp::Lt
            } else if self.eat_sym(">") {
                BinOp::Gt
            } else {
                break;
            };
            let r = self.additive()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn additive(&mut self) -> Result<Expr> {
        let mut l = self.multiplicative()?;
        loop {
            let op = if self.eat_sym("+") {
                BinOp::Add
            } else if self.eat_sym("-") {
                BinOp::Sub
            } else {
                break;
            };
            let r = self.multiplicative()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn multiplicative(&mut self) -> Result<Expr> {
        let mut l = self.concat()?;
        loop {
            let op = if self.eat_sym("*") {
                BinOp::Mul
            } else if self.eat_sym("/") {
                BinOp::Div
            } else if self.eat_sym("%") {
                BinOp::Rem
            } else {
                break;
            };
            let r = self.concat()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn concat(&mut self) -> Result<Expr> {
        let mut l = self.unary()?;
        while self.eat_sym("||") {
            let r = self.unary()?;
            l = Expr::Bin(BinOp::Concat, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.eat_sym("-") {
            return Ok(match self.unary()? {
                Expr::Lit(Value::Int(i)) => Expr::Lit(match i.checked_neg() {
                    Some(n) => Value::Int(n),
                    None => Value::Real(-(i as f64)),
                }),
                Expr::Lit(Value::Real(r)) => Expr::Lit(Value::Real(-r)),
                e => Expr::Neg(Box::new(e)),
            });
        }
        if self.eat_sym("+") {
            return self.unary();
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr> {
        match self.next() {
            Tok::Int(i) => Ok(Expr::Lit(Value::Int(i))),
            Tok::Real(r) => Ok(Expr::Lit(Value::real(r))),
            Tok::Str(s) => Ok(Expr::Lit(Value::Text(s))),
            Tok::Blob(b) => Ok(Expr::Lit(Value::Blob(b))),
            Tok::Param => {
                self.params += 1;
                Ok(Expr::Param(self.params - 1))
            }
            Tok::Sym("(") => {
                let e = self.expr()?;
                self.expect_sym(")")?;
                Ok(e)
            }
            Tok::Word(w) if w == "null" => Ok(Expr::Lit(Value::Null)),
            Tok::Word(w) if w == "true" => Ok(Expr::Lit(Value::Int(1))),
            Tok::Word(w) if w == "false" => Ok(Expr::Lit(Value::Int(0))),
            Tok::Word(w) if w == "cast" => {
                self.expect_sym("(")?;
                let e = self.expr()?;
                self.expect_kw("as")?;
                let t = match self.next() {
                    Tok::Word(t) => match t.as_str() {
                        "integer" | "int" => ColType::Integer,
                        "real" | "float" | "double" => ColType::Real,
                        "text" => ColType::Text,
                        "blob" => ColType::Blob,
                        other => return err(format!("cannot CAST to {other}")),
                    },
                    _ => return err("expected a type in CAST"),
                };
                self.expect_sym(")")?;
                Ok(Expr::Cast(Box::new(e), t))
            }
            Tok::Word(w) if w == "case" => {
                let operand = if self.is_kw("when") {
                    None
                } else {
                    Some(Box::new(self.expr()?))
                };
                let mut whens = Vec::new();
                while self.eat_kw("when") {
                    let c = self.expr()?;
                    self.expect_kw("then")?;
                    whens.push((c, self.expr()?));
                }
                if whens.is_empty() {
                    return err("CASE needs at least one WHEN");
                }
                let otherwise = if self.eat_kw("else") {
                    Some(Box::new(self.expr()?))
                } else {
                    None
                };
                self.expect_kw("end")?;
                Ok(Expr::Case {
                    operand,
                    whens,
                    otherwise,
                })
            }
            t @ (Tok::Word(_) | Tok::Quoted(_)) => {
                let (name, quoted) = match t {
                    Tok::Word(w) => (w, false),
                    Tok::Quoted(w) => (w, true),
                    _ => unreachable!(),
                };
                if !quoted && RESERVED.contains(&name.as_str()) {
                    return err(format!("unexpected '{name}'"));
                }
                if !quoted && self.peek() == &Tok::Sym("(") {
                    self.next();
                    let mut args = Vec::new();
                    let mut star = false;
                    let distinct = self.eat_kw("distinct");
                    if self.eat_sym("*") {
                        star = true;
                    } else if self.peek() != &Tok::Sym(")") {
                        args.push(self.expr()?);
                        while self.eat_sym(",") {
                            args.push(self.expr()?);
                        }
                    }
                    self.expect_sym(")")?;
                    return Ok(Expr::Func {
                        name,
                        args,
                        star,
                        distinct,
                    });
                }
                if self.eat_sym(".") {
                    let col = self.ident()?;
                    return Ok(Expr::Column {
                        table: Some(name),
                        name: col,
                    });
                }
                Ok(Expr::Column { table: None, name })
            }
            Tok::Eof => err("unexpected end of input"),
            t => err(format!("unexpected {t:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_main_forms() {
        let (s, params) = parse(
            "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL);
             CREATE INDEX by_name ON users(name);
             INSERT INTO users (id, name) VALUES (1, 'Ada'), (2, ?);
             UPDATE users SET score = score * 1.5 WHERE id IN (1, 2) AND name LIKE 'A%';
             DELETE FROM users WHERE score IS NULL OR id BETWEEN 3 AND ?;
             SELECT u.name AS n, count(*) FROM users u LEFT JOIN orders o ON o.uid = u.id
               WHERE NOT u.id <> 1 GROUP BY u.name HAVING count(*) > 1 ORDER BY 2 DESC, n LIMIT 10 OFFSET 5;
             DROP INDEX IF EXISTS by_name; DROP TABLE users",
        )
        .unwrap();
        assert_eq!(s.len(), 8);
        assert_eq!(params, 2);
    }

    #[test]
    fn precedence() {
        let (s, _) = parse("SELECT 1 + 2 * 3 = 7 AND NOT 0 OR 0").unwrap();
        let Stmt::Select(sel) = &s[0] else { panic!() };
        let SelectItem::Expr(Expr::Bin(BinOp::Or, l, _), _) = &sel.items[0] else {
            panic!("{:?}", sel.items[0])
        };
        assert!(matches!(**l, Expr::Bin(BinOp::And, _, _)));
    }

    #[test]
    fn errors_are_reported() {
        assert!(parse("SELEC 1").is_err());
        assert!(parse("SELECT 'open").is_err());
        assert!(parse("CREATE TABLE t (a)").is_err());
        assert!(parse("SELECT 1 FROM").is_err());
    }
}
