//! The query language of the console: a small subset of SQL, turned into
//! Memoire statements (Nuage DB has no SQL engine).
//!
//! ```text
//! SELECT * | COUNT(*) | col, ... FROM table [WHERE cond]
//!        [ORDER BY col [ASC|DESC], ...] [LIMIT n] [OFFSET n]
//! UPDATE table SET col = value, ... [WHERE cond]
//! DELETE FROM table [WHERE cond]
//!
//! cond:  col = | != | <> | < | <= | > | >= value
//!        col IS [NOT] NULL | col [NOT] LIKE 'pattern'
//!        col [NOT] IN (value, ...) | col [NOT] BETWEEN value AND value
//!        cond AND cond | cond OR cond | NOT cond | (cond)
//! value: 'text' | 12 | -3.5 | TRUE | FALSE | NULL
//! ```

use memoire_orm::ast::{DeleteAst, Projection, SelectAst, UpdateAst};
use memoire_orm::{AggFunc, BinOp, ColumnRef, ColumnType, Error, Expr, Order, Result, TableSchema, Value};

/// A parsed statement.
#[derive(Debug)]
pub(crate) enum Statement {
    Select(SelectAst),
    Update(UpdateAst),
    Delete(DeleteAst),
}

impl Statement {
    /// `true` for statements that change data (they need a confirmation).
    pub(crate) fn writes(&self) -> bool {
        !matches!(self, Statement::Select(_))
    }

    /// `SELECT COUNT(*)` of the rows a write would touch.
    pub(crate) fn preview(&self) -> Option<SelectAst> {
        let (table, filter) = match self {
            Statement::Update(u) => (u.table, u.filter.clone()),
            Statement::Delete(d) => (d.table, d.filter.clone()),
            Statement::Select(_) => return None,
        };
        let mut s = SelectAst::new(table);
        s.projection = Projection::Exprs(vec![Expr::Aggregate { func: AggFunc::Count, arg: None, distinct: false }]);
        s.filter = filter;
        Some(s)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Word(String),
    Quoted(String),
    Text(String),
    Number(String),
    Sym(&'static str),
}

fn syntax(msg: impl Into<String>) -> Error {
    Error::Validation(format!("query: {}", msg.into()))
}

fn tokenize(src: &str) -> Result<Vec<Tok>> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(Tok::Word(chars[start..i].iter().collect()));
        } else if c.is_ascii_digit() || (c == '-' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) {
            let start = i;
            i += 1;
            while i < chars.len()
                && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == 'e' || chars[i] == 'E')
            {
                i += 1;
            }
            out.push(Tok::Number(chars[start..i].iter().collect()));
        } else if c == '\'' || c == '"' {
            let mut s = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err(syntax("unterminated string")),
                    Some(&q) if q == c && chars.get(i + 1) == Some(&c) => {
                        s.push(c);
                        i += 2;
                    }
                    Some(&q) if q == c => {
                        i += 1;
                        break;
                    }
                    Some(&ch) => {
                        s.push(ch);
                        i += 1;
                    }
                }
            }
            out.push(if c == '\'' { Tok::Text(s) } else { Tok::Quoted(s) });
        } else {
            let two: String = chars[i..chars.len().min(i + 2)].iter().collect();
            let sym = match two.as_str() {
                "<=" => "<=",
                ">=" => ">=",
                "<>" => "<>",
                "!=" => "!=",
                _ => match c {
                    '=' => "=",
                    '<' => "<",
                    '>' => ">",
                    '(' => "(",
                    ')' => ")",
                    ',' => ",",
                    '*' => "*",
                    ';' => ";",
                    _ => return Err(syntax(format!("unexpected `{c}`"))),
                },
            };
            i += sym.len();
            out.push(Tok::Sym(sym));
        }
    }
    while out.last() == Some(&Tok::Sym(";")) {
        out.pop();
    }
    Ok(out)
}

struct Parser<'a> {
    toks: Vec<Tok>,
    at: usize,
    table: Option<&'static TableSchema>,
    lookup: &'a dyn Fn(&str) -> Result<&'static TableSchema>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }

    fn keyword(&mut self, kw: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(kw)) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<()> {
        if self.keyword(kw) { Ok(()) } else { Err(syntax(format!("expected {kw}"))) }
    }

    fn sym(&mut self, s: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Sym(x)) if *x == s) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn ident(&mut self) -> Result<String> {
        match self.toks.get(self.at).cloned() {
            Some(Tok::Word(w)) | Some(Tok::Quoted(w)) => {
                self.at += 1;
                Ok(w)
            }
            _ => Err(syntax("expected a name")),
        }
    }

    fn table(&mut self) -> Result<&'static TableSchema> {
        let name = self.ident()?;
        let t = (self.lookup)(&name)?;
        self.table = Some(t);
        Ok(t)
    }

    fn column(&mut self) -> Result<(usize, Expr)> {
        let name = self.ident()?;
        let t = self.table.ok_or_else(|| syntax("no table"))?;
        let index = t
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&name))
            .ok_or_else(|| syntax(format!("`{}` has no column `{name}`", t.name)))?;
        let c = &t.columns[index];
        Ok((index, Expr::Column(ColumnRef { table: t.name, name: c.name, index, kind: c.kind })))
    }

    fn literal(&mut self, kind: ColumnType) -> Result<Value> {
        let tok = self.toks.get(self.at).cloned().ok_or_else(|| syntax("expected a value"))?;
        self.at += 1;
        let raw = match tok {
            Tok::Word(w) if w.eq_ignore_ascii_case("null") => return Ok(Value::Null),
            Tok::Word(w) if w.eq_ignore_ascii_case("true") => return Ok(Value::Bool(true)),
            Tok::Word(w) if w.eq_ignore_ascii_case("false") => return Ok(Value::Bool(false)),
            Tok::Text(s) | Tok::Number(s) => s,
            other => return Err(syntax(format!("expected a value, found {other:?}"))),
        };
        convert(&raw, kind)
    }

    fn cond(&mut self) -> Result<Expr> {
        let mut left = self.and()?;
        while self.keyword("or") {
            left = Expr::binary(BinOp::Or, left, self.and()?);
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr> {
        let mut left = self.not()?;
        while self.keyword("and") {
            left = Expr::binary(BinOp::And, left, self.not()?);
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr> {
        if self.keyword("not") {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        if self.sym("(") {
            let e = self.cond()?;
            if !self.sym(")") {
                return Err(syntax("expected `)`"));
            }
            return Ok(e);
        }
        let (index, col) = self.column()?;
        let kind = self.table.map_or(ColumnType::Text, |t| t.columns[index].kind);
        if self.keyword("is") {
            let negated = self.keyword("not");
            self.expect_keyword("null")?;
            return Ok(Expr::IsNull { expr: Box::new(col), negated });
        }
        let negated = self.keyword("not");
        if self.keyword("like") {
            let pattern = self.literal(ColumnType::Text)?;
            return Ok(Expr::Like {
                expr: Box::new(col),
                pattern: Box::new(Expr::Value(pattern)),
                case_insensitive: false,
                negated,
                escaped: false,
            });
        }
        if self.keyword("in") {
            if !self.sym("(") {
                return Err(syntax("expected `(` after IN"));
            }
            let mut list = vec![self.literal(kind)?];
            while self.sym(",") {
                list.push(self.literal(kind)?);
            }
            if !self.sym(")") {
                return Err(syntax("expected `)`"));
            }
            return Ok(Expr::InList { expr: Box::new(col), list, negated });
        }
        if self.keyword("between") {
            let low = self.literal(kind)?;
            self.expect_keyword("and")?;
            let high = self.literal(kind)?;
            return Ok(Expr::Between {
                expr: Box::new(col),
                low: Box::new(Expr::Value(low)),
                high: Box::new(Expr::Value(high)),
                negated,
            });
        }
        if negated {
            return Err(syntax("expected LIKE, IN or BETWEEN after NOT"));
        }
        let op = match self.toks.get(self.at) {
            Some(Tok::Sym("=")) => BinOp::Eq,
            Some(Tok::Sym("!=")) | Some(Tok::Sym("<>")) => BinOp::Ne,
            Some(Tok::Sym("<")) => BinOp::Lt,
            Some(Tok::Sym("<=")) => BinOp::Le,
            Some(Tok::Sym(">")) => BinOp::Gt,
            Some(Tok::Sym(">=")) => BinOp::Ge,
            _ => return Err(syntax("expected a comparison")),
        };
        self.at += 1;
        let v = self.literal(kind)?;
        Ok(Expr::binary(op, col, Expr::Value(v)))
    }

    fn number(&mut self) -> Result<u64> {
        match self.toks.get(self.at).cloned() {
            Some(Tok::Number(n)) => {
                self.at += 1;
                n.parse().map_err(|_| syntax(format!("`{n}` is not a count")))
            }
            _ => Err(syntax("expected a number")),
        }
    }

    fn statement(&mut self) -> Result<Statement> {
        let stmt = if self.keyword("select") {
            enum P {
                All,
                Count,
                Cols(Vec<String>),
            }
            let proj = if self.sym("*") {
                P::All
            } else if matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case("count"))
                && self.toks.get(self.at + 1) == Some(&Tok::Sym("("))
            {
                self.at += 2;
                if !(self.sym("*") && self.sym(")")) {
                    return Err(syntax("only COUNT(*) is supported"));
                }
                P::Count
            } else {
                let mut cols = vec![self.ident()?];
                while self.sym(",") {
                    cols.push(self.ident()?);
                }
                P::Cols(cols)
            };
            self.expect_keyword("from")?;
            let t = self.table()?;
            let mut s = SelectAst::new(t);
            match proj {
                P::All => {}
                P::Count => {
                    s.projection =
                        Projection::Exprs(vec![Expr::Aggregate { func: AggFunc::Count, arg: None, distinct: false }]);
                }
                P::Cols(cols) => {
                    let mut exprs = Vec::new();
                    for c in cols {
                        self.toks.insert(self.at, Tok::Word(c));
                        exprs.push(self.column()?.1);
                    }
                    s.projection = Projection::Exprs(exprs);
                }
            }
            if self.keyword("where") {
                s.filter = Some(self.cond()?);
            }
            if self.keyword("order") {
                self.expect_keyword("by")?;
                loop {
                    let (_, expr) = self.column()?;
                    let desc = if self.keyword("desc") {
                        true
                    } else {
                        self.keyword("asc");
                        false
                    };
                    s.order.push(Order { expr, desc, nulls: None });
                    if !self.sym(",") {
                        break;
                    }
                }
            }
            if self.keyword("limit") {
                s.limit = Some(self.number()?);
            }
            if self.keyword("offset") {
                s.offset = Some(self.number()?);
            }
            Statement::Select(s)
        } else if self.keyword("update") {
            let t = self.table()?;
            self.expect_keyword("set")?;
            let mut sets = Vec::new();
            loop {
                let (index, _) = self.column()?;
                if !self.sym("=") {
                    return Err(syntax("expected `=`"));
                }
                sets.push((index, Expr::Value(self.literal(t.columns[index].kind)?)));
                if !self.sym(",") {
                    break;
                }
            }
            let filter = if self.keyword("where") { Some(self.cond()?) } else { None };
            Statement::Update(UpdateAst { table: t, sets, filter })
        } else if self.keyword("delete") {
            self.expect_keyword("from")?;
            let t = self.table()?;
            let filter = if self.keyword("where") { Some(self.cond()?) } else { None };
            Statement::Delete(DeleteAst { table: t, filter })
        } else {
            return Err(syntax("start with SELECT, UPDATE or DELETE"));
        };
        if self.at < self.toks.len() {
            return Err(syntax(format!("unexpected {:?}", self.toks[self.at])));
        }
        Ok(stmt)
    }
}

/// A literal as a value of the column's type.
fn convert(raw: &str, kind: ColumnType) -> Result<Value> {
    let bad = || syntax(format!("`{raw}` is not a valid {kind:?}"));
    Ok(match kind {
        ColumnType::Bool => match raw.to_ascii_lowercase().as_str() {
            "1" | "t" | "true" => Value::Bool(true),
            "0" | "f" | "false" => Value::Bool(false),
            _ => return Err(bad()),
        },
        ColumnType::SmallInt | ColumnType::Int => Value::I32(raw.parse().map_err(|_| bad())?),
        ColumnType::BigInt => Value::I64(raw.parse().map_err(|_| bad())?),
        ColumnType::Real | ColumnType::Double => Value::F64(raw.parse().map_err(|_| bad())?),
        ColumnType::Uuid => Value::Uuid(raw.parse().map_err(|_| bad())?),
        ColumnType::Timestamp => Value::Timestamp(
            chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S%.f")
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f"))
                .map_err(|_| bad())?,
        ),
        ColumnType::TimestampTz => Value::TimestampTz(
            chrono::DateTime::parse_from_rfc3339(raw).map_err(|_| bad())?.with_timezone(&chrono::Utc),
        ),
        ColumnType::Date => Value::Date(raw.parse().map_err(|_| bad())?),
        ColumnType::Time => Value::Time(raw.parse().map_err(|_| bad())?),
        ColumnType::Json => Value::Json(serde_json::from_str(raw).map_err(|_| bad())?),
        _ => Value::Text(raw.to_owned()),
    })
}

/// Parses `src`; `lookup` gives the schema of a table by name.
pub(crate) fn parse(src: &str, lookup: &dyn Fn(&str) -> Result<&'static TableSchema>) -> Result<Statement> {
    let toks = tokenize(src)?;
    if toks.is_empty() {
        return Err(syntax("empty query"));
    }
    Parser { toks, at: 0, table: None, lookup }.statement()
}

#[cfg(test)]
mod tests {
    use super::*;
    use memoire_orm::prelude::*;

    #[derive(Model, Debug)]
    #[memoire(table = "people")]
    #[allow(dead_code)]
    struct Person {
        #[memoire(primary_key)]
        id: i64,
        name: String,
        age: Option<i32>,
    }

    fn lookup(name: &str) -> Result<&'static TableSchema> {
        if name == "people" { Ok(Person::SCHEMA) } else { Err(Error::NoSuchTable(name.into())) }
    }

    #[test]
    fn statements() {
        let s = parse("select name, age from people where (age >= 18 and name like 'A%') or id in (1, 2) order by age desc, id limit 5 offset 1;", &lookup).unwrap();
        let Statement::Select(s) = s else { panic!() };
        assert_eq!((s.limit, s.offset, s.order.len()), (Some(5), Some(1), 2));
        assert!(matches!(&s.projection, Projection::Exprs(e) if e.len() == 2));
        let u = parse("UPDATE people SET age = 30, name = 'It''s me' WHERE id = 3", &lookup).unwrap();
        assert!(u.writes() && u.preview().is_some());
        let Statement::Update(u) = u else { panic!() };
        assert_eq!(u.sets[1].1, Expr::Value(Value::Text("It's me".into())));
        assert!(parse("DELETE FROM people WHERE age IS NULL", &lookup).unwrap().writes());
        assert!(parse("select count(*) from people where age between 1 and 9", &lookup).is_ok());
        for bad in [
            "drop table people",
            "select * from nope",
            "select * from people where age = 'x'",
            "select * from people limit",
        ] {
            assert!(parse(bad, &lookup).is_err(), "{bad}");
        }
    }
}
