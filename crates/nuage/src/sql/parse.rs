//! The SQL parser: tokens to [`Stmt`]s.

use memoire_orm::{ColumnType, Result, Value};

use super::ast::*;
use super::lex::{Tok, syntax, tokenize};

/// Words that end an expression or a list: never taken as an alias.
const RESERVED: [&str; 46] = [
    "select",
    "from",
    "where",
    "group",
    "having",
    "order",
    "limit",
    "offset",
    "union",
    "intersect",
    "except",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "outer",
    "natural",
    "on",
    "using",
    "as",
    "and",
    "or",
    "not",
    "is",
    "in",
    "between",
    "like",
    "ilike",
    "returning",
    "set",
    "values",
    "when",
    "then",
    "else",
    "end",
    "case",
    "asc",
    "desc",
    "nulls",
    "by",
    "into",
    "fetch",
    "window",
    "glob",
];

pub(crate) struct Parser {
    toks: Vec<Tok>,
    at: usize,
    /// The next positional `?`.
    next_param: usize,
    /// The highest parameter used, plus one.
    pub params_used: usize,
}

/// The statements of `src`, separated by `;`.
pub(crate) fn parse(src: &str) -> Result<(Vec<Stmt>, usize)> {
    let mut p = Parser { toks: tokenize(src)?, at: 0, next_param: 0, params_used: 0 };
    let mut out = Vec::new();
    loop {
        while p.sym(";") {}
        if p.at >= p.toks.len() {
            break;
        }
        out.push(p.statement()?);
        if p.at < p.toks.len() && !p.sym(";") {
            return Err(syntax(format!("unexpected {} after the statement", p.describe())));
        }
    }
    if out.is_empty() {
        return Err(syntax("no statement"));
    }
    Ok((out, p.params_used))
}

/// One expression of SQL (a column's `DEFAULT`).
pub(crate) fn parse_expr(src: &str) -> Result<Expr> {
    let mut p = Parser { toks: tokenize(src)?, at: 0, next_param: 0, params_used: 0 };
    let e = p.expr()?;
    if p.at < p.toks.len() {
        return Err(syntax(format!("unexpected {}", p.describe())));
    }
    Ok(e)
}

/// A type name of SQL, as the wire protocol names it, and whether it means
/// an auto-increment (`SERIAL`).
pub(crate) fn type_of(word: &str) -> Option<(&'static str, bool)> {
    Some(match word.to_ascii_lowercase().as_str() {
        "bool" | "boolean" => ("bool", false),
        "smallint" | "int2" | "tinyint" => ("smallint", false),
        "int" | "integer" | "int4" | "mediumint" => ("int", false),
        "bigint" | "int8" => ("bigint", false),
        "serial" | "serial4" => ("int", true),
        "bigserial" | "serial8" => ("bigint", true),
        "real" | "float4" => ("real", false),
        "double" | "float" | "float8" | "numeric" | "decimal" | "money" => ("double", false),
        "text" | "varchar" | "char" | "string" | "clob" | "character" | "nvarchar" | "nchar" | "tinytext"
        | "mediumtext" | "longtext" | "citext" => ("text", false),
        "bytes" | "blob" | "bytea" | "binary" | "varbinary" | "longblob" => ("bytes", false),
        "uuid" => ("uuid", false),
        "timestamp" | "datetime" => ("timestamp", false),
        "timestamptz" => ("timestamptz", false),
        "date" => ("date", false),
        "time" => ("time", false),
        "json" | "jsonb" => ("json", false),
        _ => return None,
    })
}

pub(crate) fn column_type(kind: &str) -> ColumnType {
    match kind {
        "bool" => ColumnType::Bool,
        "smallint" => ColumnType::SmallInt,
        "int" => ColumnType::Int,
        "bigint" => ColumnType::BigInt,
        "real" => ColumnType::Real,
        "double" => ColumnType::Double,
        "bytes" => ColumnType::Bytes,
        "uuid" => ColumnType::Uuid,
        "timestamp" => ColumnType::Timestamp,
        "timestamptz" => ColumnType::TimestampTz,
        "date" => ColumnType::Date,
        "time" => ColumnType::Time,
        "json" => ColumnType::Json,
        _ => ColumnType::Text,
    }
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }

    fn peek_word(&self, n: usize) -> Option<String> {
        match self.toks.get(self.at + n) {
            Some(Tok::Word(w)) => Some(w.to_ascii_lowercase()),
            _ => None,
        }
    }

    fn describe(&self) -> String {
        match self.peek() {
            None => "the end".into(),
            Some(Tok::Word(w) | Tok::Quoted(w)) => format!("`{w}`"),
            Some(Tok::Str(s)) => format!("'{s}'"),
            Some(Tok::Int(n) | Tok::Float(n)) => n.clone(),
            Some(Tok::Param(_)) => "a parameter".into(),
            Some(Tok::Sym(s)) => format!("`{s}`"),
        }
    }

    fn keyword(&mut self, kw: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(kw)) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn keywords(&mut self, kws: &[&str]) -> bool {
        let ok = kws
            .iter()
            .enumerate()
            .all(|(i, k)| matches!(self.toks.get(self.at + i), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(k)));
        if ok {
            self.at += kws.len();
        }
        ok
    }

    fn expect(&mut self, kw: &str) -> Result<()> {
        if self.keyword(kw) {
            Ok(())
        } else {
            Err(syntax(format!("expected {} instead of {}", kw.to_ascii_uppercase(), self.describe())))
        }
    }

    fn sym(&mut self, s: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Sym(x)) if *x == s) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect_sym(&mut self, s: &str) -> Result<()> {
        if self.sym(s) { Ok(()) } else { Err(syntax(format!("expected `{s}` instead of {}", self.describe()))) }
    }

    fn name(&mut self) -> Result<String> {
        match self.peek().cloned() {
            Some(Tok::Quoted(w)) => {
                self.at += 1;
                Ok(w)
            }
            Some(Tok::Word(w)) => {
                self.at += 1;
                Ok(w)
            }
            _ => Err(syntax(format!("expected a name instead of {}", self.describe()))),
        }
    }

    /// A table name, `schema.table` allowed (the schema is ignored).
    fn table_name(&mut self) -> Result<String> {
        let mut n = self.name()?;
        if self.sym(".") {
            n = self.name()?;
        }
        Ok(n)
    }

    /// `[AS] alias`, when there is one.
    fn alias(&mut self) -> Result<Option<String>> {
        if self.keyword("as") {
            return self.name().map(Some);
        }
        match self.peek() {
            Some(Tok::Quoted(_)) => self.name().map(Some),
            Some(Tok::Word(w)) if !RESERVED.contains(&w.to_ascii_lowercase().as_str()) => self.name().map(Some),
            _ => Ok(None),
        }
    }

    fn names_in_parens(&mut self) -> Result<Vec<String>> {
        self.expect_sym("(")?;
        let mut out = vec![self.name()?];
        let _ = self.keyword("asc") || self.keyword("desc");
        while self.sym(",") {
            out.push(self.name()?);
            let _ = self.keyword("asc") || self.keyword("desc");
        }
        self.expect_sym(")")?;
        Ok(out)
    }

    fn param(&mut self, n: Option<usize>) -> Expr {
        let i = n.unwrap_or_else(|| {
            let i = self.next_param;
            self.next_param += 1;
            i
        });
        self.params_used = self.params_used.max(i + 1);
        Expr::Param(i)
    }

    // -----------------------------------------------------------------------
    // Statements
    // -----------------------------------------------------------------------

    fn statement(&mut self) -> Result<Stmt> {
        let w = self.peek_word(0).unwrap_or_default();
        match w.as_str() {
            "select" | "with" | "values" => Ok(Stmt::Select(self.select()?)),
            "insert" | "replace" => self.insert(),
            "update" => self.update(),
            "delete" => self.delete(),
            "create" => self.create(),
            "drop" => self.drop(),
            "alter" => self.alter(),
            "begin" | "start" => {
                self.at += 1;
                let _ = self.keyword("transaction") || self.keyword("work");
                Ok(Stmt::Transaction("begin"))
            }
            "commit" | "end" => {
                self.at += 1;
                let _ = self.keyword("transaction") || self.keyword("work");
                Ok(Stmt::Transaction("commit"))
            }
            "rollback" => {
                self.at += 1;
                let _ = self.keyword("transaction") || self.keyword("work");
                Ok(Stmt::Transaction("rollback"))
            }
            "explain" => {
                self.at += 1;
                self.keywords(&["query", "plan"]);
                Ok(Stmt::Explain(Box::new(self.statement()?)))
            }
            "" if self.sym("(") => {
                self.at -= 1;
                Ok(Stmt::Select(self.select()?))
            }
            _ => Err(syntax(format!(
                "a statement starts with SELECT, INSERT, UPDATE, DELETE, CREATE, DROP or ALTER, not {}",
                self.describe()
            ))),
        }
    }

    pub(crate) fn select(&mut self) -> Result<Select> {
        if self.keyword("with") {
            return Err(syntax("WITH (common table expressions) is not supported: use a subquery in FROM"));
        }
        let first = self.core()?;
        let mut rest = Vec::new();
        loop {
            let op = if self.keywords(&["union", "all"]) {
                SetOp::UnionAll
            } else if self.keyword("union") {
                self.keyword("distinct");
                SetOp::Union
            } else if self.keyword("intersect") {
                SetOp::Intersect
            } else if self.keyword("except") {
                SetOp::Except
            } else {
                break;
            };
            rest.push((op, self.core()?));
        }
        let mut order = Vec::new();
        if self.keywords(&["order", "by"]) {
            loop {
                let expr = self.expr()?;
                let desc = if self.keyword("desc") {
                    true
                } else {
                    self.keyword("asc");
                    false
                };
                let nulls_first = if self.keywords(&["nulls", "first"]) {
                    Some(true)
                } else if self.keywords(&["nulls", "last"]) {
                    Some(false)
                } else {
                    None
                };
                order.push(OrderItem { expr, desc, nulls_first });
                if !self.sym(",") {
                    break;
                }
            }
        }
        let (mut limit, mut offset) = (None, None);
        if self.keyword("limit") {
            let a = self.expr()?;
            if self.sym(",") {
                offset = Some(a);
                limit = Some(self.expr()?);
            } else {
                limit = Some(a);
            }
        }
        if self.keyword("offset") {
            offset = Some(self.expr()?);
            let _ = self.keyword("rows") || self.keyword("row");
        }
        if self.keyword("fetch") {
            let _ = self.keyword("first") || self.keyword("next");
            limit = Some(self.expr()?);
            let _ = self.keyword("rows") || self.keyword("row");
            self.expect("only")?;
        }
        Ok(Select { first, rest, order, limit, offset })
    }

    fn core(&mut self) -> Result<Core> {
        if self.sym("(") {
            // `(SELECT ...) UNION (SELECT ...)`: the parenthesized core.
            let inner = self.select()?;
            self.expect_sym(")")?;
            if !inner.rest.is_empty() || !inner.order.is_empty() || inner.limit.is_some() {
                return Err(syntax("a parenthesized SELECT in a UNION cannot have its own ORDER BY or LIMIT"));
            }
            return Ok(inner.first);
        }
        if self.keyword("values") {
            // `VALUES (1, 'a'), (2, 'b')`: a SELECT of literal rows.
            let rows = self.value_rows()?;
            let mut core = Core {
                distinct: false,
                items: Vec::new(),
                from: None,
                joins: Vec::new(),
                filter: None,
                group_by: Vec::new(),
                having: None,
            };
            let first = rows.first().cloned().unwrap_or_default();
            core.items = first
                .into_iter()
                .enumerate()
                .map(|(i, e)| SelItem::Expr(e, Some(format!("column{}", i + 1))))
                .collect();
            if rows.len() > 1 {
                return Err(syntax("VALUES with several rows: use INSERT ... VALUES, or UNION ALL of SELECTs"));
            }
            return Ok(core);
        }
        self.expect("select")?;
        let distinct = if self.keyword("distinct") {
            true
        } else {
            self.keyword("all");
            false
        };
        let mut items = Vec::new();
        loop {
            items.push(self.sel_item()?);
            if !self.sym(",") {
                break;
            }
        }
        let mut from = None;
        let mut joins = Vec::new();
        if self.keyword("from") {
            from = Some(self.table_ref()?);
            loop {
                if self.sym(",") {
                    joins.push(Join { kind: JoinKind::Cross, table: self.table_ref()?, on: None });
                    continue;
                }
                let kind = if self.keyword("join") || self.keywords(&["inner", "join"]) {
                    JoinKind::Inner
                } else if self.keywords(&["left", "join"]) || self.keywords(&["left", "outer", "join"]) {
                    JoinKind::Left
                } else if self.keywords(&["cross", "join"]) {
                    JoinKind::Cross
                } else if self.keyword("right") || self.keyword("full") || self.keyword("natural") {
                    return Err(syntax("RIGHT, FULL and NATURAL joins are not supported: write a LEFT JOIN"));
                } else {
                    break;
                };
                let table = self.table_ref()?;
                let on = if kind == JoinKind::Cross {
                    None
                } else if self.keyword("on") {
                    Some(self.expr()?)
                } else if self.keyword("using") {
                    let cols = self.names_in_parens()?;
                    let left = match &from {
                        Some(TableRef { alias: Some(a), .. }) => a.clone(),
                        Some(TableRef { source: Source::Table(t), .. }) => t.clone(),
                        _ => return Err(syntax("USING needs a named table on the left")),
                    };
                    let right = match &table {
                        TableRef { alias: Some(a), .. } => a.clone(),
                        TableRef { source: Source::Table(t), .. } => t.clone(),
                        _ => return Err(syntax("USING needs a named table")),
                    };
                    let mut on: Option<Expr> = None;
                    for c in cols {
                        let eq = Expr::Bin(
                            Op::Eq,
                            Box::new(Expr::Col { table: Some(left.clone()), name: c.clone() }),
                            Box::new(Expr::Col { table: Some(right.clone()), name: c }),
                        );
                        on = Some(match on {
                            Some(o) => Expr::Bin(Op::And, Box::new(o), Box::new(eq)),
                            None => eq,
                        });
                    }
                    on
                } else {
                    return Err(syntax("expected ON or USING after the joined table"));
                };
                joins.push(Join { kind, table, on });
            }
        }
        let filter = if self.keyword("where") { Some(self.expr()?) } else { None };
        let mut group_by = Vec::new();
        if self.keywords(&["group", "by"]) {
            loop {
                group_by.push(self.expr()?);
                if !self.sym(",") {
                    break;
                }
            }
        }
        let having = if self.keyword("having") { Some(self.expr()?) } else { None };
        Ok(Core { distinct, items, from, joins, filter, group_by, having })
    }

    fn sel_item(&mut self) -> Result<SelItem> {
        if self.sym("*") {
            return Ok(SelItem::All);
        }
        if matches!(self.peek(), Some(Tok::Word(_) | Tok::Quoted(_)))
            && self.toks.get(self.at + 1) == Some(&Tok::Sym("."))
            && self.toks.get(self.at + 2) == Some(&Tok::Sym("*"))
        {
            let t = self.name()?;
            self.at += 2;
            return Ok(SelItem::TableAll(t));
        }
        let e = self.expr()?;
        Ok(SelItem::Expr(e, self.alias()?))
    }

    fn table_ref(&mut self) -> Result<TableRef> {
        if self.sym("(") {
            let sub = self.select()?;
            self.expect_sym(")")?;
            let alias = self.alias()?;
            return Ok(TableRef { source: Source::Select(Box::new(sub)), alias });
        }
        let name = self.table_name()?;
        Ok(TableRef { source: Source::Table(name), alias: self.alias()? })
    }

    fn value_rows(&mut self) -> Result<Vec<Vec<Expr>>> {
        let mut rows = Vec::new();
        loop {
            self.expect_sym("(")?;
            let mut row = Vec::new();
            if !self.sym(")") {
                loop {
                    row.push(self.expr()?);
                    if !self.sym(",") {
                        break;
                    }
                }
                self.expect_sym(")")?;
            }
            rows.push(row);
            if !self.sym(",") {
                break;
            }
        }
        Ok(rows)
    }

    fn returning(&mut self) -> Result<Option<Vec<SelItem>>> {
        if !self.keyword("returning") {
            return Ok(None);
        }
        let mut items = vec![self.sel_item()?];
        while self.sym(",") {
            items.push(self.sel_item()?);
        }
        Ok(Some(items))
    }

    fn sets(&mut self) -> Result<Vec<(String, Expr)>> {
        let mut sets = Vec::new();
        loop {
            let mut col = self.name()?;
            if self.sym(".") {
                col = self.name()?;
            }
            if !(self.sym("=") || self.sym("==")) {
                return Err(syntax(format!("expected `=` after `{col}`")));
            }
            sets.push((col, self.expr()?));
            if !self.sym(",") {
                break;
            }
        }
        Ok(sets)
    }

    fn insert(&mut self) -> Result<Stmt> {
        let mut replace = false;
        let mut on_conflict = None;
        if self.keyword("replace") {
            replace = true;
        } else {
            self.expect("insert")?;
            if self.keyword("or") {
                if self.keyword("replace") {
                    replace = true;
                } else if self.keyword("ignore") {
                    on_conflict = Some((Vec::new(), Conflict::Nothing));
                } else {
                    return Err(syntax("INSERT OR REPLACE or INSERT OR IGNORE"));
                }
            } else if self.keyword("ignore") {
                on_conflict = Some((Vec::new(), Conflict::Nothing));
            }
        }
        self.expect("into")?;
        let table = self.table_name()?;
        self.alias()?;
        let columns = if self.toks.get(self.at) == Some(&Tok::Sym("("))
            && !matches!(self.peek_word(1).as_deref(), Some("select"))
        {
            Some(self.names_in_parens()?)
        } else {
            None
        };
        let source = if self.keyword("values") {
            InsertSource::Values(self.value_rows()?)
        } else if self.keywords(&["default", "values"]) {
            InsertSource::Defaults
        } else if matches!(self.peek_word(0).as_deref(), Some("select"))
            || self.toks.get(self.at) == Some(&Tok::Sym("("))
        {
            InsertSource::Select(Box::new(self.select()?))
        } else {
            return Err(syntax("expected VALUES, SELECT or DEFAULT VALUES"));
        };
        if self.keywords(&["on", "conflict"]) {
            let target =
                if self.toks.get(self.at) == Some(&Tok::Sym("(")) { self.names_in_parens()? } else { Vec::new() };
            self.expect("do")?;
            let action = if self.keyword("nothing") {
                Conflict::Nothing
            } else {
                self.expect("update")?;
                self.expect("set")?;
                let sets = self.sets()?;
                let filter = if self.keyword("where") { Some(self.expr()?) } else { None };
                Conflict::Update(sets, filter)
            };
            on_conflict = Some((target, action));
        } else if self.keywords(&["on", "duplicate", "key", "update"]) {
            on_conflict = Some((Vec::new(), Conflict::Update(self.sets()?, None)));
        }
        let returning = self.returning()?;
        Ok(Stmt::Insert { table, columns, source, on_conflict, replace, returning })
    }

    fn update(&mut self) -> Result<Stmt> {
        self.expect("update")?;
        self.keyword("or");
        let table = self.table_name()?;
        let alias = self.alias()?;
        self.expect("set")?;
        let sets = self.sets()?;
        if self.keyword("from") {
            return Err(syntax("UPDATE ... FROM is not supported: use a subquery in SET or WHERE"));
        }
        let filter = if self.keyword("where") { Some(self.expr()?) } else { None };
        let returning = self.returning()?;
        Ok(Stmt::Update { table, alias, sets, filter, returning })
    }

    fn delete(&mut self) -> Result<Stmt> {
        self.expect("delete")?;
        self.expect("from")?;
        let table = self.table_name()?;
        let alias = self.alias()?;
        let filter = if self.keyword("where") { Some(self.expr()?) } else { None };
        let returning = self.returning()?;
        Ok(Stmt::Delete { table, alias, filter, returning })
    }

    fn column_def(&mut self) -> Result<ColDef> {
        let name = self.name()?;
        check_name(&name)?;
        let word = self.name()?;
        let (mut kind, mut auto_increment) =
            type_of(&word).ok_or_else(|| syntax(format!("unknown type `{word}` for column `{name}`")))?;
        // `DOUBLE PRECISION`, `VARCHAR(255)`, `TIMESTAMP WITH TIME ZONE`, `UNSIGNED`.
        let _ = self.keyword("precision") || self.keyword("varying") || self.keyword("unsigned");
        if self.sym("(") {
            while !self.sym(")") {
                if self.peek().is_none() {
                    return Err(syntax("expected `)`"));
                }
                self.at += 1;
            }
        }
        if self.keywords(&["with", "time", "zone"]) {
            if kind == "timestamp" {
                kind = "timestamptz";
            }
        } else {
            self.keywords(&["without", "time", "zone"]);
        }
        let mut c = ColDef {
            name,
            kind,
            primary_key: false,
            auto_increment: false,
            not_null: false,
            unique: false,
            indexed: false,
            default: None,
            references: None,
        };
        loop {
            if self.keyword("constraint") {
                self.name()?;
            } else if self.keywords(&["primary", "key"]) {
                c.primary_key = true;
                let _ = self.keyword("asc") || self.keyword("desc");
            } else if self.keyword("autoincrement")
                || self.keyword("auto_increment")
                || self.keyword("identity")
                || self.keywords(&["generated", "always", "as", "identity"])
                || self.keywords(&["generated", "by", "default", "as", "identity"])
            {
                auto_increment = true;
            } else if self.keywords(&["not", "null"]) {
                c.not_null = true;
            } else if self.keyword("null") {
            } else if self.keyword("unique") {
                c.unique = true;
            } else if self.keyword("index") || self.keyword("indexed") || self.keyword("key") {
                c.indexed = true;
            } else if self.keyword("default") {
                let e = if self.sym("(") {
                    let e = self.expr()?;
                    self.expect_sym(")")?;
                    e
                } else {
                    self.unary()?
                };
                c.default = Some(default_sql(&e)?);
            } else if self.keyword("references") {
                let t = self.table_name()?;
                let col = if self.toks.get(self.at) == Some(&Tok::Sym("(")) {
                    self.names_in_parens()?.remove(0)
                } else {
                    "id".to_owned()
                };
                while self.keyword("on") {
                    let _ = self.keyword("delete") || self.keyword("update");
                    if !(self.keyword("cascade") || self.keyword("restrict")) {
                        if self.keywords(&["set", "null"])
                            || self.keywords(&["set", "default"])
                            || self.keywords(&["no", "action"])
                        {
                        } else {
                            return Err(syntax(
                                "ON DELETE/UPDATE CASCADE, RESTRICT, SET NULL, SET DEFAULT or NO ACTION",
                            ));
                        }
                    }
                }
                c.references = Some((t, col));
            } else if self.keyword("check") {
                self.skip_parens()?;
            } else if self.keyword("collate") {
                self.name()?;
            } else {
                break;
            }
        }
        c.auto_increment = auto_increment;
        Ok(c)
    }

    fn skip_parens(&mut self) -> Result<()> {
        self.expect_sym("(")?;
        let mut depth = 1;
        while depth > 0 {
            match self.peek() {
                None => return Err(syntax("expected `)`")),
                Some(Tok::Sym("(")) => depth += 1,
                Some(Tok::Sym(")")) => depth -= 1,
                _ => {}
            }
            self.at += 1;
        }
        Ok(())
    }

    fn create(&mut self) -> Result<Stmt> {
        self.expect("create")?;
        let unique = self.keyword("unique");
        if self.keyword("index") {
            let if_not_exists = self.keywords(&["if", "not", "exists"]);
            let name = self.name()?;
            self.expect("on")?;
            let table = self.table_name()?;
            let columns = self.names_in_parens()?;
            return Ok(Stmt::CreateIndex { name, table, columns, unique, if_not_exists });
        }
        if unique {
            return Err(syntax("expected INDEX after CREATE UNIQUE"));
        }
        let _ = self.keyword("temporary") || self.keyword("temp");
        self.expect("table")?;
        let if_not_exists = self.keywords(&["if", "not", "exists"]);
        let name = self.table_name()?;
        check_name(&name)?;
        if self.keyword("as") {
            let sel = self.select()?;
            return Ok(Stmt::CreateTable {
                name,
                if_not_exists,
                columns: Vec::new(),
                indexes: Vec::new(),
                as_select: Some(Box::new(sel)),
            });
        }
        self.expect_sym("(")?;
        let mut columns: Vec<ColDef> = Vec::new();
        let mut indexes = Vec::new();
        loop {
            let _ = self.keyword("constraint").then(|| self.name()).transpose()?;
            if self.keywords(&["primary", "key"]) {
                let cols = self.names_in_parens()?;
                if cols.len() != 1 {
                    return Err(syntax(
                        "a primary key of several columns is not supported: use a key column and a UNIQUE (a, b)",
                    ));
                }
                let c = columns
                    .iter_mut()
                    .find(|c| c.name.eq_ignore_ascii_case(&cols[0]))
                    .ok_or_else(|| syntax(format!("no column `{}`", cols[0])))?;
                c.primary_key = true;
            } else if self.keyword("unique") {
                let _ = self.keyword("key") || self.keyword("index");
                if !matches!(self.peek(), Some(Tok::Sym("("))) {
                    self.name()?;
                }
                indexes.push((self.names_in_parens()?, true));
            } else if self.keyword("index") || self.keyword("key") {
                if !matches!(self.peek(), Some(Tok::Sym("("))) {
                    self.name()?;
                }
                indexes.push((self.names_in_parens()?, false));
            } else if self.keywords(&["foreign", "key"]) {
                let cols = self.names_in_parens()?;
                self.expect("references")?;
                let t = self.table_name()?;
                let target = if matches!(self.peek(), Some(Tok::Sym("("))) {
                    self.names_in_parens()?.remove(0)
                } else {
                    "id".into()
                };
                while self.keyword("on") {
                    let _ = self.keyword("delete") || self.keyword("update");
                    let _ = self.keyword("cascade")
                        || self.keyword("restrict")
                        || self.keywords(&["set", "null"])
                        || self.keywords(&["no", "action"]);
                }
                if let Some(c) = columns.iter_mut().find(|c| c.name.eq_ignore_ascii_case(&cols[0])) {
                    c.references = Some((t, target));
                }
            } else if self.keyword("check") {
                self.skip_parens()?;
            } else {
                columns.push(self.column_def()?);
            }
            if !self.sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        // Table options of other databases (`ENGINE=...`, `WITHOUT ROWID`, `STRICT`).
        while self.peek().is_some() && self.peek() != Some(&Tok::Sym(";")) {
            self.at += 1;
        }
        Ok(Stmt::CreateTable { name, if_not_exists, columns, indexes, as_select: None })
    }

    fn drop(&mut self) -> Result<Stmt> {
        self.expect("drop")?;
        if self.keyword("index") {
            let if_exists = self.keywords(&["if", "exists"]);
            let name = self.name()?;
            if self.keyword("on") {
                self.table_name()?;
            }
            return Ok(Stmt::DropIndex { name, if_exists });
        }
        self.expect("table")?;
        let if_exists = self.keywords(&["if", "exists"]);
        let mut names = vec![self.table_name()?];
        while self.sym(",") {
            names.push(self.table_name()?);
        }
        let _ = self.keyword("cascade") || self.keyword("restrict");
        Ok(Stmt::DropTable { names, if_exists })
    }

    fn alter(&mut self) -> Result<Stmt> {
        self.expect("alter")?;
        self.expect("table")?;
        let table = self.table_name()?;
        if self.keyword("add") {
            self.keyword("column");
            self.keywords(&["if", "not", "exists"]);
            let column = self.column_def()?;
            if column.primary_key || column.auto_increment {
                return Err(syntax("a new column cannot be the primary key"));
            }
            return Ok(Stmt::AddColumn { table, column });
        }
        Err(syntax(
            "ALTER TABLE ... ADD COLUMN is supported; to rename or drop a column, create a new table (CREATE TABLE ... AS SELECT) and drop the old one",
        ))
    }

    // -----------------------------------------------------------------------
    // Expressions
    // -----------------------------------------------------------------------

    pub(crate) fn expr(&mut self) -> Result<Expr> {
        self.or()
    }

    fn or(&mut self) -> Result<Expr> {
        let mut e = self.and()?;
        while self.keyword("or") {
            e = Expr::Bin(Op::Or, Box::new(e), Box::new(self.and()?));
        }
        Ok(e)
    }

    fn and(&mut self) -> Result<Expr> {
        let mut e = self.not()?;
        while self.keyword("and") {
            e = Expr::Bin(Op::And, Box::new(e), Box::new(self.not()?));
        }
        Ok(e)
    }

    fn not(&mut self) -> Result<Expr> {
        if self.keyword("not") {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Expr> {
        let left = self.concat()?;
        // `IS [NOT] NULL`, `IS [NOT] TRUE`, `IS [NOT] DISTINCT FROM`.
        if self.keyword("is") {
            let negated = self.keyword("not");
            if self.keyword("null") {
                return Ok(Expr::IsNull(Box::new(left), negated));
            }
            if self.keyword("true") || self.keyword("false") {
                let want = matches!(&self.toks[self.at - 1], Tok::Word(w) if w.eq_ignore_ascii_case("true"));
                let test = Expr::Func {
                    name: "coalesce".into(),
                    args: vec![
                        Expr::Bin(Op::Eq, Box::new(left), Box::new(Expr::Lit(Value::Bool(want)))),
                        Expr::Lit(Value::Bool(false)),
                    ],
                    distinct: false,
                    star: false,
                };
                return Ok(if negated { Expr::Not(Box::new(test)) } else { test });
            }
            if self.keywords(&["distinct", "from"]) {
                let right = self.concat()?;
                let same = Expr::Func { name: "_same".into(), args: vec![left, right], distinct: false, star: false };
                return Ok(if negated { same } else { Expr::Not(Box::new(same)) });
            }
            // `a IS b`: SQLite's null-safe equality.
            let right = self.concat()?;
            let same = Expr::Func { name: "_same".into(), args: vec![left, right], distinct: false, star: false };
            return Ok(if negated { Expr::Not(Box::new(same)) } else { same });
        }
        if self.keyword("isnull") {
            return Ok(Expr::IsNull(Box::new(left), false));
        }
        if self.keyword("notnull") {
            return Ok(Expr::IsNull(Box::new(left), true));
        }
        let negated = self.keyword("not");
        if self.keyword("in") {
            self.expect_sym("(")?;
            if matches!(self.peek_word(0).as_deref(), Some("select" | "with")) {
                let sub = self.select()?;
                self.expect_sym(")")?;
                return Ok(Expr::InSelect(Box::new(left), Box::new(sub), negated));
            }
            let mut list = Vec::new();
            if !self.sym(")") {
                loop {
                    list.push(self.expr()?);
                    if !self.sym(",") {
                        break;
                    }
                }
                self.expect_sym(")")?;
            }
            return Ok(Expr::In(Box::new(left), list, negated));
        }
        if self.keyword("between") {
            let low = self.concat()?;
            self.expect("and")?;
            let high = self.concat()?;
            return Ok(Expr::Between(Box::new(left), Box::new(low), Box::new(high), negated));
        }
        let like = if self.keyword("like") {
            Some(false)
        } else if self.keyword("ilike") {
            Some(true)
        } else {
            None
        };
        if let Some(ci) = like {
            let pattern = self.concat()?;
            let escape = if self.keyword("escape") { Some(Box::new(self.concat()?)) } else { None };
            return Ok(Expr::Like { expr: Box::new(left), pattern: Box::new(pattern), ci, negated, escape });
        }
        if negated {
            return Err(syntax("expected IN, BETWEEN, LIKE or ILIKE after NOT"));
        }
        let op = match self.peek() {
            Some(Tok::Sym("=" | "==")) => Op::Eq,
            Some(Tok::Sym("!=" | "<>")) => Op::Ne,
            Some(Tok::Sym("<")) => Op::Lt,
            Some(Tok::Sym("<=")) => Op::Le,
            Some(Tok::Sym(">")) => Op::Gt,
            Some(Tok::Sym(">=")) => Op::Ge,
            _ => return Ok(left),
        };
        self.at += 1;
        // `x = ANY (...)` is not supported; plain comparisons only.
        let right = self.concat()?;
        Ok(Expr::Bin(op, Box::new(left), Box::new(right)))
    }

    fn concat(&mut self) -> Result<Expr> {
        let mut e = self.additive()?;
        while self.sym("||") {
            e = Expr::Bin(Op::Concat, Box::new(e), Box::new(self.additive()?));
        }
        Ok(e)
    }

    fn additive(&mut self) -> Result<Expr> {
        let mut e = self.term()?;
        loop {
            let op = if self.sym("+") {
                Op::Add
            } else if self.sym("-") {
                Op::Sub
            } else {
                return Ok(e);
            };
            e = Expr::Bin(op, Box::new(e), Box::new(self.term()?));
        }
    }

    fn term(&mut self) -> Result<Expr> {
        let mut e = self.unary()?;
        loop {
            let op = if self.sym("*") {
                Op::Mul
            } else if self.sym("/") {
                Op::Div
            } else if self.sym("%") {
                Op::Mod
            } else {
                return Ok(e);
            };
            e = Expr::Bin(op, Box::new(e), Box::new(self.unary()?));
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.sym("-") {
            return Ok(match self.unary()? {
                Expr::Lit(Value::I64(n)) => Expr::Lit(Value::I64(-n)),
                Expr::Lit(Value::F64(n)) => Expr::Lit(Value::F64(-n)),
                e => Expr::Neg(Box::new(e)),
            });
        }
        if self.sym("+") {
            return self.unary();
        }
        let mut e = self.primary()?;
        while self.sym("::") {
            let word = self.name()?;
            let (kind, _) = type_of(&word).ok_or_else(|| syntax(format!("unknown type `{word}`")))?;
            if self.sym("(") {
                while !self.sym(")") {
                    self.at += 1;
                }
            }
            e = Expr::Cast(Box::new(e), column_type(kind));
        }
        Ok(e)
    }

    fn primary(&mut self) -> Result<Expr> {
        let tok = self.peek().cloned().ok_or_else(|| syntax("expected an expression, found the end"))?;
        match tok {
            Tok::Str(s) => {
                self.at += 1;
                Ok(Expr::Lit(Value::Text(s)))
            }
            Tok::Int(n) => {
                self.at += 1;
                Ok(Expr::Lit(match n.parse::<i64>() {
                    Ok(i) => Value::I64(i),
                    Err(_) => Value::F64(n.parse().map_err(|_| syntax(format!("bad number {n}")))?),
                }))
            }
            Tok::Float(n) => {
                self.at += 1;
                Ok(Expr::Lit(Value::F64(n.parse().map_err(|_| syntax(format!("bad number {n}")))?)))
            }
            Tok::Param(n) => {
                self.at += 1;
                Ok(self.param(n))
            }
            Tok::Sym("(") => {
                self.at += 1;
                if matches!(self.peek_word(0).as_deref(), Some("select" | "with" | "values")) {
                    let sub = self.select()?;
                    self.expect_sym(")")?;
                    return Ok(Expr::Scalar(Box::new(sub)));
                }
                let e = self.expr()?;
                if self.sym(",") {
                    return Err(syntax("row values `(a, b)` are not supported"));
                }
                self.expect_sym(")")?;
                Ok(e)
            }
            Tok::Quoted(_) => self.column(),
            Tok::Word(w) => {
                let lw = w.to_ascii_lowercase();
                match lw.as_str() {
                    "null" => {
                        self.at += 1;
                        return Ok(Expr::Lit(Value::Null));
                    }
                    "true" => {
                        self.at += 1;
                        return Ok(Expr::Lit(Value::Bool(true)));
                    }
                    "false" => {
                        self.at += 1;
                        return Ok(Expr::Lit(Value::Bool(false)));
                    }
                    "current_timestamp" | "current_date" | "current_time" | "localtimestamp"
                        if self.toks.get(self.at + 1) != Some(&Tok::Sym("(")) =>
                    {
                        self.at += 1;
                        let name = if lw == "localtimestamp" { "current_timestamp".into() } else { lw };
                        return Ok(Expr::Func { name, args: Vec::new(), distinct: false, star: false });
                    }
                    "exists" => {
                        self.at += 1;
                        self.expect_sym("(")?;
                        let sub = self.select()?;
                        self.expect_sym(")")?;
                        return Ok(Expr::Exists(Box::new(sub), false));
                    }
                    "case" => {
                        self.at += 1;
                        return self.case();
                    }
                    "cast" if self.toks.get(self.at + 1) == Some(&Tok::Sym("(")) => {
                        self.at += 2;
                        let e = self.expr()?;
                        self.expect("as")?;
                        let word = self.name()?;
                        let (kind, _) = type_of(&word).ok_or_else(|| syntax(format!("unknown type `{word}`")))?;
                        while !self.sym(")") {
                            if self.peek().is_none() {
                                return Err(syntax("expected `)`"));
                            }
                            self.at += 1;
                        }
                        return Ok(Expr::Cast(Box::new(e), column_type(kind)));
                    }
                    _ => {}
                }
                if self.toks.get(self.at + 1) == Some(&Tok::Sym("(")) {
                    return self.call(lw);
                }
                self.column()
            }
            Tok::Sym(s) => Err(syntax(format!("unexpected `{s}`"))),
        }
    }

    fn column(&mut self) -> Result<Expr> {
        let first = self.name()?;
        if self.sym(".") {
            let name = self.name()?;
            return Ok(Expr::Col { table: Some(first), name });
        }
        Ok(Expr::Col { table: None, name: first })
    }

    fn call(&mut self, name: String) -> Result<Expr> {
        self.at += 2; // name (
        if name == "values" {
            // MySQL's `VALUES(col)` in ON DUPLICATE KEY UPDATE: the new value.
            let col = self.name()?;
            self.expect_sym(")")?;
            return Ok(Expr::Col { table: Some("excluded".into()), name: col });
        }
        if self.sym("*") {
            self.expect_sym(")")?;
            return Ok(Expr::Func { name, args: Vec::new(), distinct: false, star: true });
        }
        let distinct = self.keyword("distinct");
        let mut args = Vec::new();
        if !self.sym(")") {
            loop {
                args.push(self.expr()?);
                if !self.sym(",") {
                    break;
                }
            }
            // `string_agg(x, ',' ORDER BY y)`: the order is not kept.
            if self.keywords(&["order", "by"]) {
                return Err(syntax("ORDER BY inside an aggregate is not supported"));
            }
            self.expect_sym(")")?;
        }
        if self.keyword("filter") || self.keyword("over") {
            return Err(syntax("window functions and FILTER are not supported"));
        }
        // `substring(x from 2 for 3)` is written substr(x, 2, 3) here.
        Ok(Expr::Func { name, args, distinct, star: false })
    }

    fn case(&mut self) -> Result<Expr> {
        let operand =
            if matches!(self.peek_word(0).as_deref(), Some("when")) { None } else { Some(Box::new(self.expr()?)) };
        let mut whens = Vec::new();
        while self.keyword("when") {
            let w = self.expr()?;
            self.expect("then")?;
            whens.push((w, self.expr()?));
        }
        if whens.is_empty() {
            return Err(syntax("CASE needs at least one WHEN"));
        }
        let otherwise = if self.keyword("else") { Some(Box::new(self.expr()?)) } else { None };
        self.expect("end")?;
        Ok(Expr::Case { operand, whens, otherwise })
    }
}

/// Table and column names: letters, digits and `_`, not starting with a digit.
pub(crate) fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    if ok { Ok(()) } else { Err(syntax(format!("invalid name `{name}`: use letters, digits and `_`"))) }
}

/// A constant `DEFAULT`, written back as SQL.
fn default_sql(e: &Expr) -> Result<String> {
    Ok(match e {
        Expr::Lit(Value::Null) => "NULL".into(),
        Expr::Lit(Value::Bool(b)) => if *b { "TRUE" } else { "FALSE" }.into(),
        Expr::Lit(Value::I64(n)) => n.to_string(),
        Expr::Lit(Value::F64(n)) => format!("{n:?}"),
        Expr::Lit(Value::Text(s)) => format!("'{}'", s.replace('\'', "''")),
        Expr::Neg(inner) => format!("-{}", default_sql(inner)?),
        Expr::Func { name, args, .. }
            if args.is_empty()
                && matches!(
                    name.as_str(),
                    "current_timestamp"
                        | "current_date"
                        | "current_time"
                        | "now"
                        | "random"
                        | "gen_random_uuid"
                        | "uuid"
                ) =>
        {
            format!("{name}()")
        }
        _ => return Err(syntax("DEFAULT takes a constant, or now(), current_timestamp, gen_random_uuid()")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(src: &str) -> Stmt {
        let (mut s, _) = parse(src).unwrap();
        assert_eq!(s.len(), 1, "{src}");
        s.remove(0)
    }

    #[test]
    fn statements() {
        let s = one(
            "SELECT DISTINCT u.name AS who, count(*) n, sum(p.views * 2) FROM users u LEFT JOIN posts p ON p.user_id = u.id \
             WHERE u.age BETWEEN ? AND ?2 AND u.name ILIKE '%a%' GROUP BY u.name HAVING count(*) > 1 \
             ORDER BY n DESC NULLS LAST, 1 LIMIT 10 OFFSET 5",
        );
        let Stmt::Select(sel) = s else { panic!() };
        assert!(sel.first.distinct);
        assert_eq!(sel.first.items.len(), 3);
        assert_eq!(sel.first.joins[0].kind, JoinKind::Left);
        assert_eq!(sel.order[0].nulls_first, Some(false));
        assert!(sel.first.items.iter().any(|i| matches!(i, SelItem::Expr(e, Some(a)) if a == "n" && e.is_aggregate())));
        let (_, params) = parse("select * from t where a = ? and b = ? and c = ?1").unwrap();
        assert_eq!(params, 2);
        assert!(matches!(one("SELECT 1 UNION ALL SELECT 2 ORDER BY 1"), Stmt::Select(s) if s.rest.len() == 1));
        assert!(
            matches!(one("select * from (select a from t) as x"), Stmt::Select(s) if matches!(s.first.from.as_ref().unwrap().source, Source::Select(_)))
        );
        assert!(matches!(
            one(
                "INSERT INTO t (a, b) VALUES (1, 'x'), (2, NULL) ON CONFLICT (a) DO UPDATE SET b = excluded.b RETURNING *"
            ),
            Stmt::Insert { on_conflict: Some((_, Conflict::Update(..))), returning: Some(_), .. }
        ));
        assert!(matches!(
            one("INSERT OR IGNORE INTO t VALUES (1)"),
            Stmt::Insert { on_conflict: Some((_, Conflict::Nothing)), .. }
        ));
        assert!(matches!(one("REPLACE INTO t (a) VALUES (1)"), Stmt::Insert { replace: true, .. }));
        assert!(matches!(
            one("UPDATE t SET views = views + 1, title = upper(title) WHERE id IN (SELECT id FROM t2) RETURNING id"),
            Stmt::Update { .. }
        ));
        assert!(matches!(
            one("DELETE FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE u.id = t.uid)"),
            Stmt::Delete { .. }
        ));
        let Stmt::CreateTable { columns, indexes, .. } =
            one("CREATE TABLE IF NOT EXISTS posts (id SERIAL PRIMARY KEY, slug VARCHAR(80) NOT NULL UNIQUE, \
             at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP, views INT DEFAULT 0, \
             user_id BIGINT REFERENCES users(id) ON DELETE CASCADE, price DOUBLE PRECISION, UNIQUE (user_id, slug))")
        else {
            panic!()
        };
        assert!(columns[0].auto_increment && columns[0].primary_key);
        assert_eq!(columns[2].kind, "timestamptz");
        assert_eq!(columns[2].default.as_deref(), Some("current_timestamp()"));
        assert_eq!(columns[3].default.as_deref(), Some("0"));
        assert_eq!(columns[4].references, Some(("users".into(), "id".into())));
        assert_eq!(indexes, vec![(vec!["user_id".to_string(), "slug".to_string()], true)]);
        assert!(matches!(
            one("CREATE UNIQUE INDEX IF NOT EXISTS ix ON t (a, b DESC)"),
            Stmt::CreateIndex { unique: true, .. }
        ));
        assert!(matches!(one("ALTER TABLE t ADD COLUMN c TEXT DEFAULT 'x'"), Stmt::AddColumn { .. }));
        assert!(matches!(one("DROP TABLE IF EXISTS a, b"), Stmt::DropTable { names, .. } if names.len() == 2));
        let (all, _) = parse("BEGIN; INSERT INTO t VALUES (1); COMMIT;").unwrap();
        assert_eq!(all.len(), 3);
        for bad in [
            "SELEC 1",
            "select * from",
            "select (1, 2)",
            "select * from a right join b on 1",
            "insert into t",
            "create table t (a nope)",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn expressions() {
        let e = parse_expr("a + b * 2 - -c || 'x' = 'y' AND NOT d IS NULL OR e IN (1, 2)").unwrap();
        assert!(matches!(e, Expr::Bin(Op::Or, ..)));
        assert!(matches!(parse_expr("CASE WHEN a > 1 THEN 'big' ELSE 'small' END").unwrap(), Expr::Case { .. }));
        assert!(matches!(parse_expr("CAST(x AS INTEGER)").unwrap(), Expr::Cast(_, ColumnType::Int)));
        assert!(matches!(parse_expr("x::text").unwrap(), Expr::Cast(_, ColumnType::Text)));
        assert!(matches!(parse_expr("-5").unwrap(), Expr::Lit(Value::I64(-5))));
        assert!(matches!(parse_expr("count(DISTINCT x)").unwrap(), Expr::Func { distinct: true, .. }));
    }
}
