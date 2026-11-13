//! The query language of the console and of `POST /v1/sql` (websites in any
//! language): a small subset of SQL, turned into Memoire statements (Nuage DB
//! has no SQL engine).
//!
//! ```text
//! SELECT * | COUNT(*) | col, ... FROM table [WHERE cond]
//!        [ORDER BY col [ASC|DESC], ...] [LIMIT n] [OFFSET n]
//! INSERT INTO table [(col, ...)] VALUES (value, ...), ...
//! UPDATE table SET col = value, ... [WHERE cond]
//! DELETE FROM table [WHERE cond]
//! CREATE TABLE [IF NOT EXISTS] table (col type [PRIMARY KEY] [AUTOINCREMENT]
//!        [NOT NULL] [UNIQUE] [INDEX], ...)
//! DROP TABLE [IF EXISTS] table
//!
//! cond:  col = | != | <> | < | <= | > | >= value
//!        col IS [NOT] NULL | col [NOT] LIKE 'pattern'
//!        col [NOT] IN (value, ...) | col [NOT] BETWEEN value AND value
//!        cond AND cond | cond OR cond | NOT cond | (cond)
//! value: 'text' | 12 | -3.5 | TRUE | FALSE | NULL | ?
//! type:  BOOLEAN, SMALLINT, INT, BIGINT, REAL, DOUBLE, TEXT (VARCHAR...), BYTES,
//!        UUID, TIMESTAMP, TIMESTAMPTZ, DATE, TIME, JSON
//! ```
//!
//! `?` is a parameter: the next value of the request's `params`, never parsed
//! as SQL (no injection). `LIMIT ?` and `OFFSET ?` take one too.

use memoire_orm::ast::{DeleteAst, InsertAst, Projection, SelectAst, UpdateAst};
use memoire_orm::{AggFunc, BinOp, ColumnRef, ColumnType, Error, Expr, Order, Result, TableSchema, Value};
use serde_json::{Value as Json, json};

/// A parsed statement.
#[derive(Debug)]
pub(crate) enum Statement {
    Select(SelectAst),
    Insert(InsertAst),
    Update(UpdateAst),
    Delete(DeleteAst),
    /// The schema, as `memoire_orm::nuage::wire` writes it, and `IF NOT EXISTS`.
    Create(Json, bool),
    /// The table and `IF EXISTS`.
    Drop(String, bool),
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
            _ => return None,
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
    /// `?`: a parameter.
    Param,
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
                    '?' => {
                        i += 1;
                        out.push(Tok::Param);
                        continue;
                    }
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
    params: &'a [Json],
    next_param: usize,
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

    fn param(&mut self) -> Result<&Json> {
        let p = self.params.get(self.next_param).ok_or_else(|| {
            syntax(format!("parameter {} is missing: `params` has {}", self.next_param + 1, self.params.len()))
        })?;
        self.next_param += 1;
        Ok(p)
    }

    fn literal(&mut self, kind: ColumnType) -> Result<Value> {
        let tok = self.toks.get(self.at).cloned().ok_or_else(|| syntax("expected a value"))?;
        self.at += 1;
        let raw = match tok {
            Tok::Param => return from_json(self.param()?, kind),
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
            Some(Tok::Param) => {
                self.at += 1;
                self.param()?.as_u64().ok_or_else(|| syntax("LIMIT and OFFSET take a count"))
            }
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
        } else if self.keyword("insert") {
            self.expect_keyword("into")?;
            let t = self.table()?;
            let columns: Vec<usize> = if self.sym("(") {
                let mut cols = vec![self.column()?.0];
                while self.sym(",") {
                    cols.push(self.column()?.0);
                }
                if !self.sym(")") {
                    return Err(syntax("expected `)`"));
                }
                cols
            } else {
                (0..t.columns.len()).collect()
            };
            self.expect_keyword("values")?;
            let mut rows = Vec::new();
            loop {
                if !self.sym("(") {
                    return Err(syntax("expected `(` before the values"));
                }
                let mut given: Vec<Option<Value>> = vec![None; t.columns.len()];
                for (n, &c) in columns.iter().enumerate() {
                    if n > 0 && !self.sym(",") {
                        return Err(syntax(format!("{} values expected", columns.len())));
                    }
                    given[c] = Some(self.literal(t.columns[c].kind)?);
                }
                if !self.sym(")") {
                    return Err(syntax(format!("{} values expected", columns.len())));
                }
                let row = given
                    .into_iter()
                    .zip(t.columns)
                    .map(|(v, c)| match v {
                        Some(v) => Ok(v),
                        None if c.nullable || c.auto_increment => Ok(Value::Null),
                        None => Err(syntax(format!("column `{}` needs a value", c.name))),
                    })
                    .collect::<Result<Vec<Value>>>()?;
                rows.push(row);
                if !self.sym(",") {
                    break;
                }
            }
            Statement::Insert(InsertAst { table: t, rows })
        } else if self.keyword("create") {
            self.expect_keyword("table")?;
            let if_not_exists = self.keyword("if");
            if if_not_exists {
                self.expect_keyword("not")?;
                self.expect_keyword("exists")?;
            }
            let name = self.ident()?;
            check_name(&name)?;
            if !self.sym("(") {
                return Err(syntax("expected `(` and the columns"));
            }
            let mut columns = Vec::new();
            let mut pk = None;
            loop {
                let col = self.ident()?;
                check_name(&col)?;
                let kind = self.sql_type()?;
                let (mut primary, mut auto, mut not_null, mut unique, mut indexed) =
                    (false, false, false, false, false);
                loop {
                    if self.keyword("primary") {
                        self.expect_keyword("key")?;
                        primary = true;
                    } else if self.keyword("autoincrement") || self.keyword("auto_increment") {
                        auto = true;
                    } else if self.keyword("not") {
                        self.expect_keyword("null")?;
                        not_null = true;
                    } else if self.keyword("null") {
                    } else if self.keyword("unique") {
                        unique = true;
                    } else if self.keyword("index") || self.keyword("indexed") {
                        indexed = true;
                    } else {
                        break;
                    }
                }
                if primary {
                    if pk.is_some() {
                        return Err(syntax("one PRIMARY KEY column only"));
                    }
                    pk = Some(columns.len());
                }
                if auto && !matches!(kind, "int" | "bigint" | "smallint") {
                    return Err(syntax(format!("AUTOINCREMENT needs an integer column, not `{col}`")));
                }
                columns.push(json!({
                    "name": col, "field": col, "kind": kind,
                    "nullable": !(not_null || primary), "primaryKey": primary, "autoIncrement": auto,
                    "unique": unique, "indexed": indexed, "fulltext": false,
                    "sqlType": null, "default": null, "references": null,
                }));
                if !self.sym(",") {
                    break;
                }
            }
            if !self.sym(")") {
                return Err(syntax("expected `)` after the columns"));
            }
            let pk = pk.ok_or_else(|| syntax("a table needs a PRIMARY KEY column"))?;
            Statement::Create(json!({ "name": name, "pk": pk, "columns": columns, "indexes": [] }), if_not_exists)
        } else if self.keyword("drop") {
            self.expect_keyword("table")?;
            let if_exists = self.keyword("if");
            if if_exists {
                self.expect_keyword("exists")?;
            }
            Statement::Drop(self.ident()?, if_exists)
        } else {
            return Err(syntax("start with SELECT, INSERT, UPDATE, DELETE, CREATE TABLE or DROP TABLE"));
        };
        if self.at < self.toks.len() {
            return Err(syntax(format!("unexpected {:?}", self.toks[self.at])));
        }
        Ok(stmt)
    }
}

impl Parser<'_> {
    /// A column type of `CREATE TABLE`, as the wire protocol names it.
    fn sql_type(&mut self) -> Result<&'static str> {
        let word = self.ident()?.to_ascii_lowercase();
        let kind = match word.as_str() {
            "bool" | "boolean" => "bool",
            "smallint" | "int2" => "smallint",
            "int" | "integer" | "int4" => "int",
            "bigint" | "int8" => "bigint",
            "real" | "float4" => "real",
            "double" | "float" | "float8" | "numeric" | "decimal" => "double",
            "text" | "varchar" | "char" | "string" | "clob" | "character" => "text",
            "bytes" | "blob" | "bytea" | "binary" => "bytes",
            "uuid" => "uuid",
            "timestamp" | "datetime" => "timestamp",
            "timestamptz" => "timestamptz",
            "date" => "date",
            "time" => "time",
            "json" | "jsonb" => "json",
            other => return Err(syntax(format!("unknown column type `{other}`"))),
        };
        // `DOUBLE PRECISION`, `VARCHAR(255)`: the details change nothing here.
        self.keyword("precision");
        if self.sym("(") {
            while !self.sym(")") {
                if self.toks.get(self.at).is_none() {
                    return Err(syntax("expected `)`"));
                }
                self.at += 1;
            }
        }
        Ok(kind)
    }
}

/// Table and column names of `CREATE TABLE`: letters, digits and `_`.
fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    if ok { Ok(()) } else { Err(syntax(format!("invalid name `{name}`: use letters, digits and `_`"))) }
}

/// A parameter as a value of the column's type.
fn from_json(j: &Json, kind: ColumnType) -> Result<Value> {
    Ok(match (j, kind) {
        (Json::Null, _) => Value::Null,
        (j, ColumnType::Json) => Value::Json(j.clone()),
        (Json::Bool(b), ColumnType::Bool) => Value::Bool(*b),
        (Json::String(s), _) => convert(s, kind)?,
        (Json::Number(n), _) => convert(&n.to_string(), kind)?,
        (Json::Bool(b), _) => convert(&b.to_string(), kind)?,
        (other, _) => return Err(syntax(format!("parameter `{other}` is not a valid {kind:?}"))),
    })
}

/// A value as plain JSON, for websites: numbers, strings, booleans, `null`;
/// dates and UUIDs as strings, bytes in base64.
pub(crate) fn to_json(v: &Value) -> Json {
    use base64::Engine as _;
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::I32(i) => json!(i),
        Value::I64(i) => json!(i),
        Value::F64(f) => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
        Value::Text(s) => Json::String(s.clone()),
        Value::Bytes(b) => Json::String(base64::engine::general_purpose::STANDARD.encode(b)),
        Value::Uuid(u) => Json::String(u.to_string()),
        Value::Timestamp(t) => Json::String(t.format("%Y-%m-%dT%H:%M:%S%.f").to_string()),
        Value::TimestampTz(t) => Json::String(t.to_rfc3339()),
        Value::Date(d) => Json::String(d.to_string()),
        Value::Time(t) => Json::String(t.to_string()),
        Value::Json(j) => j.clone(),
        // `Value` is non-exhaustive: a later kind, in the wire protocol's form.
        other => memoire_orm::nuage::wire::value_to_json(other),
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
    parse_with(src, &[], lookup)
}

/// Parses `src` with the values of its `?` parameters.
pub(crate) fn parse_with(
    src: &str,
    params: &[Json],
    lookup: &dyn Fn(&str) -> Result<&'static TableSchema>,
) -> Result<Statement> {
    let toks = tokenize(src)?;
    if toks.is_empty() {
        return Err(syntax("empty query"));
    }
    let mut parser = Parser { toks, at: 0, table: None, lookup, params, next_param: 0 };
    let stmt = parser.statement()?;
    if parser.next_param != params.len() {
        return Err(syntax(format!("{} parameter(s) given, {} used", params.len(), parser.next_param)));
    }
    Ok(stmt)
}

/// What a statement gave.
pub(crate) enum Outcome {
    /// A `SELECT`: column names and rows.
    Rows(Vec<String>, Vec<Vec<Value>>),
    /// A change: rows affected, and the key an insert generated.
    Done(memoire_orm::ExecResult),
}

/// Runs a statement on `branch` of `engine`.
pub(crate) fn run(
    schemas: &memoire_orm::nuage::wire::SchemaCache,
    engine: &crate::engine::Engine,
    branch: &str,
    stmt: &Statement,
) -> Result<Outcome> {
    use crate::embedded::{execute_now, fetch_rows};
    use memoire_orm::Query;
    Ok(match stmt {
        Statement::Select(s) => Outcome::Rows(headers(s), fetch_rows(engine, branch, &Query::Select(s))?),
        // One row: its generated key comes back (`last_insert_id`).
        Statement::Insert(i) if i.rows.len() == 1 => {
            Outcome::Done(execute_now(engine, branch, &Query::InsertRow { table: i.table, values: &i.rows[0] })?)
        }
        Statement::Insert(i) => Outcome::Done(execute_now(engine, branch, &Query::Insert(i))?),
        Statement::Update(u) => Outcome::Done(execute_now(engine, branch, &Query::Update(u))?),
        Statement::Delete(d) => Outcome::Done(execute_now(engine, branch, &Query::Delete(d))?),
        Statement::Create(schema, if_not_exists) => {
            let table = schemas.schema(schema)?;
            Outcome::Done(execute_now(engine, branch, &Query::CreateTable { table, if_not_exists: *if_not_exists })?)
        }
        Statement::Drop(name, if_exists) => {
            Outcome::Done(execute_now(engine, branch, &Query::DropTable { name, if_exists: *if_exists })?)
        }
    })
}

/// Column names of a `SELECT`'s rows.
pub(crate) fn headers(s: &SelectAst) -> Vec<String> {
    match &s.projection {
        Projection::Tables(t) => t.iter().flat_map(|t| t.columns.iter().map(|c| c.name.to_owned())).collect(),
        Projection::Exprs(e) => e
            .iter()
            .map(|e| match e {
                Expr::Column(c) => c.name.to_owned(),
                _ => "count".to_owned(),
            })
            .collect(),
    }
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
        let p = [json!("Ada"), json!(36), json!(2)];
        let s = parse_with("select * from people where name = ? and age >= ? limit ?", &p, &lookup).unwrap();
        let Statement::Select(s) = s else { panic!() };
        assert_eq!(s.limit, Some(2));
        // A parameter is a value, never SQL.
        let p = [json!("x' OR '1'='1")];
        let Statement::Select(s) = parse_with("select * from people where name = ?", &p, &lookup).unwrap() else {
            panic!()
        };
        assert!(format!("{:?}", s.filter).contains("x' OR '1'='1"));
        assert!(parse_with("select * from people where name = ?", &[], &lookup).is_err(), "missing");
        assert!(parse_with("select * from people", &[json!(1)], &lookup).is_err(), "unused");
        let i =
            parse_with("INSERT INTO people (id, name) VALUES (?, ?), (2, 'Bob')", &[json!(1), json!("Ada")], &lookup)
                .unwrap();
        let Statement::Insert(i) = i else { panic!() };
        assert_eq!(i.rows[0], vec![Value::I64(1), Value::Text("Ada".into()), Value::Null]);
        assert!(parse("INSERT INTO people (id) VALUES (1)", &lookup).is_err(), "name needs a value");
        let c = parse(
            "CREATE TABLE IF NOT EXISTS posts (id BIGINT PRIMARY KEY AUTOINCREMENT, slug VARCHAR(80) NOT NULL UNIQUE, views INT INDEX, body TEXT)",
            &lookup,
        )
        .unwrap();
        let Statement::Create(schema, true) = c else { panic!() };
        assert_eq!((schema["pk"].as_u64(), schema["columns"][1]["unique"].as_bool()), (Some(0), Some(true)));
        assert_eq!(schema["columns"][3]["nullable"], true);
        let cache = memoire_orm::nuage::wire::SchemaCache::default();
        assert_eq!(cache.schema(&schema).unwrap().columns.len(), 4);
        assert!(
            matches!(parse("drop table if exists posts", &lookup).unwrap(), Statement::Drop(t, true) if t == "posts")
        );
        for bad in [
            "create table t (a int)",
            "create table t (a text primary key autoincrement)",
            "truncate people",
            "select * from nope",
            "select * from people where age = 'x'",
            "select * from people limit",
        ] {
            assert!(parse(bad, &lookup).is_err(), "{bad}");
        }
    }
}
