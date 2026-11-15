//! The SQL parser: tokens to [`Stmt`]s.

use memoire_orm::{ColumnType, Result, Value};

use super::ast::*;
use super::lex::{Tok, syntax, tokenize};
use super::value;

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

/// How many expressions and `SELECT`s may be open one inside the other
/// (parentheses, function calls, subqueries, `NOT NOT`, `- -`...). The parser
/// calls itself once per level, and a statement nested without end would take
/// the whole stack of its thread: past this, it is a syntax error. Statements
/// written by hand or by a query builder stay far under it; a long list
/// (`a OR b OR c ...`, `IN (...)`) is not nested and has no limit.
pub(crate) const MAX_NESTING: usize = 64;

/// Statements nested up to this are read on the thread that asks for them;
/// deeper ones, up to [`MAX_NESTING`], on a thread of their own with
/// [`DEEP_STACK`]. Unoptimized, the parser takes some 20 KiB of stack per level
/// (it goes down through every level of precedence), and statements are read
/// on threads of 2 MiB that hold other frames already.
const NESTING_HERE: usize = 16;

/// The stack of the thread that reads a deeply nested statement: several
/// times what the deepest one takes unoptimized. Only touched pages count.
const DEEP_STACK: usize = 32 << 20;

/// How tall an expression may be: the nodes from its top to its deepest
/// operand, a subquery counting for [`SELECT_HEIGHT`] plus its own tallest
/// expression. Evaluating, translating, copying and dropping an expression
/// take one call per node on the way down, on the thread that runs it.
pub(crate) const MAX_HEIGHT: usize = 200;

/// What a `SELECT` inside another counts for in the height of an expression:
/// running it takes more of the stack than a node does.
pub(crate) const SELECT_HEIGHT: usize = 16;

/// The numbers of the `SELECT`s (see [`Select::id`]).
static SELECTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(crate) struct Parser<'t> {
    toks: &'t [Tok],
    at: usize,
    /// The next positional `?`.
    next_param: usize,
    /// The highest parameter used, plus one.
    pub params_used: usize,
    /// Expressions and `SELECT`s open around what is being read.
    depth: usize,
    /// How many may be open here ([`NESTING_HERE`] or [`MAX_NESTING`]).
    nesting: usize,
    /// The statement nests deeper than `nesting`, and less than [`MAX_NESTING`] might do.
    deeper: bool,
    /// The height of the expression (or `SELECT`) read last.
    height: usize,
    /// The tallest expression read in the `SELECT` being read.
    tallest: usize,
}

fn too_deep() -> memoire_orm::Error {
    syntax(
        "the statement is nested too deeply (parentheses, subqueries, functions inside one another): write it in several statements",
    )
}

/// `what` read from `toks`, on this thread; a statement nested deeper than
/// [`NESTING_HERE`] is read again on a thread with a stack it fits in.
fn read<T: Send>(toks: &[Tok], what: fn(&mut Parser<'_>) -> Result<T>) -> Result<T> {
    let mut p = Parser::new(toks, NESTING_HERE);
    match what(&mut p) {
        Err(_) if p.deeper => {}
        done => return done,
    }
    std::thread::scope(|s| {
        let reader = std::thread::Builder::new()
            .name("nuage-sql-parse".into())
            .stack_size(DEEP_STACK)
            .spawn_scoped(s, || what(&mut Parser::new(toks, MAX_NESTING)));
        match reader {
            Ok(reader) => reader.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
            // No thread for it (the system is out of them): refused as too deep.
            Err(_) => Err(too_deep()),
        }
    })
}

/// The statements of `src`, separated by `;`.
pub(crate) fn parse(src: &str) -> Result<(Vec<Stmt>, usize)> {
    read(&tokenize(src)?, |p| {
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
    })
}

/// One expression of SQL (a column's `DEFAULT`).
pub(crate) fn parse_expr(src: &str) -> Result<Expr> {
    read(&tokenize(src)?, |p| {
        let e = p.expr()?;
        if p.at < p.toks.len() {
            return Err(syntax(format!("unexpected {}", p.describe())));
        }
        Ok(e)
    })
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

impl<'t> Parser<'t> {
    fn new(toks: &'t [Tok], nesting: usize) -> Parser<'t> {
        Parser { toks, at: 0, next_param: 0, params_used: 0, depth: 0, nesting, deeper: false, height: 0, tallest: 0 }
    }

    /// One more expression or `SELECT` open (closed by `self.depth -= 1`).
    fn open(&mut self) -> Result<()> {
        if self.depth >= self.nesting {
            self.deeper = self.nesting < MAX_NESTING;
            return Err(too_deep());
        }
        self.depth += 1;
        Ok(())
    }

    /// An expression was built over operands the tallest of which is `under` tall.
    fn built(&mut self, under: usize) -> Result<()> {
        self.height = under + 1;
        if self.height > MAX_HEIGHT { Err(too_deep()) } else { Ok(()) }
    }

    /// `(255)`, `(10, 2)` after the name of a type: read, and left out.
    fn type_modifier(&mut self) -> Result<()> {
        if self.sym("(") {
            while !self.sym(")") {
                if self.peek().is_none() {
                    return Err(syntax("expected `)`"));
                }
                self.at += 1;
            }
        }
        Ok(())
    }

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
            "select" | "with" | "values" => Ok(Stmt::Select(Box::new(self.select()?))),
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
                self.open()?;
                let inner = self.statement();
                self.depth -= 1;
                Ok(Stmt::Explain(Box::new(inner?)))
            }
            "" if self.sym("(") => {
                self.at -= 1;
                Ok(Stmt::Select(Box::new(self.select()?)))
            }
            _ => Err(syntax(format!(
                "a statement starts with SELECT, INSERT, UPDATE, DELETE, CREATE, DROP or ALTER, not {}",
                self.describe()
            ))),
        }
    }

    pub(crate) fn select(&mut self) -> Result<Select> {
        self.open()?;
        let around = std::mem::take(&mut self.tallest);
        let select = self.select_body();
        self.depth -= 1;
        let select = select?;
        self.height = self.tallest + SELECT_HEIGHT;
        self.tallest = around.max(self.height);
        if self.height > MAX_HEIGHT {
            return Err(too_deep());
        }
        Ok(select)
    }

    fn select_body(&mut self) -> Result<Select> {
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
        self.locking_clause()?;
        let id = SELECTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Select { id, first, rest, order, limit, offset })
    }

    /// PostgreSQL's `FOR UPDATE`, `FOR NO KEY UPDATE`, `FOR SHARE`, `FOR KEY
    /// SHARE` (`OF t, ...`, `NOWAIT`, `SKIP LOCKED`), accepted: a transaction of
    /// Nuage DB already fails at its commit (`40001`) if another commit changed
    /// a table it read, so the rows it read stay as it saw them or it retries.
    /// Outside a transaction, a statement runs alone and holds nothing after.
    fn locking_clause(&mut self) -> Result<()> {
        while self.peek_word(0).as_deref() == Some("for")
            && matches!(self.peek_word(1).as_deref(), Some("update" | "share" | "no" | "key"))
        {
            self.at += 1;
            if !(self.keyword("update")
                || self.keyword("share")
                || self.keywords(&["no", "key", "update"])
                || self.keywords(&["key", "share"]))
            {
                return Err(syntax(format!("expected UPDATE or SHARE after FOR, not {}", self.describe())));
            }
            if self.keyword("of") {
                loop {
                    self.name()?;
                    if !self.sym(",") {
                        break;
                    }
                }
            }
            let _ = self.keyword("nowait") || self.keywords(&["skip", "locked"]);
        }
        Ok(())
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
                    let mut equalities = cols.into_iter().map(|c| {
                        Expr::Bin(
                            Op::Eq,
                            Box::new(Expr::Col { table: Some(left.clone()), name: c.clone() }),
                            Box::new(Expr::Col { table: Some(right.clone()), name: c }),
                        )
                    });
                    let first = equalities.next();
                    let rest: Vec<(Op, Expr)> = equalities.map(|eq| (Op::And, eq)).collect();
                    match first {
                        Some(first) if !rest.is_empty() => Some(Expr::Chain(Box::new(first), rest)),
                        first => first,
                    }
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
        self.type_modifier()?;
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
        self.open()?;
        let e = self.or();
        self.depth -= 1;
        self.tallest = self.tallest.max(self.height);
        e
    }

    /// `operand (operator operand)*`, the operators of one level: one
    /// [`Expr::Chain`] however many operands there are.
    #[inline(always)]
    fn chain(
        &mut self,
        operand: fn(&mut Parser<'t>) -> Result<Expr>,
        operator: fn(&mut Parser<'t>) -> Option<Op>,
    ) -> Result<Expr> {
        let first = operand(self)?;
        let Some(mut op) = operator(self) else { return Ok(first) };
        let mut under = self.height;
        let mut rest = Vec::new();
        loop {
            let x = operand(self)?;
            under = under.max(self.height);
            rest.push((op, x));
            match operator(self) {
                Some(next) => op = next,
                None => break,
            }
        }
        self.built(under)?;
        Ok(Expr::Chain(Box::new(first), rest))
    }

    fn or(&mut self) -> Result<Expr> {
        self.chain(Parser::and, |p| p.keyword("or").then_some(Op::Or))
    }

    fn and(&mut self) -> Result<Expr> {
        self.chain(Parser::not, |p| p.keyword("and").then_some(Op::And))
    }

    fn not(&mut self) -> Result<Expr> {
        if !self.keyword("not") {
            return self.comparison();
        }
        self.open()?;
        let inner = self.not();
        self.depth -= 1;
        let inner = inner?;
        self.built(self.height)?;
        Ok(Expr::Not(Box::new(inner)))
    }

    // The parser calls itself once per level of nesting, through every level
    // of precedence: what follows an operand (`IS`, `IN`, `BETWEEN`, `LIKE`, a
    // cast) is read by functions of their own, so that the frames on the way
    // down hold only what that way needs.

    fn comparison(&mut self) -> Result<Expr> {
        let left = self.concat()?;
        self.compared(left)
    }

    /// What follows the left operand of a comparison.
    fn compared(&mut self, left: Expr) -> Result<Expr> {
        if self.keyword("is") {
            return self.is(left);
        }
        if self.keyword("isnull") || self.keyword("notnull") {
            let negated = matches!(&self.toks[self.at - 1], Tok::Word(w) if w.eq_ignore_ascii_case("notnull"));
            self.built(self.height)?;
            return Ok(Expr::IsNull(Box::new(left), negated));
        }
        let negated = self.keyword("not");
        if self.keyword("in") {
            return self.in_list(left, negated);
        }
        if self.keyword("between") {
            return self.between(left, negated);
        }
        if self.keyword("like") || self.keyword("ilike") {
            let ci = matches!(&self.toks[self.at - 1], Tok::Word(w) if w.eq_ignore_ascii_case("ilike"));
            return self.like(left, ci, negated);
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
        let under = self.height;
        // `x = ANY (...)` is not supported; plain comparisons only.
        let right = self.concat()?;
        self.built(under.max(self.height))?;
        Ok(Expr::Bin(op, Box::new(left), Box::new(right)))
    }

    /// `IS [NOT] NULL`, `IS [NOT] TRUE`, `IS [NOT] DISTINCT FROM`, after `IS`.
    fn is(&mut self, left: Expr) -> Result<Expr> {
        let under = self.height;
        let negated = self.keyword("not");
        if self.keyword("null") {
            self.built(under)?;
            return Ok(Expr::IsNull(Box::new(left), negated));
        }
        if self.keyword("true") || self.keyword("false") {
            let want = matches!(&self.toks[self.at - 1], Tok::Word(w) if w.eq_ignore_ascii_case("true"));
            // The comparison, `coalesce` around it, and `NOT` around both.
            self.built(under + if negated { 2 } else { 1 })?;
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
        // `a IS [NOT] DISTINCT FROM b`; `a IS [NOT] b`: SQLite's null-safe equality.
        let distinct = self.keywords(&["distinct", "from"]);
        let right = self.concat()?;
        let under = under.max(self.height);
        let same = Expr::Func { name: "_same".into(), args: vec![left, right], distinct: false, star: false };
        let wrapped = distinct != negated;
        self.built(under + usize::from(wrapped))?;
        Ok(if wrapped { Expr::Not(Box::new(same)) } else { same })
    }

    /// `IN (SELECT ...)` or `IN (a, b, ...)`, after `IN`.
    fn in_list(&mut self, left: Expr, negated: bool) -> Result<Expr> {
        let mut under = self.height;
        self.expect_sym("(")?;
        if matches!(self.peek_word(0).as_deref(), Some("select" | "with")) {
            let sub = self.select()?;
            under = under.max(self.height);
            self.expect_sym(")")?;
            self.built(under)?;
            return Ok(Expr::InSelect(Box::new(left), Box::new(sub), negated));
        }
        let mut list = Vec::new();
        if !self.sym(")") {
            loop {
                list.push(self.expr()?);
                under = under.max(self.height);
                if !self.sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
        }
        self.built(under)?;
        Ok(Expr::In(Box::new(left), list, negated))
    }

    /// `BETWEEN low AND high`, after `BETWEEN`.
    fn between(&mut self, left: Expr, negated: bool) -> Result<Expr> {
        let under = self.height;
        let low = self.concat()?;
        let under = under.max(self.height);
        self.expect("and")?;
        let high = self.concat()?;
        self.built(under.max(self.height))?;
        Ok(Expr::Between(Box::new(left), Box::new(low), Box::new(high), negated))
    }

    /// `LIKE pattern [ESCAPE e]`, after `LIKE` or `ILIKE`.
    fn like(&mut self, left: Expr, ci: bool, negated: bool) -> Result<Expr> {
        let under = self.height;
        let pattern = self.concat()?;
        let mut under = under.max(self.height);
        let escape = if self.keyword("escape") {
            let e = self.concat()?;
            under = under.max(self.height);
            Some(Box::new(e))
        } else {
            None
        };
        self.built(under)?;
        Ok(Expr::Like { expr: Box::new(left), pattern: Box::new(pattern), ci, negated, escape })
    }

    fn concat(&mut self) -> Result<Expr> {
        self.chain(Parser::additive, |p| p.sym("||").then_some(Op::Concat))
    }

    /// `x + y - z`. `x + INTERVAL '1 day'`, `x - INTERVAL '2 hours'`: the
    /// interval stays an operand, and whoever evaluates the chain moves the
    /// date by it ([`Expr::interval`]).
    fn additive(&mut self) -> Result<Expr> {
        let e = self.chain(Parser::term, |p| {
            if p.sym("+") {
                Some(Op::Add)
            } else if p.sym("-") {
                Some(Op::Sub)
            } else {
                None
            }
        })?;
        Ok(match e {
            // `INTERVAL '...' + x`: the date first.
            Expr::Chain(mut first, mut rest)
                if first.interval().is_some() && rest[0].0 == Op::Add && rest[0].1.interval().is_none() =>
            {
                std::mem::swap(&mut *first, &mut rest[0].1);
                Expr::Chain(first, rest)
            }
            e => e,
        })
    }

    fn term(&mut self) -> Result<Expr> {
        self.chain(Parser::unary, |p| {
            if p.sym("*") {
                Some(Op::Mul)
            } else if p.sym("/") {
                Some(Op::Div)
            } else if p.sym("%") {
                Some(Op::Mod)
            } else {
                None
            }
        })
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.sym("-") {
            return self.negative();
        }
        if self.sym("+") {
            self.open()?;
            let inner = self.unary();
            self.depth -= 1;
            return inner;
        }
        let e = self.primary()?;
        if self.peek() == Some(&Tok::Sym("::")) { self.casts(e) } else { Ok(e) }
    }

    /// `- x`, after the `-`.
    fn negative(&mut self) -> Result<Expr> {
        self.open()?;
        let inner = self.unary();
        self.depth -= 1;
        Ok(match inner? {
            Expr::Lit(Value::I64(n)) if n != i64::MIN => Expr::Lit(Value::I64(-n)),
            Expr::Lit(Value::F64(n)) => Expr::Lit(Value::F64(-n)),
            e => {
                self.built(self.height)?;
                Expr::Neg(Box::new(e))
            }
        })
    }

    /// `e::type::type...`.
    fn casts(&mut self, mut e: Expr) -> Result<Expr> {
        while self.sym("::") {
            let word = self.name()?;
            let (kind, _) = type_of(&word).ok_or_else(|| syntax(format!("unknown type `{word}`")))?;
            self.type_modifier()?;
            self.built(self.height)?;
            e = Expr::Cast(Box::new(e), column_type(kind));
        }
        Ok(e)
    }

    fn primary(&mut self) -> Result<Expr> {
        // A value or a name, unless said otherwise below.
        self.height = 1;
        match self.peek() {
            Some(Tok::Sym("(")) => self.parenthesized(),
            Some(Tok::Word(_)) => self.word(),
            _ => self.literal(),
        }
    }

    /// `(SELECT ...)` or `(expression)`, at the `(`.
    fn parenthesized(&mut self) -> Result<Expr> {
        self.at += 1;
        if matches!(self.peek_word(0).as_deref(), Some("select" | "with" | "values")) {
            let sub = self.select()?;
            self.expect_sym(")")?;
            self.built(self.height)?;
            return Ok(Expr::Scalar(Box::new(sub)));
        }
        let e = self.expr()?;
        if self.sym(",") {
            return Err(syntax("row values `(a, b)` are not supported"));
        }
        self.expect_sym(")")?;
        Ok(e)
    }

    /// A value, a parameter or a quoted name.
    fn literal(&mut self) -> Result<Expr> {
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
            Tok::Quoted(_) | Tok::Word(_) => self.column(),
            Tok::Sym(s) => Err(syntax(format!("unexpected `{s}`"))),
        }
    }

    /// What starts with a word: a keyword (`NULL`, `CASE`, `EXISTS`...), a
    /// function, a column.
    fn word(&mut self) -> Result<Expr> {
        let Some(Tok::Word(w)) = self.peek() else { return self.literal() };
        let lw = w.to_ascii_lowercase();
        let next = self.toks.get(self.at + 1);
        let called = next == Some(&Tok::Sym("("));
        let quoted = matches!(next, Some(Tok::Str(_)));
        match lw.as_str() {
            "null" | "true" | "false" => {
                self.at += 1;
                Ok(Expr::Lit(match lw.as_str() {
                    "null" => Value::Null,
                    b => Value::Bool(b == "true"),
                }))
            }
            "current_timestamp" | "current_date" | "current_time" | "localtimestamp" if !called => {
                self.at += 1;
                let name = if lw == "localtimestamp" { "current_timestamp".into() } else { lw };
                Ok(Expr::Func { name, args: Vec::new(), distinct: false, star: false })
            }
            "date" | "timestamp" | "timestamptz" | "time" | "interval" if quoted => self.typed_literal(&lw),
            "exists" => self.exists(),
            "case" => {
                self.at += 1;
                self.case()
            }
            "cast" if called => self.cast(),
            _ if called => self.call(lw),
            _ => self.column(),
        }
    }

    /// `DATE '2026-01-31'`, `TIMESTAMP '...'`, `TIMESTAMPTZ '...'`; `INTERVAL
    /// '1 day'`: a span to add to or take from a date (`now() - INTERVAL '2 hours'`).
    fn typed_literal(&mut self, kind: &str) -> Result<Expr> {
        let Some(Tok::Str(text)) = self.toks.get(self.at + 1).cloned() else {
            return Err(syntax(format!("expected a text after {}", kind.to_ascii_uppercase())));
        };
        self.at += 2;
        self.height = 2;
        if kind == "interval" {
            value::interval_months_and_micros(&text)?;
            return Ok(Expr::Func {
                name: "_interval".into(),
                args: vec![Expr::Lit(Value::Text(text))],
                distinct: false,
                star: false,
            });
        }
        let (kind, _) = type_of(kind).ok_or_else(|| syntax(format!("unknown type `{kind}`")))?;
        Ok(Expr::Cast(Box::new(Expr::Lit(Value::Text(text))), column_type(kind)))
    }

    /// `EXISTS (SELECT ...)`, at `EXISTS`.
    fn exists(&mut self) -> Result<Expr> {
        self.at += 1;
        self.expect_sym("(")?;
        let sub = self.select()?;
        self.expect_sym(")")?;
        self.built(self.height)?;
        Ok(Expr::Exists(Box::new(sub), false))
    }

    /// `CAST(x AS type)`, at `CAST`.
    fn cast(&mut self) -> Result<Expr> {
        self.at += 2;
        let e = self.expr()?;
        let under = self.height;
        self.expect("as")?;
        let word = self.name()?;
        let (kind, _) = type_of(&word).ok_or_else(|| syntax(format!("unknown type `{word}`")))?;
        // `DOUBLE PRECISION`, `VARCHAR(255)`, `TIMESTAMP WITH TIME ZONE`.
        while !self.sym(")") {
            if self.peek() == Some(&Tok::Sym("(")) {
                self.type_modifier()?;
                continue;
            }
            if self.peek().is_none() {
                return Err(syntax("expected `)`"));
            }
            self.at += 1;
        }
        self.built(under)?;
        Ok(Expr::Cast(Box::new(e), column_type(kind)))
    }

    fn column(&mut self) -> Result<Expr> {
        let first = self.name()?;
        self.height = 1;
        if self.sym(".") {
            let name = self.name()?;
            return Ok(Expr::Col { table: Some(first), name });
        }
        Ok(Expr::Col { table: None, name: first })
    }

    fn call(&mut self, name: String) -> Result<Expr> {
        // The functions the parser writes itself (`_same`, `_interval`) are not for statements.
        if name.starts_with('_') {
            return Err(syntax(format!("unknown function {name}()")));
        }
        self.at += 2; // name (
        if name == "values" {
            // MySQL's `VALUES(col)` in ON DUPLICATE KEY UPDATE: the new value.
            let col = self.name()?;
            self.expect_sym(")")?;
            self.height = 1;
            return Ok(Expr::Col { table: Some("excluded".into()), name: col });
        }
        if self.sym("*") {
            self.expect_sym(")")?;
            self.height = 1;
            return Ok(Expr::Func { name, args: Vec::new(), distinct: false, star: true });
        }
        if name == "extract" && matches!(self.peek_word(1).as_deref(), Some("from")) {
            return self.extract(name);
        }
        let distinct = self.keyword("distinct");
        let mut args = Vec::new();
        let mut under = 0;
        if !self.sym(")") {
            loop {
                args.push(self.expr()?);
                under = under.max(self.height);
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
        self.built(under)?;
        Ok(Expr::Func { name, args, distinct, star: false })
    }

    /// `extract(epoch FROM x)`: the standard form of extract('epoch', x).
    fn extract(&mut self, name: String) -> Result<Expr> {
        let field = self.name()?;
        self.expect("from")?;
        let x = self.expr()?;
        let under = self.height;
        self.expect_sym(")")?;
        self.built(under)?;
        Ok(Expr::Func {
            name,
            args: vec![Expr::Lit(Value::Text(field.to_ascii_lowercase())), x],
            distinct: false,
            star: false,
        })
    }

    fn case(&mut self) -> Result<Expr> {
        let mut under = 0;
        let operand = if matches!(self.peek_word(0).as_deref(), Some("when")) {
            None
        } else {
            let e = self.expr()?;
            under = self.height;
            Some(Box::new(e))
        };
        let mut whens = Vec::new();
        while self.keyword("when") {
            let w = self.expr()?;
            under = under.max(self.height);
            self.expect("then")?;
            let then = self.expr()?;
            under = under.max(self.height);
            whens.push((w, then));
        }
        if whens.is_empty() {
            return Err(syntax("CASE needs at least one WHEN"));
        }
        let otherwise = if self.keyword("else") {
            let e = self.expr()?;
            under = under.max(self.height);
            Some(Box::new(e))
        } else {
            None
        };
        self.expect("end")?;
        self.built(under)?;
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
            // A type's `(` left open: once read past the end for ever.
            "select 1::int(",
            "select 1::varchar(10",
            "select cast(1 as varchar(10",
            // The functions the parser writes itself.
            "select _same(1, 2)",
            "select 1 + _interval('1 day')",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn expressions() {
        let e = parse_expr("a + b * 2 - -c || 'x' = 'y' AND NOT d IS NULL OR e IN (1, 2)").unwrap();
        let Expr::Chain(and, or) = e else { panic!() };
        assert!(matches!(or.as_slice(), [(Op::Or, Expr::In(..))]));
        let Expr::Chain(compared, not) = *and else { panic!() };
        assert!(matches!(not.as_slice(), [(Op::And, Expr::Not(_))]));
        let Expr::Bin(Op::Eq, left, _) = *compared else { panic!() };
        // `a + (b * 2) - (-c)`, then `|| 'x'`.
        let Expr::Chain(sum, concat) = *left else { panic!() };
        assert!(matches!(concat.as_slice(), [(Op::Concat, Expr::Lit(_))]));
        let Expr::Chain(a, terms) = *sum else { panic!() };
        assert!(matches!(*a, Expr::Col { .. }));
        assert!(matches!(terms.as_slice(), [(Op::Add, Expr::Chain(..)), (Op::Sub, Expr::Neg(_))]));
        assert!(matches!(parse_expr("CASE WHEN a > 1 THEN 'big' ELSE 'small' END").unwrap(), Expr::Case { .. }));
        assert!(matches!(parse_expr("CAST(x AS INTEGER)").unwrap(), Expr::Cast(_, ColumnType::Int)));
        assert!(matches!(parse_expr("x::text").unwrap(), Expr::Cast(_, ColumnType::Text)));
        assert!(matches!(parse_expr("-5").unwrap(), Expr::Lit(Value::I64(-5))));
        assert!(matches!(parse_expr("count(DISTINCT x)").unwrap(), Expr::Func { distinct: true, .. }));
    }
}
