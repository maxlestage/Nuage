//! Running SQL statements on a transaction of the engine.
//!
//! A `SELECT` that Memoire's statements can express (columns, comparisons,
//! `IN`, `LIKE`, joins, `GROUP BY`, aggregates, `ORDER BY`, `LIMIT`) runs as
//! one Memoire statement: through the indexes, stopping as soon as the page
//! is full. Anything else (functions, `CASE`, subqueries, `UNION`, derived
//! tables...) runs here: each table is read with the part of the filter its
//! indexes can serve, then joined (hash joins on equalities), filtered,
//! grouped, sorted and cut by this module.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use memoire_orm::ast::{DeleteAst, Projection, SelectAst, UpdateAst};
use memoire_orm::nuage::wire::SchemaCache;
use memoire_orm::{
    AggFunc, BinOp, ColumnRef, ColumnType, Error, ExecResult, Expr as M, Nulls, Order, Query, Result, TableSchema,
    Value,
};
use serde_json::{Value as Json, json};

use super::ast::*;
use super::lex::syntax;
use super::parse::{check_name, column_type, parse_expr};
use super::value::{self as v, arith, bool_or_null, cast, compare, order_cmp, tri, truthy};
use crate::embedded::EmbeddedTransaction;
use crate::engine::exec::Alter;

/// What a statement gave.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SqlResult {
    /// Column names of the rows (empty for a statement without rows).
    pub columns: Vec<String>,
    /// Rows: of a `SELECT`, or of `RETURNING`.
    pub rows: Vec<Vec<Value>>,
    /// Rows inserted, updated or deleted.
    pub rows_affected: u64,
    /// The key the last inserted row got.
    pub last_insert_id: Option<Value>,
    /// It returns rows (a `SELECT`, `RETURNING`, `EXPLAIN`).
    pub returns_rows: bool,
}

/// Rows and their column names.
#[derive(Clone, Debug, Default)]
struct Rows {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
}

/// One table (or derived table) of a `FROM`.
#[derive(Clone, Debug)]
struct Part {
    alias: String,
    columns: Vec<String>,
    kinds: Vec<Option<ColumnType>>,
    schema: Option<&'static TableSchema>,
    offset: usize,
}

#[derive(Clone, Debug, Default)]
struct Scope {
    parts: Vec<Part>,
    width: usize,
}

impl Scope {
    fn push(
        &mut self,
        alias: String,
        columns: Vec<String>,
        kinds: Vec<Option<ColumnType>>,
        schema: Option<&'static TableSchema>,
    ) {
        let n = columns.len();
        self.parts.push(Part { alias, columns, kinds, schema, offset: self.width });
        self.width += n;
    }

    fn of_schema(alias: &str, schema: &'static TableSchema) -> Scope {
        let mut s = Scope::default();
        s.push(
            alias.to_owned(),
            schema.columns.iter().map(|c| c.name.to_owned()).collect(),
            schema.columns.iter().map(|c| Some(c.kind)).collect(),
            Some(schema),
        );
        s
    }

    /// The part and the column of a reference, `None` when it is not here.
    fn find(&self, table: Option<&str>, name: &str) -> Result<Option<(usize, usize)>> {
        let mut found = None;
        for (p, part) in self.parts.iter().enumerate() {
            if table.is_some_and(|t| !t.eq_ignore_ascii_case(&part.alias)) {
                continue;
            }
            if let Some(c) = part.columns.iter().position(|c| c.eq_ignore_ascii_case(name)) {
                if found.is_some() {
                    return Err(syntax(format!("column `{name}` is ambiguous: write table.{name}")));
                }
                found = Some((p, c));
            }
        }
        Ok(found)
    }
}

/// Where an expression is evaluated: a row of a scope, its group (for
/// aggregates), the enclosing query's row (correlated subqueries), and the
/// output columns (`ORDER BY` an alias).
struct Env<'e> {
    scope: &'e Scope,
    row: &'e [Value],
    group: Option<&'e [Vec<Value>]>,
    outer: Option<&'e Env<'e>>,
    used_outer: &'e Cell<bool>,
    output: Option<(&'e [String], &'e [Value])>,
}

impl Env<'_> {
    fn lookup(&self, table: Option<&str>, name: &str) -> Result<Value> {
        if let Some((p, c)) = self.scope.find(table, name)? {
            return Ok(self.row.get(self.scope.parts[p].offset + c).cloned().unwrap_or(Value::Null));
        }
        if table.is_none()
            && let Some((cols, vals)) = self.output
            && let Some(i) = cols.iter().position(|c| c.eq_ignore_ascii_case(name))
        {
            return Ok(vals[i].clone());
        }
        if let Some(o) = self.outer {
            self.used_outer.set(true);
            return o.lookup(table, name);
        }
        Err(match table {
            Some(t) => syntax(format!("no column `{t}.{name}`")),
            None => syntax(format!("no column `{name}`")),
        })
    }
}

/// Runs statements on one transaction.
pub(crate) struct Runner<'a> {
    pub tx: &'a EmbeddedTransaction,
    pub schemas: &'a SchemaCache,
    pub params: &'a [Value],
    /// Results of subqueries that read nothing of the enclosing row.
    cache: RefCell<HashMap<usize, Rc<Rows>>>,
}

fn conj(e: &Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::Bin(Op::And, a, b) => {
            conj(a, out);
            conj(b, out);
        }
        other => out.push(other.clone()),
    }
}

fn and_all(parts: Vec<M>) -> Option<M> {
    parts.into_iter().reduce(|a, b| M::binary(BinOp::And, a, b))
}

/// A column name for an expression without an alias.
fn header(e: &Expr, n: usize) -> String {
    match e {
        Expr::Col { name, .. } => name.clone(),
        Expr::Func { name, .. } => name.clone(),
        Expr::Cast(inner, _) => header(inner, n),
        _ => format!("column{}", n + 1),
    }
}

/// Unique names: a repeated name is qualified by its table (`users.id`).
fn unique_headers(names: Vec<(String, Option<String>)>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for (name, table) in names {
        let taken = out.iter().any(|o| o.eq_ignore_ascii_case(&name));
        out.push(match (taken, table) {
            (true, Some(t)) => format!("{t}.{name}"),
            _ => name,
        });
    }
    out
}

impl<'a> Runner<'a> {
    pub(crate) fn new(tx: &'a EmbeddedTransaction, schemas: &'a SchemaCache, params: &'a [Value]) -> Self {
        Runner { tx, schemas, params, cache: RefCell::new(HashMap::new()) }
    }

    /// The schema of a table (its name in any case).
    pub(crate) fn schema(&self, name: &str) -> Result<&'static TableSchema> {
        let json = match self.tx.table_schema(name)? {
            Some(j) => j,
            None => {
                let real = self.tx.table_names()?.into_iter().find(|t| t.eq_ignore_ascii_case(name));
                match real {
                    Some(r) => self.tx.table_schema(&r)?.ok_or_else(|| Error::NoSuchTable(name.to_owned()))?,
                    None => return Err(Error::NoSuchTable(name.to_owned())),
                }
            }
        };
        self.schemas.schema(&json)
    }

    fn param(&self, i: usize) -> Result<Value> {
        self.params.get(i).cloned().ok_or_else(|| syntax(format!("parameter {} is missing", i + 1)))
    }

    // -----------------------------------------------------------------------
    // Expressions
    // -----------------------------------------------------------------------

    fn eval(&self, e: &Expr, env: &Env<'_>) -> Result<Value> {
        Ok(match e {
            Expr::Lit(v) => v.clone(),
            Expr::Param(i) => self.param(*i)?,
            Expr::Col { table, name } => env.lookup(table.as_deref(), name)?,
            Expr::Neg(x) => {
                let v = self.eval(x, env)?;
                match v {
                    Value::I32(i) => Value::I32(-i),
                    Value::F64(f) => Value::F64(-f),
                    other => arith(Op::Sub, &Value::I64(0), &other)?,
                }
            }
            Expr::Not(x) => bool_or_null(tri(&self.eval(x, env)?).map(|b| !b)),
            Expr::Bin(Op::And, a, b) => {
                let l = tri(&self.eval(a, env)?);
                if l == Some(false) {
                    return Ok(Value::Bool(false));
                }
                let r = tri(&self.eval(b, env)?);
                bool_or_null(match (l, r) {
                    (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                })
            }
            Expr::Bin(Op::Or, a, b) => {
                let l = tri(&self.eval(a, env)?);
                if l == Some(true) {
                    return Ok(Value::Bool(true));
                }
                let r = tri(&self.eval(b, env)?);
                bool_or_null(match (l, r) {
                    (_, Some(true)) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                })
            }
            Expr::Bin(op @ (Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge), a, b) => {
                let (l, r) = (self.eval(a, env)?, self.eval(b, env)?);
                if l.is_null() || r.is_null() {
                    return Ok(Value::Null);
                }
                let ord = compare(&l, &r);
                Value::Bool(match (op, ord) {
                    (Op::Ne, None) => true,
                    (_, None) => false,
                    (Op::Eq, Some(o)) => o.is_eq(),
                    (Op::Ne, Some(o)) => !o.is_eq(),
                    (Op::Lt, Some(o)) => o.is_lt(),
                    (Op::Le, Some(o)) => o.is_le(),
                    (Op::Gt, Some(o)) => o.is_gt(),
                    (_, Some(o)) => o.is_ge(),
                })
            }
            Expr::Bin(op, a, b) => arith(*op, &self.eval(a, env)?, &self.eval(b, env)?)?,
            Expr::IsNull(x, negated) => Value::Bool(self.eval(x, env)?.is_null() != *negated),
            Expr::In(x, list, negated) => {
                let v = self.eval(x, env)?;
                let mut values = Vec::with_capacity(list.len());
                for item in list {
                    values.push(self.eval(item, env)?);
                }
                in_values(&v, &values, *negated)
            }
            Expr::InSelect(x, sub, negated) => {
                let v = self.eval(x, env)?;
                let rows = self.subquery(sub, env)?;
                if rows.columns.len() != 1 {
                    return Err(syntax("the subquery of IN must return one column"));
                }
                let values: Vec<Value> = rows.rows.iter().map(|r| r[0].clone()).collect();
                in_values(&v, &values, *negated)
            }
            Expr::Exists(sub, negated) => Value::Bool(self.subquery(sub, env)?.rows.is_empty() == *negated),
            Expr::Scalar(sub) => {
                let rows = self.subquery(sub, env)?;
                if rows.columns.len() != 1 {
                    return Err(syntax("a subquery used as a value must return one column"));
                }
                rows.rows.first().map_or(Value::Null, |r| r[0].clone())
            }
            Expr::Between(x, low, high, negated) => {
                let (v, l, h) = (self.eval(x, env)?, self.eval(low, env)?, self.eval(high, env)?);
                if v.is_null() || l.is_null() || h.is_null() {
                    return Ok(Value::Null);
                }
                let inside = compare(&v, &l).is_some_and(|o| o.is_ge()) && compare(&v, &h).is_some_and(|o| o.is_le());
                Value::Bool(inside != *negated)
            }
            Expr::Like { expr, pattern, ci, negated, escape } => {
                let (s, p) = (self.eval(expr, env)?, self.eval(pattern, env)?);
                let (Some(s), Some(p)) = (v::to_text(&s), v::to_text(&p)) else { return Ok(Value::Null) };
                let esc = match escape {
                    Some(e) => v::to_text(&self.eval(e, env)?).and_then(|t| t.chars().next()),
                    None => None,
                };
                Value::Bool(v::like(&s, &p, *ci, esc) != *negated)
            }
            Expr::Case { operand, whens, otherwise } => {
                let subject = match operand {
                    Some(o) => Some(self.eval(o, env)?),
                    None => None,
                };
                for (w, then) in whens {
                    let hit = match &subject {
                        Some(s) => compare(s, &self.eval(w, env)?).is_some_and(|o| o.is_eq()),
                        None => truthy(&self.eval(w, env)?),
                    };
                    if hit {
                        return self.eval(then, env);
                    }
                }
                match otherwise {
                    Some(o) => self.eval(o, env)?,
                    None => Value::Null,
                }
            }
            Expr::Cast(x, kind) => cast(&self.eval(x, env)?, *kind)?,
            Expr::Func { .. } if e.is_aggregate() => self.aggregate(e, env)?,
            Expr::Func { name, args, .. } => {
                let mut values = Vec::with_capacity(args.len());
                for a in args {
                    values.push(self.eval(a, env)?);
                }
                v::scalar(name, values)?
            }
        })
    }

    fn aggregate(&self, e: &Expr, env: &Env<'_>) -> Result<Value> {
        let Expr::Func { name, args, distinct, star } = e else { unreachable!() };
        let Some(rows) = env.group else {
            return Err(syntax(format!("{name}() is an aggregate: not allowed in WHERE or ON (use HAVING)")));
        };
        if *star {
            if name != "count" {
                return Err(syntax(format!("{name}(*) is not valid: only count(*)")));
            }
            return Ok(Value::I64(rows.len() as i64));
        }
        let mut values = Vec::with_capacity(rows.len());
        let mut seen = std::collections::HashSet::new();
        for row in rows {
            let inner =
                Env { scope: env.scope, row, group: None, outer: env.outer, used_outer: env.used_outer, output: None };
            let x = self.eval(&args[0], &inner)?;
            if x.is_null() {
                continue;
            }
            if *distinct {
                let mut k = String::new();
                v::key(&x, &mut k);
                if !seen.insert(k) {
                    continue;
                }
            }
            values.push(x);
        }
        Ok(match name.as_str() {
            "count" => Value::I64(values.len() as i64),
            "sum" | "total" => {
                if values.is_empty() {
                    return Ok(if name == "total" { Value::F64(0.0) } else { Value::Null });
                }
                let mut acc = if name == "total" { Value::F64(0.0) } else { Value::I64(0) };
                for x in &values {
                    acc = arith(Op::Add, &acc, x)?;
                }
                acc
            }
            "avg" => {
                if values.is_empty() {
                    return Ok(Value::Null);
                }
                let mut sum = 0.0;
                for x in &values {
                    sum += cast(x, ColumnType::Double)?.as_f64().unwrap_or(0.0);
                }
                Value::F64(sum / values.len() as f64)
            }
            "min" | "max" => {
                let want = if name == "max" { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less };
                let mut best: Option<Value> = None;
                for x in values {
                    best = match best {
                        Some(b) if compare(&x, &b) != Some(want) => Some(b),
                        _ => Some(x),
                    };
                }
                best.unwrap_or(Value::Null)
            }
            "array_agg" => Value::Json(Json::Array(values.iter().map(v::to_json).collect())),
            _ => {
                // group_concat(x [, sep]), string_agg(x, sep)
                if values.is_empty() {
                    return Ok(Value::Null);
                }
                let sep = match args.get(1) {
                    Some(s) => v::to_text(&self.eval(s, env)?).unwrap_or_default(),
                    None => ",".into(),
                };
                Value::Text(values.iter().filter_map(v::to_text).collect::<Vec<_>>().join(&sep))
            }
        })
    }

    fn subquery(&self, sub: &Select, env: &Env<'_>) -> Result<Rc<Rows>> {
        let key = sub as *const Select as usize;
        if let Some(r) = self.cache.borrow().get(&key) {
            return Ok(r.clone());
        }
        let used = Cell::new(false);
        let rows = Rc::new(self.select(sub, Some(env), &used)?);
        if !used.get() {
            self.cache.borrow_mut().insert(key, rows.clone());
        }
        Ok(rows)
    }

    // -----------------------------------------------------------------------
    // Translation to Memoire's statements
    // -----------------------------------------------------------------------

    /// `e` as a Memoire expression over `scope` (only part `only`, when
    /// given), the enclosing row's columns as values. `None`: Memoire cannot
    /// run it.
    fn to_m(
        &self,
        e: &Expr,
        scope: &Scope,
        only: Option<usize>,
        outer: Option<&Env<'_>>,
        used: &Cell<bool>,
    ) -> Option<M> {
        Some(match e {
            Expr::Lit(v) => M::Value(v.clone()),
            Expr::Param(i) => M::Value(self.param(*i).ok()?),
            Expr::Col { table, name } => match scope.find(table.as_deref(), name).ok()? {
                Some((p, c)) => {
                    if only.is_some_and(|o| o != p) {
                        return None;
                    }
                    let schema = scope.parts[p].schema?;
                    let col = &schema.columns[c];
                    M::Column(ColumnRef { table: schema.name, name: col.name, index: c, kind: col.kind })
                }
                None => {
                    let o = outer?;
                    let v = o.lookup(table.as_deref(), name).ok()?;
                    used.set(true);
                    M::Value(v)
                }
            },
            Expr::Not(x) => M::Not(Box::new(self.to_m(x, scope, only, outer, used)?)),
            Expr::IsNull(x, negated) => {
                M::IsNull { expr: Box::new(self.to_m(x, scope, only, outer, used)?), negated: *negated }
            }
            Expr::Bin(op, a, b) => {
                let op = match op {
                    Op::Eq => BinOp::Eq,
                    Op::Ne => BinOp::Ne,
                    Op::Lt => BinOp::Lt,
                    Op::Le => BinOp::Le,
                    Op::Gt => BinOp::Gt,
                    Op::Ge => BinOp::Ge,
                    Op::And => BinOp::And,
                    Op::Or => BinOp::Or,
                    Op::Add | Op::Sub | Op::Mul | Op::Div => return None,
                    Op::Mod | Op::Concat => return None,
                };
                let (l, r) = (self.to_m(a, scope, only, outer, used)?, self.to_m(b, scope, only, outer, used)?);
                // A value compared with a column: of the column's type (an
                // index holds typed values).
                let (l, r) = match (&l, &r) {
                    (M::Column(c), M::Value(x)) => {
                        let x = coerce(x, c.kind)?;
                        (l, M::Value(x))
                    }
                    (M::Value(x), M::Column(c)) => {
                        let x = coerce(x, c.kind)?;
                        (M::Value(x), r)
                    }
                    (M::Column(a), M::Column(b)) if !same_family(a.kind, b.kind) => return None,
                    (M::Value(_), M::Value(_))
                        if matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) =>
                    {
                        return None;
                    }
                    _ => (l, r),
                };
                M::binary(op, l, r)
            }
            Expr::In(x, list, negated) => {
                let M::Column(c) = self.to_m(x, scope, only, outer, used)? else { return None };
                let mut values = Vec::with_capacity(list.len());
                for item in list {
                    let M::Value(v) = self.to_m(item, scope, only, outer, used)? else { return None };
                    if v.is_null() {
                        return None;
                    }
                    values.push(coerce(&v, c.kind)?);
                }
                M::InList { expr: Box::new(M::Column(c)), list: values, negated: *negated }
            }
            Expr::InSelect(x, sub, negated) => {
                let M::Column(c) = self.to_m(x, scope, only, outer, used)? else { return None };
                // Run it first (once), and look the values up: an index serves them.
                let env =
                    Env { scope: &Scope::default(), row: &[], group: None, outer, used_outer: used, output: None };
                let rows = self.subquery(sub, &env).ok()?;
                if rows.columns.len() != 1 || rows.rows.iter().any(|r| r[0].is_null()) {
                    return None;
                }
                let list = rows.rows.iter().map(|r| coerce(&r[0], c.kind)).collect::<Option<Vec<_>>>()?;
                M::InList { expr: Box::new(M::Column(c)), list, negated: *negated }
            }
            Expr::Between(x, low, high, negated) => {
                let M::Column(c) = self.to_m(x, scope, only, outer, used)? else { return None };
                let (M::Value(l), M::Value(h)) =
                    (self.to_m(low, scope, only, outer, used)?, self.to_m(high, scope, only, outer, used)?)
                else {
                    return None;
                };
                M::Between {
                    expr: Box::new(M::Column(c)),
                    low: Box::new(M::Value(coerce(&l, c.kind)?)),
                    high: Box::new(M::Value(coerce(&h, c.kind)?)),
                    negated: *negated,
                }
            }
            Expr::Like { expr, pattern, ci, negated, escape: None } => {
                let M::Column(c) = self.to_m(expr, scope, only, outer, used)? else { return None };
                if c.kind != ColumnType::Text {
                    return None;
                }
                let M::Value(Value::Text(p)) = self.to_m(pattern, scope, only, outer, used)? else { return None };
                if p.contains('\\') {
                    return None;
                }
                M::Like {
                    expr: Box::new(M::Column(c)),
                    pattern: Box::new(M::Value(Value::Text(p))),
                    case_insensitive: *ci,
                    negated: *negated,
                    escaped: false,
                }
            }
            Expr::Func { name, args, distinct, star } if e.is_aggregate() => {
                let func = match name.as_str() {
                    "count" => AggFunc::Count,
                    "sum" => AggFunc::Sum,
                    "avg" => AggFunc::Avg,
                    "min" => AggFunc::Min,
                    "max" => AggFunc::Max,
                    _ => return None,
                };
                let arg = if *star { None } else { Some(Box::new(self.to_m(&args[0], scope, only, outer, used)?)) };
                if arg.as_deref().is_some_and(|a| !matches!(a, M::Column(_))) {
                    return None;
                }
                M::Aggregate { func, arg, distinct: *distinct }
            }
            _ => return None,
        })
    }

    /// The whole `SELECT` as one Memoire statement, when it can be.
    fn as_memoire(&self, sel: &Select, outer: Option<&Env<'_>>, used: &Cell<bool>) -> Option<(SelectAst, Vec<String>)> {
        if !sel.rest.is_empty() {
            return None;
        }
        let core = &sel.first;
        let from = core.from.as_ref()?;
        let mut scope = Scope::default();
        let mut tables: Vec<&'static TableSchema> = Vec::new();
        for t in std::iter::once(from).chain(core.joins.iter().map(|j| &j.table)) {
            let Source::Table(name) = &t.source else { return None };
            let schema = self.schema(name).ok()?;
            if tables.iter().any(|s| s.name == schema.name) {
                return None;
            }
            tables.push(schema);
            let alias = t.alias.clone().unwrap_or_else(|| name.clone());
            scope.push(
                alias,
                schema.columns.iter().map(|c| c.name.to_owned()).collect(),
                schema.columns.iter().map(|c| Some(c.kind)).collect(),
                Some(schema),
            );
        }
        let mut ast = SelectAst::new(tables[0]);
        for (j, schema) in core.joins.iter().zip(&tables[1..]) {
            let on = match (&j.kind, &j.on) {
                (JoinKind::Cross, _) | (_, None) => M::Value(Value::Bool(true)),
                (_, Some(on)) => self.to_m(on, &scope, None, outer, used)?,
            };
            ast.joins.push(memoire_orm::ast::Join {
                kind: if j.kind == JoinKind::Left {
                    memoire_orm::ast::JoinKind::Left
                } else {
                    memoire_orm::ast::JoinKind::Inner
                },
                table: schema,
                on,
            });
        }
        // The projection.
        let mut exprs = Vec::new();
        let mut names = Vec::new();
        let mut items: Vec<Option<&Expr>> = Vec::new();
        match core.items.as_slice() {
            [SelItem::All] => {
                ast.projection = Projection::Tables(tables.clone());
                for p in &scope.parts {
                    for c in &p.columns {
                        names.push((c.clone(), Some(p.alias.clone())));
                        items.push(None);
                    }
                }
            }
            list => {
                for (n, item) in list.iter().enumerate() {
                    let SelItem::Expr(e, alias) = item else { return None };
                    let m = self.to_m(e, &scope, None, outer, used)?;
                    if !matches!(m, M::Column(_) | M::Aggregate { .. }) {
                        return None;
                    }
                    exprs.push(m);
                    let table = match e {
                        Expr::Col { table, .. } => table.clone(),
                        _ => None,
                    };
                    names.push((alias.clone().unwrap_or_else(|| header(e, n)), table));
                    items.push(Some(e));
                }
                ast.projection = Projection::Exprs(exprs.clone());
            }
        }
        let headers = unique_headers(names);
        if let Some(f) = &core.filter {
            ast.filter = Some(self.to_m(f, &scope, None, outer, used)?);
        }
        for g in &core.group_by {
            ast.group_by.push(self.to_m(g, &scope, None, outer, used)?);
        }
        if let Some(h) = &core.having {
            ast.having = Some(self.to_m(h, &scope, None, outer, used)?);
        }
        let grouped = !ast.group_by.is_empty() || exprs.iter().any(|e| matches!(e, M::Aggregate { .. }));
        if grouped {
            // Every plain column must be grouped (Memoire's rule; SQLite's is looser).
            for (e, m) in items.iter().zip(&exprs) {
                if matches!(m, M::Column(_)) && !ast.group_by.contains(m) {
                    let _ = e;
                    return None;
                }
            }
        }
        for o in &sel.order {
            let target = match &o.expr {
                Expr::Lit(Value::I64(n)) => {
                    let i = usize::try_from(*n).ok()?.checked_sub(1)?;
                    exprs.get(i)?.clone()
                }
                Expr::Col { table: None, name }
                    if headers.iter().any(|h| h.eq_ignore_ascii_case(name))
                        && scope.find(None, name).ok()?.is_none() =>
                {
                    let i = headers.iter().position(|h| h.eq_ignore_ascii_case(name))?;
                    exprs.get(i)?.clone()
                }
                e => self.to_m(e, &scope, None, outer, used)?,
            };
            if grouped && !matches!(target, M::Aggregate { .. }) && !ast.group_by.contains(&target) {
                return None;
            }
            ast.order.push(Order {
                expr: target,
                desc: o.desc,
                nulls: o.nulls_first.map(|f| if f { Nulls::First } else { Nulls::Last }),
            });
        }
        ast.distinct = core.distinct;
        let count = |e: &Option<Expr>| -> Option<Option<u64>> {
            match e {
                None => Some(None),
                Some(Expr::Lit(Value::I64(n))) if *n >= 0 => Some(Some(*n as u64)),
                Some(Expr::Lit(Value::I64(_))) => Some(None),
                Some(Expr::Param(i)) => match self.param(*i).ok()? {
                    Value::I64(n) if n >= 0 => Some(Some(n as u64)),
                    Value::I64(_) => Some(None),
                    _ => None,
                },
                _ => None,
            }
        };
        ast.limit = count(&sel.limit)?;
        ast.offset = count(&sel.offset)?;
        Some((ast, headers))
    }

    // -----------------------------------------------------------------------
    // SELECT
    // -----------------------------------------------------------------------

    fn select(&self, sel: &Select, outer: Option<&Env<'_>>, used: &Cell<bool>) -> Result<Rows> {
        if let Some((ast, columns)) = self.as_memoire(sel, outer, used) {
            let rows = self.tx.fetch_rows(&Query::Select(&ast))?;
            return Ok(Rows { columns, rows });
        }
        let empty_env = Env { scope: &Scope::default(), row: &[], group: None, outer, used_outer: used, output: None };
        let limit = match &sel.limit {
            Some(e) => match self.eval(e, &empty_env)? {
                Value::Null => None,
                v => v.as_i64().filter(|n| *n >= 0).map(|n| n as usize),
            },
            None => None,
        };
        let offset = match &sel.offset {
            Some(e) => self.eval(e, &empty_env)?.as_i64().unwrap_or(0).max(0) as usize,
            None => 0,
        };
        let mut out = if sel.rest.is_empty() {
            let hint = limit.map(|l| l + offset);
            self.core(&sel.first, &sel.order, hint, outer, used)?
        } else {
            let mut acc = self.core(&sel.first, &[], None, outer, used)?;
            for (op, core) in &sel.rest {
                let next = self.core(core, &[], None, outer, used)?;
                if next.columns.len() != acc.columns.len() {
                    return Err(syntax("each SELECT of a UNION must have the same number of columns"));
                }
                let keyed = |r: &Vec<Value>| {
                    let mut k = String::new();
                    r.iter().for_each(|x| v::key(x, &mut k));
                    k
                };
                acc.rows = match op {
                    SetOp::UnionAll => acc.rows.into_iter().chain(next.rows).collect(),
                    SetOp::Union => {
                        let mut seen = std::collections::HashSet::new();
                        acc.rows.into_iter().chain(next.rows).filter(|r| seen.insert(keyed(r))).collect()
                    }
                    SetOp::Intersect => {
                        let other: std::collections::HashSet<String> = next.rows.iter().map(keyed).collect();
                        let mut seen = std::collections::HashSet::new();
                        acc.rows.into_iter().filter(|r| other.contains(&keyed(r)) && seen.insert(keyed(r))).collect()
                    }
                    SetOp::Except => {
                        let other: std::collections::HashSet<String> = next.rows.iter().map(keyed).collect();
                        let mut seen = std::collections::HashSet::new();
                        acc.rows.into_iter().filter(|r| !other.contains(&keyed(r)) && seen.insert(keyed(r))).collect()
                    }
                };
            }
            // ORDER BY of a compound: output columns, by name or position.
            if !sel.order.is_empty() {
                let mut keys = Vec::new();
                for o in &sel.order {
                    let i = match &o.expr {
                        Expr::Lit(Value::I64(n)) => (*n as usize).checked_sub(1).filter(|i| *i < acc.columns.len()),
                        Expr::Col { name, .. } => acc.columns.iter().position(|c| c.eq_ignore_ascii_case(name)),
                        _ => None,
                    }
                    .ok_or_else(|| syntax("ORDER BY of a UNION takes a column name or position"))?;
                    keys.push((i, o.desc, o.nulls_first));
                }
                acc.rows.sort_by(|a, b| {
                    keys.iter()
                        .map(|&(i, desc, nulls)| order_cmp(&a[i], &b[i], desc, nulls))
                        .find(|o| !o.is_eq())
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            acc
        };
        if offset > 0 || limit.is_some() {
            out.rows = out.rows.into_iter().skip(offset).take(limit.unwrap_or(usize::MAX)).collect();
        }
        Ok(out)
    }

    /// Rows of a table through Memoire: `filter` (served by the indexes), and
    /// for a lone table `ORDER BY ... LIMIT` (stopping early).
    fn read_table(
        &self,
        schema: &'static TableSchema,
        filter: Option<M>,
        top: Option<(Vec<Order>, u64)>,
    ) -> Result<Vec<Vec<Value>>> {
        let mut ast = SelectAst::new(schema);
        ast.filter = filter;
        if let Some((order, n)) = top {
            ast.order = order;
            ast.limit = Some(n);
        }
        self.tx.fetch_rows(&Query::Select(&ast))
    }

    /// One `SELECT` core: rows read, joined, filtered, grouped, projected,
    /// sorted (`order`), deduplicated; `hint`: only the first rows are needed.
    fn core(
        &self,
        core: &Core,
        order: &[OrderItem],
        hint: Option<usize>,
        outer: Option<&Env<'_>>,
        used: &Cell<bool>,
    ) -> Result<Rows> {
        // The scope.
        let mut scope = Scope::default();
        let mut sources: Vec<Option<Vec<Vec<Value>>>> = Vec::new();
        let refs: Vec<(&TableRef, Option<&Join>)> =
            core.from.iter().map(|f| (f, None)).chain(core.joins.iter().map(|j| (&j.table, Some(j)))).collect();
        for (t, _) in &refs {
            match &t.source {
                Source::Table(name) => {
                    let schema = self.schema(name)?;
                    let alias = t.alias.clone().unwrap_or_else(|| schema.name.to_owned());
                    scope.push(
                        alias,
                        schema.columns.iter().map(|c| c.name.to_owned()).collect(),
                        schema.columns.iter().map(|c| Some(c.kind)).collect(),
                        Some(schema),
                    );
                    sources.push(None);
                }
                Source::Select(sub) => {
                    let rows = self.select(sub, outer, used)?;
                    let alias = t
                        .alias
                        .clone()
                        .ok_or_else(|| syntax("a subquery in FROM needs an alias: (SELECT ...) AS name"))?;
                    let kinds = (0..rows.columns.len())
                        .map(|i| rows.rows.iter().map(|r| &r[i]).find(|x| !x.is_null()).and_then(v::kind_of))
                        .collect();
                    scope.push(alias, rows.columns.clone(), kinds, None);
                    sources.push(Some(rows.rows));
                }
            }
        }
        // WHERE, split: what each table's read can apply.
        let mut where_parts = Vec::new();
        if let Some(f) = &core.filter {
            conj(f, &mut where_parts);
        }
        let mut pushed = vec![false; where_parts.len()];
        let lone = refs.len() == 1;
        let grouped = !core.group_by.is_empty()
            || core.having.is_some()
            || core.items.iter().any(|i| matches!(i, SelItem::Expr(e, _) if e.has_aggregate()))
            || order.iter().any(|o| o.expr.has_aggregate());
        let mut rows: Vec<Vec<Value>> = if refs.is_empty() { vec![Vec::new()] } else { Vec::new() };
        for (p, (_, join)) in refs.iter().enumerate() {
            let part_rows = match sources[p].take() {
                Some(r) => r,
                None => {
                    let schema = scope.parts[p].schema.expect("a base table");
                    let mut filters = Vec::new();
                    let inner = join.is_none_or(|j| j.kind != JoinKind::Left);
                    if inner {
                        for (i, w) in where_parts.iter().enumerate() {
                            if let Some(m) = self.to_m(w, &scope, Some(p), outer, used) {
                                filters.push(m);
                                pushed[i] = true;
                            }
                        }
                    }
                    if let Some(Join { on: Some(on), .. }) = join {
                        let mut ons = Vec::new();
                        conj(on, &mut ons);
                        for o in &ons {
                            if let Some(m) = self.to_m(o, &scope, Some(p), outer, used) {
                                filters.push(m);
                            }
                        }
                    }
                    // A lone table, all its filter applied, rows needed in
                    // an order Memoire knows: it stops at the page's end.
                    let top = match hint {
                        Some(n) if lone && !grouped && !core.distinct && pushed.iter().all(|x| *x) => {
                            let ordered: Option<Vec<Order>> = order
                                .iter()
                                .map(|o| match self.to_m(&o.expr, &scope, Some(p), outer, used)? {
                                    m @ M::Column(_) => Some(Order {
                                        expr: m,
                                        desc: o.desc,
                                        nulls: o.nulls_first.map(|f| if f { Nulls::First } else { Nulls::Last }),
                                    }),
                                    _ => None,
                                })
                                .collect();
                            ordered.map(|o| (o, n as u64))
                        }
                        _ => None,
                    };
                    self.read_table(schema, and_all(filters), top)?
                }
            };
            if p == 0 {
                rows = part_rows;
                continue;
            }
            let join = join.expect("joins after the first table");
            rows = self.join(&scope, p, rows, part_rows, join, outer, used)?;
        }
        // The rest of WHERE.
        let residual: Vec<&Expr> = where_parts.iter().zip(&pushed).filter(|(_, p)| !**p).map(|(w, _)| w).collect();
        if !residual.is_empty() {
            let mut kept = Vec::with_capacity(rows.len());
            for r in rows {
                let env = Env { scope: &scope, row: &r, group: None, outer, used_outer: used, output: None };
                let mut ok = true;
                for w in &residual {
                    if tri(&self.eval(w, &env)?) != Some(true) {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    kept.push(r);
                }
            }
            rows = kept;
        }
        // Columns of the result.
        let mut names = Vec::new();
        enum Item<'i> {
            Column(usize),
            Expr(&'i Expr),
        }
        let mut items = Vec::new();
        for (n, item) in core.items.iter().enumerate() {
            match item {
                SelItem::All => {
                    if scope.parts.is_empty() {
                        return Err(syntax("SELECT * needs a FROM"));
                    }
                    for p in &scope.parts {
                        for (c, name) in p.columns.iter().enumerate() {
                            names.push((name.clone(), Some(p.alias.clone())));
                            items.push(Item::Column(p.offset + c));
                        }
                    }
                }
                SelItem::TableAll(t) => {
                    let p = scope
                        .parts
                        .iter()
                        .find(|p| p.alias.eq_ignore_ascii_case(t))
                        .ok_or_else(|| syntax(format!("no table `{t}` in FROM")))?;
                    for (c, name) in p.columns.iter().enumerate() {
                        names.push((name.clone(), Some(p.alias.clone())));
                        items.push(Item::Column(p.offset + c));
                    }
                }
                SelItem::Expr(e, alias) => {
                    let table = match e {
                        Expr::Col { table, .. } => table.clone(),
                        _ => None,
                    };
                    names.push((alias.clone().unwrap_or_else(|| header(e, n)), table));
                    items.push(Item::Expr(e));
                }
            }
        }
        let columns = unique_headers(names);
        // Groups: each with the row its plain columns are read from.
        let null_row = vec![Value::Null; scope.width];
        let groups: Vec<(Vec<Value>, Vec<Vec<Value>>)> = if grouped {
            let mut index: HashMap<String, usize> = HashMap::new();
            let mut groups: Vec<(Vec<Value>, Vec<Vec<Value>>)> = Vec::new();
            for r in rows {
                let mut k = String::new();
                {
                    let env = Env { scope: &scope, row: &r, group: None, outer, used_outer: used, output: None };
                    for g in &core.group_by {
                        v::key(&self.eval(g, &env)?, &mut k);
                    }
                }
                match index.get(&k) {
                    Some(&i) => groups[i].1.push(r),
                    None => {
                        index.insert(k, groups.len());
                        groups.push((r.clone(), vec![r]));
                    }
                }
            }
            if groups.is_empty() && core.group_by.is_empty() {
                groups.push((null_row.clone(), Vec::new()));
            }
            groups
        } else {
            rows.into_iter().map(|r| (r, Vec::new())).collect()
        };
        // Projection, HAVING and the sort keys.
        let mut produced: Vec<(Vec<Value>, Vec<Value>)> = Vec::with_capacity(groups.len());
        for (row, members) in &groups {
            let env = Env {
                scope: &scope,
                row,
                group: grouped.then_some(members.as_slice()),
                outer,
                used_outer: used,
                output: None,
            };
            if let Some(h) = &core.having
                && tri(&self.eval(h, &env)?) != Some(true)
            {
                continue;
            }
            let mut out = Vec::with_capacity(items.len());
            for item in &items {
                out.push(match item {
                    Item::Column(i) => row.get(*i).cloned().unwrap_or(Value::Null),
                    Item::Expr(e) => self.eval(e, &env)?,
                });
            }
            let mut keys = Vec::with_capacity(order.len());
            for o in order {
                keys.push(match &o.expr {
                    Expr::Lit(Value::I64(n)) => {
                        let i = (*n as usize)
                            .checked_sub(1)
                            .filter(|i| *i < out.len())
                            .ok_or_else(|| syntax(format!("ORDER BY {n}: there are {} columns", out.len())))?;
                        out[i].clone()
                    }
                    e => {
                        // An output column's name first (`ORDER BY total`), then the sources.
                        let by_alias = match e {
                            Expr::Col { table: None, name } => core.items.iter().zip(&columns).find_map(|(it, c)| {
                                matches!(it, SelItem::Expr(_, Some(a)) if a.eq_ignore_ascii_case(name)).then_some(c)
                            }),
                            _ => None,
                        };
                        match by_alias.and_then(|c| columns.iter().position(|x| x == c)) {
                            Some(i) => out[i].clone(),
                            None => {
                                let env = Env {
                                    output: Some((&columns, &out)),
                                    ..Env {
                                        scope: &scope,
                                        row,
                                        group: grouped.then_some(members.as_slice()),
                                        outer,
                                        used_outer: used,
                                        output: None,
                                    }
                                };
                                self.eval(e, &env)?
                            }
                        }
                    }
                });
            }
            produced.push((out, keys));
        }
        if core.distinct {
            let mut seen = std::collections::HashSet::new();
            produced.retain(|(r, _)| {
                let mut k = String::new();
                r.iter().for_each(|x| v::key(x, &mut k));
                seen.insert(k)
            });
        }
        if !order.is_empty() {
            produced.sort_by(|(_, a), (_, b)| {
                order
                    .iter()
                    .enumerate()
                    .map(|(i, o)| order_cmp(&a[i], &b[i], o.desc, o.nulls_first))
                    .find(|o| !o.is_eq())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        Ok(Rows { columns, rows: produced.into_iter().map(|(r, _)| r).collect() })
    }

    /// `rows` (the tables before `p`) joined with `right` (table `p`).
    #[allow(clippy::too_many_arguments)]
    fn join(
        &self,
        scope: &Scope,
        p: usize,
        left: Vec<Vec<Value>>,
        right: Vec<Vec<Value>>,
        join: &Join,
        outer: Option<&Env<'_>>,
        used: &Cell<bool>,
    ) -> Result<Vec<Vec<Value>>> {
        let width = scope.parts[p].columns.len();
        let mut out = Vec::new();
        let on = match (&join.kind, &join.on) {
            (JoinKind::Cross, _) | (_, None) => None,
            (_, Some(on)) => Some(on),
        };
        let Some(on) = on else {
            for l in &left {
                for r in &right {
                    out.push(l.iter().chain(r).cloned().collect());
                }
            }
            return Ok(out);
        };
        // A hash join on `right.col = <expression of the left tables>`.
        let mut ons = Vec::new();
        conj(on, &mut ons);
        let part = &scope.parts[p];
        let local = |e: &Expr| -> Option<usize> {
            if let Expr::Col { table, name } = e
                && let Ok(Some((q, c))) = scope.find(table.as_deref(), name)
                && q == p
            {
                return Some(c);
            }
            None
        };
        let refs_only_left = |e: &Expr| -> bool {
            let mut ok = true;
            e.walk(&mut |x| {
                if let Expr::Col { table, name } = x {
                    match scope.find(table.as_deref(), name) {
                        Ok(Some((q, _))) => ok &= q < p,
                        _ => ok = false,
                    }
                }
                if matches!(x, Expr::Exists(..) | Expr::Scalar(_) | Expr::InSelect(..)) || x.is_aggregate() {
                    ok = false;
                }
            });
            ok
        };
        let equi = ons.iter().find_map(|o| match o {
            Expr::Bin(Op::Eq, a, b) => match (local(a), local(b)) {
                (Some(c), None) if refs_only_left(b) => Some((c, (**b).clone())),
                (None, Some(c)) if refs_only_left(a) => Some((c, (**a).clone())),
                _ => None,
            },
            _ => None,
        });
        let nulls = vec![Value::Null; width];
        let matches = |l: &[Value], r: &[Value]| -> Result<bool> {
            let row: Vec<Value> = l.iter().chain(r).cloned().collect();
            let env = Env { scope, row: &row, group: None, outer, used_outer: used, output: None };
            Ok(tri(&self.eval(on, &env)?) == Some(true))
        };
        match equi {
            Some((c, left_key)) => {
                let mut table: HashMap<String, Vec<usize>> = HashMap::new();
                for (i, r) in right.iter().enumerate() {
                    if r[c].is_null() {
                        continue;
                    }
                    let mut k = String::new();
                    v::key(&r[c], &mut k);
                    table.entry(k).or_default().push(i);
                }
                for l in &left {
                    let env = Env { scope, row: l, group: None, outer, used_outer: used, output: None };
                    let lk = self.eval(&left_key, &env)?;
                    let mut hit = false;
                    if !lk.is_null() {
                        // Keys compare as SQL does: an integer column and a text holding it.
                        let lk = part.kinds[c].and_then(|k| cast(&lk, k).ok()).unwrap_or(lk);
                        let mut k = String::new();
                        v::key(&lk, &mut k);
                        for &i in table.get(&k).map(Vec::as_slice).unwrap_or_default() {
                            if matches(l, &right[i])? {
                                out.push(l.iter().chain(&right[i]).cloned().collect());
                                hit = true;
                            }
                        }
                    }
                    if !hit && join.kind == JoinKind::Left {
                        out.push(l.iter().chain(&nulls).cloned().collect());
                    }
                }
            }
            None => {
                for l in &left {
                    let mut hit = false;
                    for r in &right {
                        if matches(l, r)? {
                            out.push(l.iter().chain(r).cloned().collect());
                            hit = true;
                        }
                    }
                    if !hit && join.kind == JoinKind::Left {
                        out.push(l.iter().chain(&nulls).cloned().collect());
                    }
                }
            }
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Statements
    // -----------------------------------------------------------------------

    pub(crate) fn run(&self, stmt: &Stmt) -> Result<SqlResult> {
        match stmt {
            Stmt::Select(sel) => {
                let rows = self.select(sel, None, &Cell::new(false))?;
                Ok(SqlResult { columns: rows.columns, rows: rows.rows, returns_rows: true, ..SqlResult::default() })
            }
            Stmt::Insert { table, columns, source, on_conflict, replace, returning } => {
                self.insert(table, columns.as_deref(), source, on_conflict.as_ref(), *replace, returning.as_deref())
            }
            Stmt::Update { table, alias, sets, filter, returning } => {
                self.update(table, alias.as_deref(), sets, filter.as_ref(), returning.as_deref())
            }
            Stmt::Delete { table, alias, filter, returning } => {
                self.delete(table, alias.as_deref(), filter.as_ref(), returning.as_deref())
            }
            Stmt::CreateTable { name, if_not_exists, columns, indexes, as_select } => {
                self.create_table(name, *if_not_exists, columns, indexes, as_select.as_deref())
            }
            Stmt::CreateIndex { name, table, columns, unique, if_not_exists } => {
                check_name(name)?;
                let schema = self.schema(table)?;
                let mut cols = Vec::new();
                for c in columns {
                    let col = schema
                        .columns
                        .iter()
                        .find(|x| x.name.eq_ignore_ascii_case(c))
                        .ok_or_else(|| syntax(format!("`{}` has no column `{c}`", schema.name)))?;
                    cols.push(col.name.to_owned());
                }
                self.tx.alter(&Alter::AddIndex {
                    table: schema.name.to_owned(),
                    name: name.clone(),
                    columns: cols,
                    unique: *unique,
                    if_not_exists: *if_not_exists,
                })?;
                Ok(SqlResult::default())
            }
            Stmt::DropIndex { name, if_exists } => {
                self.tx.alter(&Alter::DropIndex { name: name.clone(), if_exists: *if_exists })?;
                Ok(SqlResult::default())
            }
            Stmt::AddColumn { table, column } => {
                let schema = self.schema(table)?;
                if column.not_null && column.default.is_none() {
                    return Err(syntax(format!(
                        "a NOT NULL column added to `{}` needs a DEFAULT for the rows already there",
                        schema.name
                    )));
                }
                let fill = match &column.default {
                    Some(d) => cast(&self.eval_const(&parse_expr(d)?)?, column_type(column.kind))?,
                    None => Value::Null,
                };
                self.tx.alter(&Alter::AddColumn {
                    table: schema.name.to_owned(),
                    column: column_json(column),
                    fill,
                })?;
                if column.unique || column.indexed {
                    self.tx.alter(&Alter::AddIndex {
                        table: schema.name.to_owned(),
                        name: format!("{}_{}_{}", schema.name, column.name, if column.unique { "key" } else { "idx" }),
                        columns: vec![column.name.clone()],
                        unique: column.unique,
                        if_not_exists: false,
                    })?;
                }
                Ok(SqlResult::default())
            }
            Stmt::DropTable { names, if_exists } => {
                for n in names {
                    let real = self
                        .tx
                        .table_names()?
                        .into_iter()
                        .find(|t| t.eq_ignore_ascii_case(n))
                        .unwrap_or_else(|| n.clone());
                    self.tx.execute_now(&Query::DropTable { name: &real, if_exists: *if_exists })?;
                }
                Ok(SqlResult::default())
            }
            Stmt::Transaction(_) => Ok(SqlResult::default()),
            Stmt::Explain(inner) => {
                let plan = match &**inner {
                    Stmt::Select(sel) => {
                        let used = Cell::new(false);
                        match self.as_memoire(sel, None, &used) {
                            Some(_) => "one engine statement: indexes for the filter, early stop for ORDER BY ... LIMIT".to_owned(),
                            None => "tables read with their indexable filters, then joined, filtered, grouped and sorted by the SQL layer".to_owned(),
                        }
                    }
                    other if other.writes() => "a change, in the request's transaction".to_owned(),
                    _ => "nothing to read".to_owned(),
                };
                Ok(SqlResult {
                    columns: vec!["plan".into()],
                    rows: vec![vec![Value::Text(plan)]],
                    returns_rows: true,
                    ..SqlResult::default()
                })
            }
        }
    }

    /// A value with no row: `VALUES`, `DEFAULT`, `LIMIT`.
    fn eval_const(&self, e: &Expr) -> Result<Value> {
        let (scope, used) = (Scope::default(), Cell::new(false));
        self.eval(e, &Env { scope: &scope, row: &[], group: None, outer: None, used_outer: &used, output: None })
    }

    /// Rows of `RETURNING`.
    fn returning(&self, items: &[SelItem], scope: &Scope, rows: &[Vec<Value>]) -> Result<SqlResult> {
        let sel = Core {
            distinct: false,
            items: items.to_vec(),
            from: None,
            joins: Vec::new(),
            filter: None,
            group_by: Vec::new(),
            having: None,
        };
        let mut names = Vec::new();
        let mut columns_of = Vec::new();
        for (n, item) in sel.items.iter().enumerate() {
            match item {
                SelItem::All | SelItem::TableAll(_) => {
                    for (c, name) in scope.parts[0].columns.iter().enumerate() {
                        names.push((name.clone(), None));
                        columns_of.push(Err(c));
                    }
                }
                SelItem::Expr(e, alias) => {
                    names.push((alias.clone().unwrap_or_else(|| header(e, n)), None));
                    columns_of.push(Ok(e));
                }
            }
        }
        let mut out = Vec::with_capacity(rows.len());
        let used = Cell::new(false);
        for r in rows {
            let env = Env { scope, row: r, group: None, outer: None, used_outer: &used, output: None };
            let mut row = Vec::with_capacity(columns_of.len());
            for c in &columns_of {
                row.push(match c {
                    Err(i) => r[*i].clone(),
                    Ok(e) => self.eval(e, &env)?,
                });
            }
            out.push(row);
        }
        Ok(SqlResult { columns: unique_headers(names), rows: out, returns_rows: true, ..SqlResult::default() })
    }

    /// The rows of `schema` matching `filter`, and the scope they live in.
    fn target_rows(
        &self,
        schema: &'static TableSchema,
        alias: Option<&str>,
        filter: Option<&Expr>,
    ) -> Result<(Scope, Vec<Vec<Value>>, bool)> {
        let scope = Scope::of_schema(alias.unwrap_or(schema.name), schema);
        let used = Cell::new(false);
        let mut parts = Vec::new();
        if let Some(f) = filter {
            conj(f, &mut parts);
        }
        let mut pushed = Vec::new();
        let mut residual = Vec::new();
        for p in &parts {
            match self.to_m(p, &scope, Some(0), None, &used) {
                Some(m) => pushed.push(m),
                None => residual.push(p),
            }
        }
        let all_pushed = residual.is_empty();
        let mut rows = self.read_table(schema, and_all(pushed), None)?;
        if !residual.is_empty() {
            let mut kept = Vec::new();
            for r in rows {
                let env = Env { scope: &scope, row: &r, group: None, outer: None, used_outer: &used, output: None };
                let mut ok = true;
                for w in &residual {
                    if tri(&self.eval(w, &env)?) != Some(true) {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    kept.push(r);
                }
            }
            rows = kept;
        }
        Ok((scope, rows, all_pushed))
    }

    fn update(
        &self,
        table: &str,
        alias: Option<&str>,
        sets: &[(String, Expr)],
        filter: Option<&Expr>,
        returning: Option<&[SelItem]>,
    ) -> Result<SqlResult> {
        let schema = self.schema(table)?;
        let mut targets = Vec::new();
        for (c, e) in sets {
            let i = schema
                .columns
                .iter()
                .position(|x| x.name.eq_ignore_ascii_case(c))
                .ok_or_else(|| syntax(format!("`{}` has no column `{c}`", schema.name)))?;
            if i == schema.primary_key {
                return Err(Error::Unsupported(
                    "changing a primary key is not supported: delete the row and insert it again".into(),
                ));
            }
            targets.push((i, e));
        }
        // Constant values and a filter Memoire runs: one engine statement.
        let used = Cell::new(false);
        let scope = Scope::of_schema(alias.unwrap_or(schema.name), schema);
        if returning.is_none() {
            let consts: Option<Vec<(usize, M)>> = targets
                .iter()
                .map(|(i, e)| {
                    let value = match e {
                        Expr::Lit(x) => x.clone(),
                        Expr::Param(p) => self.param(*p).ok()?,
                        _ => return None,
                    };
                    let value = cast(&value, schema.columns[*i].kind).ok()?;
                    if value.is_null() && !schema.columns[*i].nullable {
                        return None;
                    }
                    Some((*i, M::Value(value)))
                })
                .collect();
            let m_filter = match filter {
                Some(f) => self.to_m(f, &scope, Some(0), None, &used).map(Some),
                None => Some(None),
            };
            if let (Some(sets), Some(filter)) = (consts, m_filter) {
                let r = self.tx.execute_now(&Query::Update(&UpdateAst { table: schema, sets, filter }))?;
                return Ok(SqlResult { rows_affected: r.rows_affected, ..SqlResult::default() });
            }
        }
        let (scope, rows, _) = self.target_rows(schema, alias, filter)?;
        let mut after = Vec::with_capacity(rows.len());
        for old in &rows {
            let env = Env { scope: &scope, row: old, group: None, outer: None, used_outer: &used, output: None };
            let mut new = old.clone();
            for (i, e) in &targets {
                let col = &schema.columns[*i];
                let value = cast(&self.eval(e, &env)?, col.kind)
                    .map_err(|err| syntax(format!("column `{}`: {err}", col.name)))?;
                if value.is_null() && !col.nullable {
                    return Err(syntax(format!("column `{}` cannot be NULL", col.name)));
                }
                new[*i] = value;
            }
            if new != *old {
                self.tx.execute_now(&Query::UpdateRow { table: schema, values: &new })?;
            }
            after.push(new);
        }
        let mut result = match returning {
            Some(items) => self.returning(items, &scope, &after)?,
            None => SqlResult::default(),
        };
        result.rows_affected = rows.len() as u64;
        Ok(result)
    }

    fn delete(
        &self,
        table: &str,
        alias: Option<&str>,
        filter: Option<&Expr>,
        returning: Option<&[SelItem]>,
    ) -> Result<SqlResult> {
        let schema = self.schema(table)?;
        if returning.is_none() {
            let used = Cell::new(false);
            let scope = Scope::of_schema(alias.unwrap_or(schema.name), schema);
            let m_filter = match filter {
                Some(f) => self.to_m(f, &scope, Some(0), None, &used).map(Some),
                None => Some(None),
            };
            if let Some(filter) = m_filter {
                let r = self.tx.execute_now(&Query::Delete(&DeleteAst { table: schema, filter }))?;
                return Ok(SqlResult { rows_affected: r.rows_affected, ..SqlResult::default() });
            }
        }
        let (scope, rows, _) = self.target_rows(schema, alias, filter)?;
        for r in &rows {
            self.tx.execute_now(&Query::DeleteByKey { table: schema, key: &r[schema.primary_key] })?;
        }
        let mut result = match returning {
            Some(items) => self.returning(items, &scope, &rows)?,
            None => SqlResult::default(),
        };
        result.rows_affected = rows.len() as u64;
        Ok(result)
    }

    /// A full row for `schema` from the given columns' values: defaults,
    /// NULLs and generated keys for the others, every value of its column's type.
    fn full_row(&self, schema: &'static TableSchema, given: &[(usize, Value)]) -> Result<Vec<Value>> {
        let mut row = vec![None; schema.columns.len()];
        for (i, value) in given {
            let col = &schema.columns[*i];
            row[*i] = Some(cast(value, col.kind).map_err(|err| syntax(format!("column `{}`: {err}", col.name)))?);
        }
        let mut out = Vec::with_capacity(row.len());
        for (i, (value, col)) in row.into_iter().zip(schema.columns).enumerate() {
            let value = match value {
                Some(v) => v,
                None => match col.default {
                    Some(d) => cast(&self.eval_const(&parse_expr(d)?)?, col.kind)?,
                    None => Value::Null,
                },
            };
            if value.is_null() && !col.nullable && !(col.auto_increment && i == schema.primary_key) {
                return Err(syntax(format!("column `{}` of `{}` needs a value", col.name, schema.name)));
            }
            out.push(value);
        }
        Ok(out)
    }

    /// The existing row a new one collides with on `keys` (column sets:
    /// the primary key, unique columns and indexes).
    fn colliding(
        &self,
        schema: &'static TableSchema,
        keys: &[Vec<usize>],
        row: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        for key in keys {
            if key.iter().any(|&i| row[i].is_null()) {
                continue;
            }
            let filter = key
                .iter()
                .map(|&i| {
                    let c = &schema.columns[i];
                    M::binary(
                        BinOp::Eq,
                        M::Column(ColumnRef { table: schema.name, name: c.name, index: i, kind: c.kind }),
                        M::Value(row[i].clone()),
                    )
                })
                .reduce(|a, b| M::binary(BinOp::And, a, b));
            let found = self.read_table(schema, filter, None)?;
            if let Some(r) = found.into_iter().next() {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    fn insert(
        &self,
        table: &str,
        columns: Option<&[String]>,
        source: &InsertSource,
        on_conflict: Option<&(Vec<String>, Conflict)>,
        replace: bool,
        returning: Option<&[SelItem]>,
    ) -> Result<SqlResult> {
        let schema = self.schema(table)?;
        let positions: Vec<usize> = match columns {
            Some(cols) => cols
                .iter()
                .map(|c| {
                    schema
                        .columns
                        .iter()
                        .position(|x| x.name.eq_ignore_ascii_case(c))
                        .ok_or_else(|| syntax(format!("`{}` has no column `{c}`", schema.name)))
                })
                .collect::<Result<_>>()?,
            None => (0..schema.columns.len()).collect(),
        };
        // The new rows' values.
        let source_rows: Vec<Vec<Value>> = match source {
            InsertSource::Values(rows) => rows
                .iter()
                .map(|r| r.iter().map(|e| self.eval_const(e)).collect::<Result<Vec<_>>>())
                .collect::<Result<_>>()?,
            InsertSource::Select(sel) => self.select(sel, None, &Cell::new(false))?.rows,
            InsertSource::Defaults => vec![Vec::new()],
        };
        let mut rows = Vec::with_capacity(source_rows.len());
        for r in source_rows {
            let n = r.len();
            // `INSERT INTO t VALUES (...)` may leave out generated keys at the end.
            let fits = n == positions.len()
                || matches!(source, InsertSource::Defaults)
                || (columns.is_none()
                    && n < positions.len()
                    && positions[n..].iter().all(|&i| schema.columns[i].auto_increment));
            if !fits {
                return Err(syntax(format!("{} value(s) for {} column(s)", n, positions.len())));
            }
            let given: Vec<(usize, Value)> = positions.iter().copied().zip(r).collect();
            rows.push(self.full_row(schema, &given)?);
        }
        let pk = schema.primary_key;
        let scope = Scope::of_schema(schema.name, schema);
        // Plain inserts: one engine statement (keys given for an
        // auto-increment column go one by one, below).
        let explicit_keys = schema.columns[pk].auto_increment && rows.iter().any(|r| !r[pk].is_null());
        if on_conflict.is_none() && !replace && returning.is_none() && !explicit_keys {
            let r: ExecResult = if rows.len() == 1 {
                self.tx.execute_now(&Query::InsertRow { table: schema, values: &rows[0] })?
            } else {
                self.tx
                    .execute_now(&Query::Insert(&memoire_orm::ast::InsertAst { table: schema, rows: rows.clone() }))?
            };
            let last = r.last_insert_id.or_else(|| rows.last().map(|r| r[pk].clone()).filter(|v| !v.is_null()));
            return Ok(SqlResult {
                rows_affected: r.rows_affected.max(rows.len() as u64),
                last_insert_id: last,
                ..SqlResult::default()
            });
        }
        // Conflict targets: the given columns, or every unique key.
        let keys: Vec<Vec<usize>> = match on_conflict {
            Some((target, _)) if !target.is_empty() => vec![
                target
                    .iter()
                    .map(|c| {
                        schema
                            .columns
                            .iter()
                            .position(|x| x.name.eq_ignore_ascii_case(c))
                            .ok_or_else(|| syntax(format!("`{}` has no column `{c}`", schema.name)))
                    })
                    .collect::<Result<_>>()?,
            ],
            _ => {
                let mut k = vec![vec![pk]];
                k.extend(schema.columns.iter().enumerate().filter(|(i, c)| *i != pk && c.unique).map(|(i, _)| vec![i]));
                k.extend(schema.indexes.iter().filter(|ix| ix.unique).map(|ix| ix.columns.to_vec()));
                k
            }
        };
        let mut done = Vec::new();
        let mut affected = 0u64;
        let mut last = None;
        let mut excluded_scope = scope.clone();
        excluded_scope.push("excluded".into(), scope.parts[0].columns.clone(), scope.parts[0].kinds.clone(), None);
        for row in rows {
            let existing = if on_conflict.is_some() || replace { self.colliding(schema, &keys, &row)? } else { None };
            match (existing, on_conflict.map(|(_, a)| a)) {
                (Some(old), Some(Conflict::Nothing)) => {
                    let _ = old;
                }
                (Some(old), Some(Conflict::Update(sets, filter))) => {
                    let both: Vec<Value> = old.iter().chain(&row).cloned().collect();
                    let used = Cell::new(false);
                    let env = Env {
                        scope: &excluded_scope,
                        row: &both,
                        group: None,
                        outer: None,
                        used_outer: &used,
                        output: None,
                    };
                    if let Some(f) = filter
                        && tri(&self.eval(f, &env)?) != Some(true)
                    {
                        continue;
                    }
                    let mut new = old.clone();
                    for (c, e) in sets {
                        let i = schema
                            .columns
                            .iter()
                            .position(|x| x.name.eq_ignore_ascii_case(c))
                            .ok_or_else(|| syntax(format!("`{}` has no column `{c}`", schema.name)))?;
                        if i == pk {
                            return Err(Error::Unsupported("ON CONFLICT cannot change the primary key".into()));
                        }
                        new[i] = cast(&self.eval(e, &env)?, schema.columns[i].kind)?;
                    }
                    self.tx.execute_now(&Query::UpdateRow { table: schema, values: &new })?;
                    affected += 1;
                    done.push(new);
                }
                (Some(old), None) => {
                    // REPLACE: the old row goes, the new one takes its place.
                    self.tx.execute_now(&Query::DeleteByKey { table: schema, key: &old[pk] })?;
                    let id = self.put_new(schema, &row)?;
                    last = Some(id.clone());
                    affected += 1;
                    done.push(self.by_key(schema, &id)?.unwrap_or(row));
                }
                (None, _) => {
                    let id = self.put_new(schema, &row)?;
                    last = Some(id.clone());
                    affected += 1;
                    if returning.is_some() {
                        done.push(self.by_key(schema, &id)?.unwrap_or(row));
                    }
                }
            }
        }
        let mut result = match returning {
            Some(items) => self.returning(items, &scope, &done)?,
            None => SqlResult::default(),
        };
        result.rows_affected = affected;
        result.last_insert_id = last;
        Ok(result)
    }

    /// Inserts one row; returns its key. A key given for an auto-increment
    /// column is kept (Memoire generates one otherwise), unless taken.
    fn put_new(&self, schema: &'static TableSchema, row: &[Value]) -> Result<Value> {
        let pk = schema.primary_key;
        if schema.columns[pk].auto_increment && !row[pk].is_null() {
            if self.by_key(schema, &row[pk])?.is_some() {
                return Err(Error::UniqueViolation {
                    table: schema.name.to_owned(),
                    column: schema.columns[pk].name.to_owned(),
                });
            }
            self.tx.execute_now(&Query::UpsertRow { table: schema, values: row })?;
            return Ok(row[pk].clone());
        }
        let r = self.tx.execute_now(&Query::InsertRow { table: schema, values: row })?;
        Ok(r.last_insert_id.unwrap_or_else(|| row[pk].clone()))
    }

    fn by_key(&self, schema: &'static TableSchema, key: &Value) -> Result<Option<Vec<Value>>> {
        let c = &schema.columns[schema.primary_key];
        let filter = M::binary(
            BinOp::Eq,
            M::Column(ColumnRef { table: schema.name, name: c.name, index: schema.primary_key, kind: c.kind }),
            M::Value(cast(key, c.kind)?),
        );
        Ok(self.read_table(schema, Some(filter), None)?.into_iter().next())
    }

    fn create_table(
        &self,
        name: &str,
        if_not_exists: bool,
        columns: &[ColDef],
        indexes: &[(Vec<String>, bool)],
        as_select: Option<&Select>,
    ) -> Result<SqlResult> {
        if self.tx.table_names()?.iter().any(|t| t.eq_ignore_ascii_case(name)) {
            return if if_not_exists { Ok(SqlResult::default()) } else { Err(Error::TableExists(name.to_owned())) };
        }
        let mut cols = columns.to_vec();
        let mut rows = None;
        if let Some(sel) = as_select {
            let r = self.select(sel, None, &Cell::new(false))?;
            for (i, c) in r.columns.iter().enumerate() {
                let kind = r
                    .rows
                    .iter()
                    .map(|row| &row[i])
                    .find(|x| !x.is_null())
                    .and_then(v::kind_of)
                    .unwrap_or(ColumnType::Text);
                let wire = match kind {
                    ColumnType::Bool => "bool",
                    ColumnType::SmallInt | ColumnType::Int => "int",
                    ColumnType::BigInt => "bigint",
                    ColumnType::Real | ColumnType::Double => "double",
                    ColumnType::Bytes => "bytes",
                    ColumnType::Uuid => "uuid",
                    ColumnType::Timestamp => "timestamp",
                    ColumnType::TimestampTz => "timestamptz",
                    ColumnType::Date => "date",
                    ColumnType::Time => "time",
                    ColumnType::Json => "json",
                    _ => "text",
                };
                let name = c.rsplit('.').next().unwrap_or(c).to_owned();
                check_name(&name)?;
                cols.push(ColDef {
                    name,
                    kind: wire,
                    primary_key: false,
                    auto_increment: false,
                    not_null: false,
                    unique: false,
                    indexed: false,
                    default: None,
                    references: None,
                });
            }
            if let Some(id) = cols.iter_mut().find(|c| c.name.eq_ignore_ascii_case("id")) {
                id.primary_key = true;
            }
            rows = Some(r.rows);
        }
        let mut seen = std::collections::HashSet::new();
        for c in &cols {
            if !seen.insert(c.name.to_ascii_lowercase()) {
                return Err(syntax(format!("column `{}` is declared twice", c.name)));
            }
        }
        // Memoire's tables have a primary key: one is added when none is declared.
        let pk = match cols.iter().position(|c| c.primary_key) {
            Some(p) => p,
            None => {
                cols.push(ColDef {
                    name: "rowid".into(),
                    kind: "bigint",
                    primary_key: true,
                    auto_increment: true,
                    not_null: true,
                    unique: false,
                    indexed: false,
                    default: None,
                    references: None,
                });
                cols.len() - 1
            }
        };
        if cols[pk].auto_increment && !matches!(cols[pk].kind, "int" | "bigint" | "smallint") {
            return Err(syntax(format!("AUTOINCREMENT needs an integer column, not `{}`", cols[pk].name)));
        }
        // An integer primary key with no value given gets the next one, as in SQLite.
        if matches!(cols[pk].kind, "int" | "bigint") {
            cols[pk].auto_increment = true;
        }
        let ix: Vec<Json> = indexes
            .iter()
            .map(|(names, unique)| {
                let ids: Vec<usize> = names
                    .iter()
                    .map(|n| cols.iter().position(|c| c.name.eq_ignore_ascii_case(n)).ok_or_else(|| syntax(format!("no column `{n}`"))))
                    .collect::<Result<_>>()?;
                Ok(json!({ "name": format!("{name}_{}_{}", names.join("_"), if *unique { "key" } else { "idx" }), "columns": ids, "unique": unique }))
            })
            .collect::<Result<_>>()?;
        let schema_json = json!({ "name": name, "pk": pk, "columns": cols.iter().map(column_json).collect::<Vec<_>>(), "indexes": ix });
        let schema = self.schemas.schema(&schema_json)?;
        self.tx.execute_now(&Query::CreateTable { table: schema, if_not_exists })?;
        let mut affected = 0;
        if let Some(rows) = rows {
            let positions: Vec<usize> = (0..rows.first().map_or(0, Vec::len)).collect();
            for r in rows {
                let given: Vec<(usize, Value)> = positions.iter().copied().zip(r).collect();
                let full = self.full_row(schema, &given)?;
                self.put_new(schema, &full)?;
                affected += 1;
            }
        }
        Ok(SqlResult { rows_affected: affected, ..SqlResult::default() })
    }
}

/// A column as the wire protocol writes it in a schema.
fn column_json(c: &ColDef) -> Json {
    json!({
        "name": c.name, "field": c.name, "kind": c.kind,
        "nullable": !(c.not_null || c.primary_key), "primaryKey": c.primary_key, "autoIncrement": c.auto_increment,
        "unique": c.unique, "indexed": c.indexed, "fulltext": false,
        "sqlType": null, "default": c.default,
        "references": c.references.as_ref().map(|(t, col)| json!({ "table": t, "column": col, "onDelete": null, "onUpdate": null })),
    })
}

fn in_values(v: &Value, values: &[Value], negated: bool) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let mut saw_null = false;
    for x in values {
        if x.is_null() {
            saw_null = true;
        } else if compare(v, x).is_some_and(|o| o.is_eq()) {
            return Value::Bool(!negated);
        }
    }
    if saw_null { Value::Null } else { Value::Bool(negated) }
}

/// A value of a column's type, for Memoire (`None`: it is not one).
fn coerce(x: &Value, kind: ColumnType) -> Option<Value> {
    if x.is_null() {
        return Some(Value::Null);
    }
    let c = cast(x, kind).ok()?;
    // A float that is not an integer compared with an integer column: not
    // the same comparison once truncated.
    if matches!(kind, ColumnType::SmallInt | ColumnType::Int | ColumnType::BigInt)
        && matches!(x, Value::F64(f) if f.fract() != 0.0)
    {
        return None;
    }
    if matches!(
        kind,
        ColumnType::SmallInt | ColumnType::Int | ColumnType::BigInt | ColumnType::Real | ColumnType::Double
    ) && matches!(x, Value::Text(_))
    {
        return None;
    }
    Some(c)
}

fn same_family(a: ColumnType, b: ColumnType) -> bool {
    let num = |k: ColumnType| {
        matches!(k, ColumnType::SmallInt | ColumnType::Int | ColumnType::BigInt | ColumnType::Real | ColumnType::Double)
    };
    a == b || (num(a) && num(b))
}
