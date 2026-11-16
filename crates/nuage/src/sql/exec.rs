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
use super::limits::{Hold, Limits, row_bytes, rows_bytes, text_fits, value_bytes};
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

/// Rows and their column names, counted in the memory of the statement for
/// as long as they are kept.
struct Rows {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    hold: Hold,
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
    /// The end of the row when it is in two pieces: a join tries a row of the
    /// tables before with a row of the next one, without copying either.
    tail: &'e [Value],
    group: Option<&'e [Vec<Value>]>,
    outer: Option<&'e Env<'e>>,
    used_outer: &'e Cell<bool>,
    output: Option<(&'e [String], &'e [Value])>,
}

impl Env<'_> {
    fn lookup(&self, table: Option<&str>, name: &str) -> Result<Value> {
        if let Some((p, c)) = self.scope.find(table, name)? {
            let i = self.scope.parts[p].offset + c;
            let held = if i < self.row.len() { self.row.get(i) } else { self.tail.get(i - self.row.len()) };
            return Ok(held.cloned().unwrap_or(Value::Null));
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
    /// Results of the statement's subqueries that read nothing of the
    /// enclosing row, by the number of their `SELECT` ([`Select::id`]). Never
    /// by where the `SELECT` is in memory: a copy of it is elsewhere, and
    /// another one may come to the same place.
    cache: RefCell<HashMap<u64, Rc<Rows>>>,
    /// The memory and the time the statements may take.
    limits: Rc<Limits>,
    /// Levels of [`Runner::to_m`] at work (see [`M_DEPTH`]).
    m_depth: Cell<usize>,
}

/// How deep a condition handed to Memoire may be. Its evaluator calls itself
/// once per level with larger frames than this module's: a deeper condition
/// is evaluated here.
const M_DEPTH: usize = 24;

/// The conditions `e` is the `AND` of.
fn conj<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::Chain(first, rest) if matches!(rest.first(), Some((Op::And, _))) => {
            conj(first, out);
            rest.iter().for_each(|(_, x)| conj(x, out));
        }
        other => out.push(other),
    }
}

/// `parts` joined by `op` as a tree no deeper than the logarithm of their
/// number, in their order. Memoire evaluates (and copies, and drops) a
/// condition by calling itself on each side: a long list joined one after
/// the other would be as deep as it is long.
fn balanced(op: BinOp, mut parts: Vec<M>) -> Option<M> {
    while parts.len() > 1 {
        let mut pairs = Vec::with_capacity(parts.len().div_ceil(2));
        let mut it = parts.into_iter();
        while let Some(a) = it.next() {
            pairs.push(match it.next() {
                Some(b) => M::binary(op, a, b),
                None => a,
            });
        }
        parts = pairs;
    }
    parts.pop()
}

fn and_all(parts: Vec<M>) -> Option<M> {
    balanced(BinOp::And, parts)
}

/// `column = v1 OR column = v2 OR ...` (each operand an equality of the same
/// column with a value that is not NULL) as `column IN (v1, v2, ...)`: the
/// same rows, found through the column's index instead of a read of the
/// whole table. `None` when the operands are not all of that form.
fn one_column_among(parts: &[M]) -> Option<M> {
    let mut column: Option<ColumnRef> = None;
    let mut list = Vec::with_capacity(parts.len());
    for part in parts {
        let M::Binary { op: BinOp::Eq, left, right } = part else { return None };
        let (c, value) = match (&**left, &**right) {
            (M::Column(c), M::Value(x)) | (M::Value(x), M::Column(c)) => (c, x),
            _ => return None,
        };
        if value.is_null() || column.is_some_and(|k| k.table != c.table || k.index != c.index) {
            return None;
        }
        column = Some(*c);
        list.push(value.clone());
    }
    Some(M::InList { expr: Box::new(M::Column(column?)), list, negated: false })
}

/// What a join gives.
struct Joined {
    rows: Vec<Vec<Value>>,
    hold: Hold,
    /// How many rows it gives (the rows themselves are left out when only their number was asked).
    count: usize,
    /// For each condition of `WHERE` it applied, whether it failed on some row (see [`Runner::join`]).
    failed: Vec<bool>,
}

/// What a join is asked for.
#[derive(Clone, Copy)]
struct Wanted {
    /// Only how many rows it gives.
    count: bool,
    /// Only its first rows.
    first: Option<usize>,
}

/// The rows of a group.
type Group = Vec<Vec<Value>>;

/// Rows an `UPDATE` or a `DELETE` reads at once (see [`Runner::each_target`]).
const CHANGE_CHUNK: usize = 1024;

/// What a key of a group, of `DISTINCT` or of a hash join takes beside its text.
const KEY_BYTES: usize = 48;

/// The values a column holds, as a hash join tells them apart.
#[derive(Clone, Copy)]
enum Family {
    /// Integers and floats: SQL compares them by their value, whatever their type.
    Number,
    Text,
    /// Values of one type only (dates, UUIDs...).
    Exact(ColumnType),
}

impl Family {
    /// `None`: no key tells the values apart as SQL does (JSON documents may
    /// be equal and written differently).
    fn of(kind: Option<ColumnType>) -> Option<Family> {
        Some(match kind? {
            ColumnType::SmallInt | ColumnType::Int | ColumnType::BigInt | ColumnType::Real | ColumnType::Double => {
                Family::Number
            }
            ColumnType::Text => Family::Text,
            ColumnType::Json => return None,
            other => Family::Exact(other),
        })
    }
}

/// The key of a value in a hash join: two values that SQL finds equal have
/// the same key (two that do not may too; the join's condition is asked
/// again of each pair).
#[derive(Hash, PartialEq, Eq)]
enum JoinKey {
    Number(u64),
    Text(String),
    Other(String),
}

impl JoinKey {
    /// `Ok(None)`: it equals nothing (NULL, NaN). `Err(())`: it is not of the
    /// family, and is compared with each value instead.
    fn of(v: &Value, family: Family) -> std::result::Result<Option<JoinKey>, ()> {
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(match (family, v) {
            (Family::Number, Value::I32(_) | Value::I64(_) | Value::F64(_)) => {
                let f = v.as_f64().ok_or(())?;
                if f.is_nan() {
                    return Ok(None);
                }
                // `0.0` and `-0.0` are equal.
                JoinKey::Number(if f == 0.0 { 0 } else { f.to_bits() })
            }
            (Family::Text, Value::Text(t)) => JoinKey::Text(t.clone()),
            (Family::Exact(kind), _) if v::kind_of(v) == Some(kind) => {
                let mut k = String::new();
                v::key(v, &mut k);
                JoinKey::Other(k)
            }
            _ => return Err(()),
        }))
    }

    fn bytes(&self) -> usize {
        match self {
            JoinKey::Number(_) => 8,
            JoinKey::Text(t) | JoinKey::Other(t) => t.len(),
        }
    }
}

/// The key of a row among others (`DISTINCT`, `UNION`).
fn row_key(row: &[Value]) -> String {
    let mut k = String::new();
    row.iter().for_each(|x| v::key(x, &mut k));
    k
}

/// `l op r` for Memoire. A value beside a column takes the column's type (an
/// index holds typed values). `None`: Memoire would not compare them as SQL does.
fn m_pair(op: BinOp, l: M, r: M) -> Option<M> {
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
    Some(M::binary(op, l, r))
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

/// A column of `scope` for Memoire (of part `only`, when given), or one of
/// the enclosing row's as a value.
fn m_column(
    table: Option<&str>,
    name: &str,
    scope: &Scope,
    only: Option<usize>,
    outer: Option<&Env<'_>>,
    used: &Cell<bool>,
) -> Option<M> {
    match scope.find(table, name).ok()? {
        Some((p, c)) => {
            if only.is_some_and(|o| o != p) {
                return None;
            }
            let schema = scope.parts[p].schema?;
            let col = &schema.columns[c];
            Some(M::Column(ColumnRef { table: schema.name, name: col.name, index: c, kind: col.kind }))
        }
        None => {
            let v = outer?.lookup(table, name).ok()?;
            used.set(true);
            Some(M::Value(v))
        }
    }
}

impl<'a> Runner<'a> {
    /// A runner with the limits the settings give (see [`super::limits`]).
    pub(crate) fn new(tx: &'a EmbeddedTransaction, schemas: &'a SchemaCache, params: &'a [Value]) -> Self {
        Runner::with_limits(tx, schemas, params, Limits::from_settings())
    }

    /// A runner under `limits`: the scripts of one request share one budget.
    pub(crate) fn with_limits(
        tx: &'a EmbeddedTransaction,
        schemas: &'a SchemaCache,
        params: &'a [Value],
        limits: Rc<Limits>,
    ) -> Self {
        Runner { tx, schemas, params, cache: RefCell::new(HashMap::new()), limits, m_depth: Cell::new(0) }
    }

    /// The schema of a table (its name in any case).
    pub(crate) fn schema(&self, name: &str) -> Result<&'static TableSchema> {
        if let Some(s) = self.tx.static_schema(name, self.schemas)? {
            return Ok(s);
        }
        let real = self.tx.table_names()?.into_iter().find(|t| t.eq_ignore_ascii_case(name));
        match real {
            Some(r) => self.tx.static_schema(&r, self.schemas)?.ok_or_else(|| Error::NoSuchTable(name.to_owned())),
            None => Err(Error::NoSuchTable(name.to_owned())),
        }
    }

    fn param(&self, i: usize) -> Result<Value> {
        self.params.get(i).cloned().ok_or_else(|| syntax(format!("parameter {} is missing", i + 1)))
    }

    // -----------------------------------------------------------------------
    // Expressions
    // -----------------------------------------------------------------------

    /// The value of `e`. Each kind of expression is computed by a function of
    /// its own: an expression inside another is computed by calling this one
    /// again, and every level would otherwise pay on the stack for the
    /// variables of every kind (unoptimized builds give each its own place).
    fn eval(&self, e: &Expr, env: &Env<'_>) -> Result<Value> {
        match e {
            Expr::Lit(v) => Ok(v.clone()),
            Expr::Param(i) => self.param(*i),
            Expr::Col { table, name } => env.lookup(table.as_deref(), name),
            Expr::Neg(x) => self.neg(x, env),
            Expr::Not(x) => self.not(x, env),
            Expr::Chain(first, rest) => self.chain(first, rest, env),
            Expr::Bin(op, a, b) => self.binary(*op, a, b, env),
            Expr::IsNull(x, negated) => self.is_null(x, *negated, env),
            Expr::In(x, list, negated) => self.in_list(x, list, *negated, env),
            Expr::InSelect(x, sub, negated) => self.in_select(x, sub, *negated, env),
            Expr::Exists(sub, negated) => self.exists(sub, *negated, env),
            Expr::Scalar(sub) => self.scalar_subquery(sub, env),
            Expr::Between(x, low, high, negated) => self.between(x, low, high, *negated, env),
            Expr::Like { expr, pattern, ci, negated, escape } => {
                self.like(expr, pattern, *ci, *negated, escape.as_deref(), env)
            }
            Expr::Case { operand, whens, otherwise } => self.case(operand.as_deref(), whens, otherwise.as_deref(), env),
            Expr::Cast(x, kind) => self.cast(x, *kind, env),
            Expr::Func { .. } if e.is_aggregate() => self.aggregate(e, env),
            Expr::Func { name, args, .. } => self.call(name, args, env),
        }
    }

    fn neg(&self, x: &Expr, env: &Env<'_>) -> Result<Value> {
        Ok(match self.eval(x, env)? {
            // The smallest INT has no opposite among them.
            Value::I32(i) => i.checked_neg().map_or(Value::I64(-i64::from(i)), Value::I32),
            Value::F64(f) => Value::F64(-f),
            other => arith(Op::Sub, &Value::I64(0), &other)?,
        })
    }

    fn not(&self, x: &Expr, env: &Env<'_>) -> Result<Value> {
        Ok(bool_or_null(tri(&self.eval(x, env)?).map(|b| !b)))
    }

    fn binary(&self, op: Op, a: &Expr, b: &Expr, env: &Env<'_>) -> Result<Value> {
        let l = self.eval(a, env)?;
        // The left side is kept while the right one is computed.
        let mut live = self.limits.live();
        live.add(&l)?;
        let r = self.eval(b, env)?;
        drop(live);
        if !matches!(op, Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge) {
            return arith(op, &l, &r);
        }
        if l.is_null() || r.is_null() {
            return Ok(Value::Null);
        }
        Ok(Value::Bool(match (op, compare(&l, &r)) {
            (Op::Ne, None) => true,
            (_, None) => false,
            (Op::Eq, Some(o)) => o.is_eq(),
            (Op::Ne, Some(o)) => !o.is_eq(),
            (Op::Lt, Some(o)) => o.is_lt(),
            (Op::Le, Some(o)) => o.is_le(),
            (Op::Gt, Some(o)) => o.is_gt(),
            (_, Some(o)) => o.is_ge(),
        }))
    }

    fn is_null(&self, x: &Expr, negated: bool, env: &Env<'_>) -> Result<Value> {
        Ok(Value::Bool(self.eval(x, env)?.is_null() != negated))
    }

    fn in_list(&self, x: &Expr, list: &[Expr], negated: bool, env: &Env<'_>) -> Result<Value> {
        let v = self.eval(x, env)?;
        let mut live = self.limits.live();
        live.add(&v)?;
        let mut values = Vec::with_capacity(list.len());
        for item in list {
            let item = self.eval(item, env)?;
            live.add(&item)?;
            values.push(item);
        }
        Ok(in_values(&v, &values, negated))
    }

    fn in_select(&self, x: &Expr, sub: &Select, negated: bool, env: &Env<'_>) -> Result<Value> {
        let v = self.eval(x, env)?;
        let mut live = self.limits.live();
        live.add(&v)?;
        let rows = self.subquery(sub, env)?;
        if rows.columns.len() != 1 {
            return Err(syntax("the subquery of IN must return one column"));
        }
        let values: Vec<Value> = rows.rows.iter().map(|r| r[0].clone()).collect();
        Ok(in_values(&v, &values, negated))
    }

    fn exists(&self, sub: &Select, negated: bool, env: &Env<'_>) -> Result<Value> {
        // One row tells.
        Ok(Value::Bool(self.subquery_first(sub, env, Some(1))?.rows.is_empty() == negated))
    }

    fn scalar_subquery(&self, sub: &Select, env: &Env<'_>) -> Result<Value> {
        // Its first row is its value.
        let rows = self.subquery_first(sub, env, Some(1))?;
        if rows.columns.len() != 1 {
            return Err(syntax("a subquery used as a value must return one column"));
        }
        Ok(rows.rows.first().map_or(Value::Null, |r| r[0].clone()))
    }

    fn between(&self, x: &Expr, low: &Expr, high: &Expr, negated: bool, env: &Env<'_>) -> Result<Value> {
        let mut live = self.limits.live();
        let v = self.eval(x, env)?;
        live.add(&v)?;
        let l = self.eval(low, env)?;
        live.add(&l)?;
        let h = self.eval(high, env)?;
        drop(live);
        if v.is_null() || l.is_null() || h.is_null() {
            return Ok(Value::Null);
        }
        let inside = compare(&v, &l).is_some_and(|o| o.is_ge()) && compare(&v, &h).is_some_and(|o| o.is_le());
        Ok(Value::Bool(inside != negated))
    }

    fn like(
        &self,
        expr: &Expr,
        pattern: &Expr,
        ci: bool,
        negated: bool,
        escape: Option<&Expr>,
        env: &Env<'_>,
    ) -> Result<Value> {
        let mut live = self.limits.live();
        let s = self.eval(expr, env)?;
        live.add(&s)?;
        let p = self.eval(pattern, env)?;
        live.add(&p)?;
        let (Some(s), Some(p)) = (v::to_text(&s), v::to_text(&p)) else { return Ok(Value::Null) };
        let esc = match escape {
            Some(e) => v::to_text(&self.eval(e, env)?).and_then(|t| t.chars().next()),
            None => None,
        };
        let matched = v::like(&s, &p, ci, esc)?;
        self.limits.work(s.len() + p.len())?;
        Ok(Value::Bool(matched != negated))
    }

    fn case(
        &self,
        operand: Option<&Expr>,
        whens: &[(Expr, Expr)],
        otherwise: Option<&Expr>,
        env: &Env<'_>,
    ) -> Result<Value> {
        let subject = match operand {
            Some(o) => Some(self.eval(o, env)?),
            None => None,
        };
        let mut live = self.limits.live();
        if let Some(s) = &subject {
            live.add(s)?;
        }
        for (w, then) in whens {
            self.limits.tick()?;
            let hit = match &subject {
                Some(s) => compare(s, &self.eval(w, env)?).is_some_and(|o| o.is_eq()),
                None => truthy(&self.eval(w, env)?),
            };
            if hit {
                return self.eval(then, env);
            }
        }
        match otherwise {
            Some(o) => self.eval(o, env),
            None => Ok(Value::Null),
        }
    }

    fn cast(&self, x: &Expr, kind: ColumnType, env: &Env<'_>) -> Result<Value> {
        let x = self.eval(x, env)?;
        let out = cast(&x, kind)?;
        if matches!(x, Value::Text(_) | Value::Bytes(_) | Value::Json(_)) {
            self.limits.work(value_bytes(&x))?;
        }
        Ok(out)
    }

    fn call(&self, name: &str, args: &[Expr], env: &Env<'_>) -> Result<Value> {
        self.limits.tick()?;
        let mut values = Vec::with_capacity(args.len());
        // The arguments computed are kept while the next ones are: each may be large.
        let mut live = self.limits.live();
        for a in args {
            let x = self.eval(a, env)?;
            live.add(&x)?;
            values.push(x);
        }
        let read = live.bytes();
        let out = v::scalar(name, values)?;
        drop(live);
        // What a function gets through counts toward the deadline, however few the rows.
        self.limits.work(read.saturating_add(value_bytes(&out)))?;
        Ok(out)
    }

    /// `first op x op y ...`, from the left, in a loop: a long list takes no
    /// more of the stack than a short one.
    fn chain(&self, first: &Expr, rest: &[(Op, Expr)], env: &Env<'_>) -> Result<Value> {
        let acc = self.eval(first, env)?;
        match rest.first().map(|(op, _)| *op) {
            Some(Op::And) => self.all(acc, rest, env),
            Some(Op::Or) => self.any(acc, rest, env),
            _ => self.fold(acc, rest, env),
        }
    }

    /// `AND`: false as soon as an operand is (the others are not evaluated),
    /// NULL when one is unknown and none is false.
    fn all(&self, first: Value, rest: &[(Op, Expr)], env: &Env<'_>) -> Result<Value> {
        let mut all = tri(&first);
        for (_, x) in rest {
            if all == Some(false) {
                break;
            }
            self.limits.tick()?;
            all = match (all, tri(&self.eval(x, env)?)) {
                (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            };
        }
        Ok(bool_or_null(all))
    }

    /// `OR`: true as soon as an operand is, NULL when one is unknown and none is true.
    fn any(&self, first: Value, rest: &[(Op, Expr)], env: &Env<'_>) -> Result<Value> {
        let mut any = tri(&first);
        for (_, x) in rest {
            if any == Some(true) {
                break;
            }
            self.limits.tick()?;
            any = match (any, tri(&self.eval(x, env)?)) {
                (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            };
        }
        Ok(bool_or_null(any))
    }

    /// Arithmetic and `||`, from the left.
    fn fold(&self, mut acc: Value, rest: &[(Op, Expr)], env: &Env<'_>) -> Result<Value> {
        for (op, x) in rest {
            self.limits.tick()?;
            acc = match (op, x.interval()) {
                // `x + INTERVAL '1 day'`, `x - INTERVAL '2 hours'`.
                (Op::Add, Some(span)) => v::shift(&acc, span, 1)?,
                (Op::Sub, Some(span)) => v::shift(&acc, span, -1)?,
                _ => {
                    // What was computed so far is kept while the next operand is.
                    let mut live = self.limits.live();
                    live.add(&acc)?;
                    let next = self.eval(x, env)?;
                    drop(live);
                    if *op == Op::Concat {
                        self.limits.work(value_bytes(&next))?;
                        v::concat(acc, &next)?
                    } else {
                        arith(*op, &acc, &next)?
                    }
                }
            };
        }
        Ok(acc)
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
        // The values of the group, and the keys that tell them apart, are held while it is computed.
        let mut held = self.limits.hold();
        for row in rows {
            self.limits.tick()?;
            let inner = Env {
                scope: env.scope,
                row,
                tail: &[],
                group: None,
                outer: env.outer,
                used_outer: env.used_outer,
                output: None,
            };
            let x = self.eval(&args[0], &inner)?;
            if x.is_null() {
                continue;
            }
            if *distinct {
                let mut k = String::new();
                v::key(&x, &mut k);
                held.add(k.len() + KEY_BYTES)?;
                if !seen.insert(k) {
                    continue;
                }
            }
            held.add(value_bytes(&x))?;
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
            "array_agg" => {
                text_fits("array_agg()", values.iter().fold(0usize, |n, x| n.saturating_add(value_bytes(x))))?;
                Value::Json(Json::Array(values.iter().map(v::to_json).collect()))
            }
            // PostgreSQL's boolean aggregates: NULL without rows, NULLs left out.
            "bool_and" | "every" | "bool_or" => {
                if values.is_empty() {
                    return Ok(Value::Null);
                }
                let mut all = true;
                let mut any = false;
                for x in &values {
                    let b = matches!(cast(x, ColumnType::Bool)?, Value::Bool(true));
                    all &= b;
                    any |= b;
                }
                Value::Bool(if name == "bool_or" { any } else { all })
            }
            _ => {
                // group_concat(x [, sep]), string_agg(x, sep)
                if values.is_empty() {
                    return Ok(Value::Null);
                }
                let sep = match args.get(1) {
                    Some(s) => v::to_text(&self.eval(s, env)?).unwrap_or_default(),
                    None => ",".into(),
                };
                // Written straight into the result, its size known first: the values are
                // held already, and a copy of them as texts would hold them twice.
                let size = v::joined_len(values.iter().filter_map(v::text_ref).map(|t| t.len()), sep.len());
                text_fits(&format!("{name}()"), size)?;
                let mut out = String::with_capacity(size);
                for (i, t) in values.iter().filter_map(v::text_ref).enumerate() {
                    if i > 0 {
                        out.push_str(&sep);
                    }
                    out.push_str(&t);
                }
                self.limits.work(size)?;
                Value::Text(out)
            }
        })
    }

    fn subquery(&self, sub: &Select, env: &Env<'_>) -> Result<Rc<Rows>> {
        self.subquery_first(sub, env, None)
    }

    /// The rows of a subquery, the first `want` of them when only those are
    /// needed (`EXISTS`). A `SELECT` is in one place of a statement: its rows
    /// are always asked for the same way.
    fn subquery_first(&self, sub: &Select, env: &Env<'_>, want: Option<usize>) -> Result<Rc<Rows>> {
        if let Some(r) = self.cache.borrow().get(&sub.id) {
            return Ok(r.clone());
        }
        // A subquery that reads the enclosing row runs once per row: the clock is read at each run.
        self.limits.check_time()?;
        let used = Cell::new(false);
        let rows = Rc::new(self.select_first(sub, Some(env), &used, want)?);
        if !used.get() {
            self.cache.borrow_mut().insert(sub.id, rows.clone());
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
        let depth = self.m_depth.get();
        if depth >= M_DEPTH {
            return None;
        }
        self.m_depth.set(depth + 1);
        let m = self.m_expr(e, scope, only, outer, used);
        self.m_depth.set(depth);
        m
    }

    /// [`Runner::to_m`], one function per kind of expression as for [`Runner::eval`].
    fn m_expr(
        &self,
        e: &Expr,
        scope: &Scope,
        only: Option<usize>,
        outer: Option<&Env<'_>>,
        used: &Cell<bool>,
    ) -> Option<M> {
        match e {
            Expr::Lit(v) => Some(M::Value(v.clone())),
            Expr::Param(i) => Some(M::Value(self.param(*i).ok()?)),
            Expr::Col { table, name } => m_column(table.as_deref(), name, scope, only, outer, used),
            Expr::Not(x) => Some(M::Not(Box::new(self.to_m(x, scope, only, outer, used)?))),
            Expr::IsNull(x, negated) => self.m_is_null(x, *negated, scope, only, outer, used),
            Expr::Bin(op, a, b) => self.m_compare(*op, a, b, scope, only, outer, used),
            Expr::Chain(first, rest) => self.m_logic(first, rest, scope, only, outer, used),
            Expr::In(x, list, negated) => self.m_in(x, list, *negated, scope, only, outer, used),
            Expr::InSelect(x, sub, negated) => self.m_in_select(x, sub, *negated, scope, only, outer, used),
            Expr::Between(x, low, high, negated) => {
                self.m_between([&**x, &**low, &**high], *negated, scope, only, outer, used)
            }
            Expr::Like { expr, pattern, ci, negated, escape: None } => {
                self.m_like(expr, pattern, *ci, *negated, scope, only, outer, used)
            }
            Expr::Func { name, args, distinct, star } if e.is_aggregate() => {
                self.m_aggregate(name, args.first(), *distinct, *star, scope, only, outer, used)
            }
            _ => None,
        }
    }

    fn m_is_null(
        &self,
        x: &Expr,
        negated: bool,
        scope: &Scope,
        only: Option<usize>,
        outer: Option<&Env<'_>>,
        used: &Cell<bool>,
    ) -> Option<M> {
        Some(M::IsNull { expr: Box::new(self.to_m(x, scope, only, outer, used)?), negated })
    }

    #[allow(clippy::too_many_arguments)]
    fn m_compare(
        &self,
        op: Op,
        a: &Expr,
        b: &Expr,
        scope: &Scope,
        only: Option<usize>,
        outer: Option<&Env<'_>>,
        used: &Cell<bool>,
    ) -> Option<M> {
        let op = match op {
            Op::Eq => BinOp::Eq,
            Op::Ne => BinOp::Ne,
            Op::Lt => BinOp::Lt,
            Op::Le => BinOp::Le,
            Op::Gt => BinOp::Gt,
            Op::Ge => BinOp::Ge,
            _ => return None,
        };
        m_pair(op, self.to_m(a, scope, only, outer, used)?, self.to_m(b, scope, only, outer, used)?)
    }

    /// `AND` and `OR` lists (Memoire has no arithmetic).
    fn m_logic(
        &self,
        first: &Expr,
        rest: &[(Op, Expr)],
        scope: &Scope,
        only: Option<usize>,
        outer: Option<&Env<'_>>,
        used: &Cell<bool>,
    ) -> Option<M> {
        let op = match rest.first()?.0 {
            Op::And => BinOp::And,
            Op::Or => BinOp::Or,
            _ => return None,
        };
        let mut parts = Vec::with_capacity(rest.len() + 1);
        parts.push(self.to_m(first, scope, only, outer, used)?);
        for (_, x) in rest {
            parts.push(self.to_m(x, scope, only, outer, used)?);
        }
        // `id = 1 OR id = 2 OR ...`: `id IN (1, 2, ...)`, which an index serves.
        if op == BinOp::Or
            && let Some(list) = one_column_among(&parts)
        {
            return Some(list);
        }
        // The first two as a pair (a value beside a column takes its type), the others after.
        let mut parts = parts.into_iter();
        let mut joined = vec![m_pair(op, parts.next()?, parts.next()?)?];
        joined.extend(parts);
        balanced(op, joined)
    }

    #[allow(clippy::too_many_arguments)]
    fn m_in(
        &self,
        x: &Expr,
        list: &[Expr],
        negated: bool,
        scope: &Scope,
        only: Option<usize>,
        outer: Option<&Env<'_>>,
        used: &Cell<bool>,
    ) -> Option<M> {
        let M::Column(c) = self.to_m(x, scope, only, outer, used)? else { return None };
        let mut values = Vec::with_capacity(list.len());
        for item in list {
            let M::Value(v) = self.to_m(item, scope, only, outer, used)? else { return None };
            if v.is_null() {
                return None;
            }
            values.push(coerce(&v, c.kind)?);
        }
        Some(M::InList { expr: Box::new(M::Column(c)), list: values, negated })
    }

    #[allow(clippy::too_many_arguments)]
    fn m_in_select(
        &self,
        x: &Expr,
        sub: &Select,
        negated: bool,
        scope: &Scope,
        only: Option<usize>,
        outer: Option<&Env<'_>>,
        used: &Cell<bool>,
    ) -> Option<M> {
        let M::Column(c) = self.to_m(x, scope, only, outer, used)? else { return None };
        // Run it first (once), and look the values up: an index serves them.
        let env =
            Env { scope: &Scope::default(), row: &[], tail: &[], group: None, outer, used_outer: used, output: None };
        let rows = self.subquery(sub, &env).ok()?;
        if rows.columns.len() != 1 || rows.rows.iter().any(|r| r[0].is_null()) {
            return None;
        }
        let list = rows.rows.iter().map(|r| coerce(&r[0], c.kind)).collect::<Option<Vec<_>>>()?;
        Some(M::InList { expr: Box::new(M::Column(c)), list, negated })
    }

    /// `x BETWEEN low AND high`, `[x, low, high]`.
    fn m_between(
        &self,
        [x, low, high]: [&Expr; 3],
        negated: bool,
        scope: &Scope,
        only: Option<usize>,
        outer: Option<&Env<'_>>,
        used: &Cell<bool>,
    ) -> Option<M> {
        let M::Column(c) = self.to_m(x, scope, only, outer, used)? else { return None };
        let (M::Value(l), M::Value(h)) =
            (self.to_m(low, scope, only, outer, used)?, self.to_m(high, scope, only, outer, used)?)
        else {
            return None;
        };
        Some(M::Between {
            expr: Box::new(M::Column(c)),
            low: Box::new(M::Value(coerce(&l, c.kind)?)),
            high: Box::new(M::Value(coerce(&h, c.kind)?)),
            negated,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn m_like(
        &self,
        expr: &Expr,
        pattern: &Expr,
        ci: bool,
        negated: bool,
        scope: &Scope,
        only: Option<usize>,
        outer: Option<&Env<'_>>,
        used: &Cell<bool>,
    ) -> Option<M> {
        let M::Column(c) = self.to_m(expr, scope, only, outer, used)? else { return None };
        if c.kind != ColumnType::Text {
            return None;
        }
        let M::Value(Value::Text(p)) = self.to_m(pattern, scope, only, outer, used)? else { return None };
        if p.contains('\\') {
            return None;
        }
        Some(M::Like {
            expr: Box::new(M::Column(c)),
            pattern: Box::new(M::Value(Value::Text(p))),
            case_insensitive: ci,
            negated,
            escaped: false,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn m_aggregate(
        &self,
        name: &str,
        arg: Option<&Expr>,
        distinct: bool,
        star: bool,
        scope: &Scope,
        only: Option<usize>,
        outer: Option<&Env<'_>>,
        used: &Cell<bool>,
    ) -> Option<M> {
        let func = match name {
            "count" => AggFunc::Count,
            "sum" => AggFunc::Sum,
            "avg" => AggFunc::Avg,
            "min" => AggFunc::Min,
            "max" => AggFunc::Max,
            _ => return None,
        };
        let arg = if star { None } else { Some(Box::new(self.to_m(arg?, scope, only, outer, used)?)) };
        if arg.as_deref().is_some_and(|a| !matches!(a, M::Column(_))) {
            return None;
        }
        Some(M::Aggregate { func, arg, distinct })
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
        self.select_first(sel, outer, used, None)
    }

    /// [`Runner::select`], only its first `want` rows when given.
    fn select_first(
        &self,
        sel: &Select,
        outer: Option<&Env<'_>>,
        used: &Cell<bool>,
        want: Option<usize>,
    ) -> Result<Rows> {
        if let Some((mut ast, columns)) = self.as_memoire(sel, outer, used) {
            if let Some(w) = want {
                let w = w as u64;
                ast.limit = Some(ast.limit.map_or(w, |l| l.min(w)));
            }
            self.limits.take_declined();
            match self.tx.fetch_rows(&Query::Select(&ast)) {
                Ok(rows) => return Ok(Rows { columns, hold: self.limits.hold_rows(&rows)?, rows }),
                // The engine would have joined more rows than the statement
                // may hold: they are joined below, each row counted.
                Err(_) if self.limits.take_declined() => {}
                Err(e) => return Err(e),
            }
        }
        let empty_env =
            Env { scope: &Scope::default(), row: &[], tail: &[], group: None, outer, used_outer: used, output: None };
        let limit = match &sel.limit {
            Some(e) => match self.eval(e, &empty_env)? {
                Value::Null => None,
                v => v.as_i64().filter(|n| *n >= 0).map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
            },
            None => None,
        };
        let limit = match (limit, want) {
            (Some(l), Some(w)) => Some(l.min(w)),
            (l, w) => l.or(w),
        };
        let offset = match &sel.offset {
            Some(e) => usize::try_from(self.eval(e, &empty_env)?.as_i64().unwrap_or(0).max(0)).unwrap_or(usize::MAX),
            None => 0,
        };
        let mut out = if sel.rest.is_empty() {
            let hint = limit.map(|l| l.saturating_add(offset));
            self.core(&sel.first, &sel.order, hint, outer, used)?
        } else {
            let mut acc = self.core(&sel.first, &[], None, outer, used)?;
            // The keys of the rows of `acc` when they are all different (after a UNION, an
            // INTERSECT or an EXCEPT): a UNION after it looks up only its own rows, instead
            // of the rows of every step before, which made a long UNION take the square
            // of its rows.
            let mut distinct: Option<(std::collections::HashSet<String>, Hold)> = None;
            for (op, core) in &sel.rest {
                let next = self.core(core, &[], None, outer, used)?;
                if next.columns.len() != acc.columns.len() {
                    return Err(syntax("each SELECT of a UNION must have the same number of columns"));
                }
                match op {
                    SetOp::UnionAll => {
                        // The new rows after the others, where those are.
                        acc.rows.extend(next.rows);
                        acc.hold.absorb(next.hold);
                        distinct = None;
                    }
                    SetOp::Union => {
                        let (mut seen, mut keys) = match distinct.take() {
                            Some(d) => d,
                            None => {
                                let mut seen = std::collections::HashSet::new();
                                let mut keys = self.limits.hold();
                                let mut dropped = 0;
                                let rows = std::mem::take(&mut acc.rows);
                                for r in rows {
                                    self.limits.tick()?;
                                    let k = row_key(&r);
                                    if seen.contains(&k) {
                                        dropped += row_bytes(&r);
                                        continue;
                                    }
                                    keys.add(k.len() + KEY_BYTES)?;
                                    seen.insert(k);
                                    acc.rows.push(r);
                                }
                                acc.hold.less(dropped);
                                (seen, keys)
                            }
                        };
                        acc.hold.absorb(next.hold);
                        let mut dropped = 0;
                        for r in next.rows {
                            self.limits.tick()?;
                            let k = row_key(&r);
                            if seen.contains(&k) {
                                dropped += row_bytes(&r);
                                continue;
                            }
                            keys.add(k.len() + KEY_BYTES)?;
                            seen.insert(k);
                            acc.rows.push(r);
                        }
                        acc.hold.less(dropped);
                        distinct = Some((seen, keys));
                    }
                    SetOp::Intersect | SetOp::Except => {
                        // The keys of the step before are of no use here.
                        drop(distinct.take());
                        // The keys of the rows of both sides are held while this step runs.
                        let mut keys = self.limits.hold();
                        let mut other = std::collections::HashSet::new();
                        for r in &next.rows {
                            self.limits.tick()?;
                            let k = row_key(r);
                            keys.add(k.len() + KEY_BYTES)?;
                            other.insert(k);
                        }
                        drop(next);
                        let mut seen = std::collections::HashSet::new();
                        let mut kept_keys = self.limits.hold();
                        let mut kept = Vec::new();
                        for r in std::mem::take(&mut acc.rows) {
                            self.limits.tick()?;
                            let k = row_key(&r);
                            if other.contains(&k) == (*op == SetOp::Intersect) && !seen.contains(&k) {
                                kept_keys.add(k.len() + KEY_BYTES)?;
                                seen.insert(k);
                                kept.push(r);
                            }
                        }
                        acc.rows = kept;
                        acc.hold.set(rows_bytes(&acc.rows))?;
                        distinct = Some((seen, kept_keys));
                    }
                }
            }
            drop(distinct);
            // ORDER BY of a compound: output columns, by name or position.
            if !sel.order.is_empty() {
                let mut keys = Vec::new();
                for o in &sel.order {
                    let i = match &o.expr {
                        Expr::Lit(Value::I64(n)) => {
                            usize::try_from(*n).ok().and_then(|n| n.checked_sub(1)).filter(|i| *i < acc.columns.len())
                        }
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
            out.rows.drain(..offset.min(out.rows.len()));
            out.rows.truncate(limit.unwrap_or(usize::MAX));
            out.hold.set(rows_bytes(&out.rows))?;
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

    /// `SELECT jsonb_array_elements_text(x)::bigint` (no `FROM`): one row per
    /// element of the JSON array, the expression around the function (casts,
    /// arithmetic) computed for each. `None` when the SELECT is not one.
    fn set_returning_core(
        &self,
        core: &Core,
        scope: &Scope,
        outer: Option<&Env<'_>>,
        used: &Cell<bool>,
    ) -> Result<Option<Rows>> {
        fn find(e: &Expr) -> Option<(&str, &[Expr])> {
            match e {
                Expr::Func { name, args, .. } if v::SET_RETURNING.contains(&name.as_str()) => Some((name, args)),
                Expr::Cast(x, _) | Expr::Neg(x) => find(x),
                Expr::Bin(_, a, b) => find(a).or_else(|| find(b)),
                Expr::Chain(first, rest) => find(first).or_else(|| rest.iter().find_map(|(_, x)| find(x))),
                _ => None,
            }
        }
        fn replace(e: &Expr, with: &Value) -> Expr {
            match e {
                Expr::Func { name, .. } if v::SET_RETURNING.contains(&name.as_str()) => Expr::Lit(with.clone()),
                Expr::Cast(x, k) => Expr::Cast(Box::new(replace(x, with)), *k),
                Expr::Neg(x) => Expr::Neg(Box::new(replace(x, with))),
                Expr::Bin(op, a, b) => Expr::Bin(*op, Box::new(replace(a, with)), Box::new(replace(b, with))),
                Expr::Chain(first, rest) => Expr::Chain(
                    Box::new(replace(first, with)),
                    rest.iter().map(|(op, x)| (*op, replace(x, with))).collect(),
                ),
                other => other.clone(),
            }
        }
        let [SelItem::Expr(item, alias)] = core.items.as_slice() else { return Ok(None) };
        let Some((name, args)) = find(item) else { return Ok(None) };
        if core.filter.is_some() || !core.group_by.is_empty() || core.having.is_some() || core.distinct {
            return Err(syntax(format!("{name}(): use it alone, without WHERE, GROUP BY or DISTINCT")));
        }
        let [arg] = args else { return Err(syntax(format!("{name}() takes one argument"))) };
        let env = Env { scope, row: &[], tail: &[], group: None, outer, used_outer: used, output: None };
        let elements = v::set_returning(name, &self.eval(arg, &env)?)?;
        let mut held_elements = self.limits.hold();
        held_elements.add(elements.iter().map(value_bytes).sum())?;
        let mut rows = Vec::with_capacity(elements.len());
        let mut hold = self.limits.hold();
        for x in &elements {
            self.limits.tick()?;
            let row = vec![self.eval(&replace(item, x), &env)?];
            hold.add(row_bytes(&row))?;
            rows.push(row);
        }
        let column = alias.clone().unwrap_or_else(|| name.to_owned());
        Ok(Some(Rows { columns: vec![column], rows, hold }))
    }

    /// The last table of `scope` that `e` reads a column of (`Some(0)` when it
    /// reads none). `None` when that cannot be told from the names: it has a
    /// subquery (which may read any column of the row), or a name the scope
    /// does not settle.
    fn last_part_read(&self, e: &Expr, scope: &Scope) -> Option<usize> {
        let mut last = Some(0);
        e.walk(&mut |x| match x {
            Expr::Col { table, name } => match scope.find(table.as_deref(), name) {
                Ok(Some((p, _))) => last = last.map(|l| l.max(p)),
                // A column of the enclosing statement: the same for every row here.
                Ok(None) => {}
                Err(_) => last = None,
            },
            Expr::Exists(..) | Expr::Scalar(_) | Expr::InSelect(..) => last = None,
            _ => {}
        });
        last
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
        let mut sources: Vec<Option<(Vec<Vec<Value>>, Hold)>> = Vec::new();
        let refs: Vec<(&TableRef, Option<&Join>)> =
            core.from.iter().map(|f| (f, None)).chain(core.joins.iter().map(|j| (&j.table, Some(j)))).collect();
        if refs.is_empty()
            && let Some(rows) = self.set_returning_core(core, &scope, outer, used)?
        {
            return Ok(rows);
        }
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
                    let Rows { columns, rows, hold } = self.select(sub, outer, used)?;
                    let alias = t
                        .alias
                        .clone()
                        .ok_or_else(|| syntax("a subquery in FROM needs an alias: (SELECT ...) AS name"))?;
                    let kinds = (0..columns.len())
                        .map(|i| rows.iter().map(|r| &r[i]).find(|x| !x.is_null()).and_then(v::kind_of))
                        .collect();
                    scope.push(alias, columns, kinds, None);
                    sources.push(Some((rows, hold)));
                }
            }
        }
        // WHERE, split: what each table's read can apply.
        let mut where_parts: Vec<&Expr> = Vec::new();
        if let Some(f) = &core.filter {
            conj(f, &mut where_parts);
        }
        // Applied already: by a table's read, or while joining.
        let mut pushed = vec![false; where_parts.len()];
        let lone = refs.len() == 1;
        let grouped = !core.group_by.is_empty()
            || core.having.is_some()
            || core.items.iter().any(|i| matches!(i, SelItem::Expr(e, _) if e.has_aggregate()))
            || order.iter().any(|o| o.expr.has_aggregate());
        // `SELECT count(*) FROM a, b ...` and nothing else: the rows of the last join are
        // counted, not kept. `count(1)` counts the same.
        let count_only = order.is_empty()
            && !core.distinct
            && core.group_by.is_empty()
            && core.having.is_none()
            && match core.items.as_slice() {
                [SelItem::Expr(Expr::Func { name, star: true, .. }, _)] => name == "count",
                [SelItem::Expr(Expr::Func { name, args, distinct: false, star: false }, _)] => {
                    name == "count" && matches!(args.as_slice(), [Expr::Lit(v)] if !v.is_null())
                }
                _ => false,
            };
        // Only the first rows are needed, in the order they come: the last join stops there.
        let first = hint.filter(|_| !grouped && !core.distinct && order.is_empty());
        // For each condition of WHERE, the last table it reads: a join applies it as
        // soon as that table is joined, instead of keeping rows it will refuse.
        let reads: Vec<Option<usize>> =
            if lone { Vec::new() } else { where_parts.iter().map(|w| self.last_part_read(w, &scope)).collect() };
        let last = refs.len().saturating_sub(1);
        let mut rows: Vec<Vec<Value>> = if refs.is_empty() { vec![Vec::new()] } else { Vec::new() };
        let mut held = self.limits.hold_rows(&rows)?;
        let mut counted: Option<usize> = None;
        for (p, (_, join)) in refs.iter().enumerate() {
            let (part_rows, part_hold) = match sources[p].take() {
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
                    let read = self.read_table(schema, and_all(filters), top)?;
                    let hold = self.limits.hold_rows(&read)?;
                    (read, hold)
                }
            };
            if p == 0 {
                rows = part_rows;
                held = part_hold;
                // What WHERE says of the first table alone, before its rows are joined
                // (a condition that fails on a row is left for the whole rows, as in a join).
                let now: Vec<usize> = (0..reads.len()).filter(|&i| !pushed[i] && reads[i] == Some(0)).collect();
                if !now.is_empty() {
                    let mut failed = vec![false; now.len()];
                    let mut kept = Vec::with_capacity(rows.len());
                    for r in rows {
                        self.limits.tick()?;
                        let env = Env {
                            scope: &scope,
                            row: &r,
                            tail: &[],
                            group: None,
                            outer,
                            used_outer: used,
                            output: None,
                        };
                        let mut keep = true;
                        for (n, &i) in now.iter().enumerate() {
                            match self.eval(where_parts[i], &env) {
                                Ok(v) if tri(&v) == Some(true) => {}
                                Ok(_) => {
                                    keep = false;
                                    break;
                                }
                                Err(_) => {
                                    failed[n] = true;
                                    break;
                                }
                            }
                        }
                        if keep {
                            kept.push(r);
                        }
                    }
                    rows = kept;
                    held.set(rows_bytes(&rows))?;
                    for (n, &i) in now.iter().enumerate() {
                        pushed[i] = !failed[n];
                    }
                }
                continue;
            }
            let join = join.expect("joins after the first table");
            // The conditions of WHERE this join can answer. At the last join, all that
            // are left, in their order: the rows it gives are the rows of the result.
            // Before, those that read only the tables joined so far.
            let whole = p == last;
            let now: Vec<usize> =
                (0..where_parts.len()).filter(|&i| !pushed[i] && (whole || reads[i].is_some_and(|q| q <= p))).collect();
            let also: Vec<&Expr> = now.iter().map(|&i| where_parts[i]).collect();
            let wanted = Wanted { count: whole && count_only, first: if whole { first } else { None } };
            let joined = self.join(&scope, p, &rows, &part_rows, join, &also, whole, wanted, outer, used)?;
            for (&i, failed) in now.iter().zip(&joined.failed) {
                pushed[i] = !failed;
            }
            if whole && count_only {
                counted = Some(joined.count);
            }
            rows = joined.rows;
            held = joined.hold;
            drop(part_hold);
        }
        if let Some(n) = counted {
            let [SelItem::Expr(e, alias)] = core.items.as_slice() else { unreachable!("count_only") };
            let rows = vec![vec![Value::I64(i64::try_from(n).unwrap_or(i64::MAX))]];
            return Ok(Rows {
                columns: unique_headers(vec![(alias.clone().unwrap_or_else(|| header(e, 0)), None)]),
                hold: self.limits.hold_rows(&rows)?,
                rows,
            });
        }
        // The rest of WHERE.
        let residual: Vec<&Expr> = where_parts.iter().zip(&pushed).filter(|(_, p)| !**p).map(|(w, _)| *w).collect();
        if !residual.is_empty() {
            let mut kept = Vec::with_capacity(rows.len());
            for r in rows {
                self.limits.tick()?;
                let env = Env { scope: &scope, row: &r, tail: &[], group: None, outer, used_outer: used, output: None };
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
            held.set(rows_bytes(&rows))?;
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
        // Groups: the rows of each, the first one being where its plain columns are read.
        let null_row = vec![Value::Null; scope.width];
        let (rows, groups): (Vec<Vec<Value>>, Option<Vec<Group>>) = if grouped {
            let mut index: HashMap<String, usize> = HashMap::new();
            let mut groups: Vec<Group> = Vec::new();
            for r in rows {
                self.limits.tick()?;
                let mut k = String::new();
                {
                    let env =
                        Env { scope: &scope, row: &r, tail: &[], group: None, outer, used_outer: used, output: None };
                    // The key grows with each expression: it is kept while the next one is computed.
                    let mut live = self.limits.live();
                    for g in &core.group_by {
                        let before = k.len();
                        v::key(&self.eval(g, &env)?, &mut k);
                        live.add_bytes(k.len() - before)?;
                    }
                }
                match index.get(&k) {
                    Some(&i) => groups[i].push(r),
                    None => {
                        held.add(k.len() + KEY_BYTES)?;
                        index.insert(k, groups.len());
                        groups.push(vec![r]);
                    }
                }
            }
            if groups.is_empty() && core.group_by.is_empty() {
                groups.push(Vec::new());
            }
            (Vec::new(), Some(groups))
        } else {
            (rows, None)
        };
        // Projection, HAVING and the sort keys.
        let units = groups.as_ref().map_or(rows.len(), Vec::len);
        let mut produced: Vec<(Vec<Value>, Vec<Value>)> = Vec::with_capacity(units);
        let mut produced_hold = self.limits.hold();
        // Each row read is let go once its columns are computed: the statement holds the
        // rows read, then the result, not both whole at once.
        let mut read = rows.into_iter();
        for unit in 0..units {
            self.limits.tick()?;
            let owned;
            let (row, members): (&[Value], Option<&[Vec<Value>]>) = match &groups {
                Some(groups) => {
                    (groups[unit].first().map_or(null_row.as_slice(), Vec::as_slice), Some(groups[unit].as_slice()))
                }
                None => {
                    owned = read.next().unwrap_or_default();
                    (owned.as_slice(), None)
                }
            };
            let read_bytes = if members.is_none() { row_bytes(row) } else { 0 };
            let env = Env { scope: &scope, row, tail: &[], group: members, outer, used_outer: used, output: None };
            if let Some(h) = &core.having
                && tri(&self.eval(h, &env)?) != Some(true)
            {
                continue;
            }
            let mut out = Vec::with_capacity(items.len());
            // The columns computed are kept while the next ones are, and the sort keys after them.
            let mut live = self.limits.live();
            for item in &items {
                let x = match item {
                    Item::Column(i) => row.get(*i).cloned().unwrap_or(Value::Null),
                    Item::Expr(e) => self.eval(e, &env)?,
                };
                live.add(&x)?;
                out.push(x);
            }
            let mut keys = Vec::with_capacity(order.len());
            for o in order {
                let key = match &o.expr {
                    Expr::Lit(Value::I64(n)) => {
                        let i = usize::try_from(*n)
                            .ok()
                            .and_then(|n| n.checked_sub(1))
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
                                let env = Env { output: Some((&columns, &out)), ..env };
                                self.eval(e, &env)?
                            }
                        }
                    }
                };
                live.add(&key)?;
                keys.push(key);
            }
            let bytes = live.bytes() + 2 * size_of::<Vec<Value>>();
            drop(live);
            produced_hold.add(bytes)?;
            produced.push((out, keys));
            held.less(read_bytes);
        }
        // What was read is no longer needed: only the result is held.
        drop((read, groups, held));
        if core.distinct {
            let mut seen = std::collections::HashSet::new();
            let mut keys = self.limits.hold();
            let mut kept = Vec::with_capacity(produced.len());
            for (r, k) in produced {
                self.limits.tick()?;
                let key = row_key(&r);
                keys.add(key.len() + KEY_BYTES)?;
                if seen.insert(key) {
                    kept.push((r, k));
                }
            }
            produced = kept;
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
        let rows: Vec<Vec<Value>> = produced.into_iter().map(|(r, _)| r).collect();
        produced_hold.set(rows_bytes(&rows))?;
        Ok(Rows { columns, rows, hold: produced_hold })
    }

    /// The rows of the tables before `p` (`left`) joined with those of table
    /// `p` (`right`), the pairs `also` accepts: conditions of `WHERE` that can
    /// be answered as soon as table `p` is there. Rows are refused there
    /// instead of being kept until the end; a join without `ON` whose `WHERE`
    /// sets two columns equal is not the product of the tables any more.
    ///
    /// `whole`: `also` is all that was left of `WHERE`, and what it says is
    /// final. Otherwise a condition that fails on a row (a division by zero)
    /// does not fail the statement here, where it is evaluated on rows the
    /// rest of `WHERE` might refuse: the row is kept, the condition marked in
    /// [`Joined::failed`], and it is asked again on the whole rows.
    ///
    /// `wanted`: what of the rows is needed.
    #[allow(clippy::too_many_arguments)]
    fn join(
        &self,
        scope: &Scope,
        p: usize,
        left: &[Vec<Value>],
        right: &[Vec<Value>],
        join: &Join,
        also: &[&Expr],
        whole: bool,
        wanted: Wanted,
        outer: Option<&Env<'_>>,
        used: &Cell<bool>,
    ) -> Result<Joined> {
        let Wanted { count, first } = wanted;
        // Enough rows were given.
        let full = |out: &Joined| first.is_some_and(|n| out.count >= n);
        let part = &scope.parts[p];
        let mut out = Joined { rows: Vec::new(), hold: self.limits.hold(), count: 0, failed: vec![false; also.len()] };
        let on = match (&join.kind, &join.on) {
            (JoinKind::Cross, _) | (_, None) => None,
            (_, Some(on)) => Some(on),
        };
        let left_join = join.kind == JoinKind::Left && on.is_some();
        if on.is_none() && also.is_empty() && count {
            out.count = left.len().saturating_mul(right.len());
            return Ok(out);
        }
        if full(&out) {
            return Ok(out);
        }
        // A hash join on `right.col = <expression of the left tables>`.
        let mut ons = Vec::new();
        if let Some(on) = on {
            conj(on, &mut ons);
        }
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
        // From ON; or, when the join is not a LEFT one (every row must satisfy WHERE
        // as well), from an equality of two columns in WHERE: `FROM a, b WHERE a.x = b.y`.
        let columns = |e: &Expr| matches!(e, Expr::Bin(Op::Eq, a, b) if matches!((&**a, &**b), (Expr::Col { .. }, Expr::Col { .. })));
        let from_where = also.iter().copied().filter(|w| !left_join && columns(w));
        let mut equi: Option<(usize, &Expr)> = None;
        for o in ons.iter().copied().chain(from_where) {
            let Expr::Bin(Op::Eq, a, b) = o else { continue };
            equi = match (local(a), local(b)) {
                (Some(c), None) if refs_only_left(b) => Some((c, &**b)),
                (None, Some(c)) if refs_only_left(a) => Some((c, &**a)),
                _ => continue,
            };
            break;
        }
        let nulls = vec![Value::Null; part.columns.len()];
        // `l` with `r` after it: ON says whether they match, and WHERE whether the row is kept.
        let pair = |out: &mut Joined, l: &[Value], r: &[Value], matched: &mut bool, null_side: bool| -> Result<()> {
            self.limits.tick()?;
            let env = Env { scope, row: l, tail: r, group: None, outer, used_outer: used, output: None };
            if !null_side {
                if let Some(on) = on
                    && tri(&self.eval(on, &env)?) != Some(true)
                {
                    return Ok(());
                }
                *matched = true;
            }
            for (i, w) in also.iter().enumerate() {
                match self.eval(w, &env) {
                    Ok(v) if tri(&v) == Some(true) => {}
                    Ok(_) => return Ok(()),
                    Err(e) if whole => return Err(e),
                    Err(_) => {
                        out.failed[i] = true;
                        break;
                    }
                }
            }
            out.count += 1;
            if !count {
                let row: Vec<Value> = l.iter().chain(r).cloned().collect();
                out.hold.add(row_bytes(&row))?;
                out.rows.push(row);
            }
            Ok(())
        };
        // A hash join when the right column's values can be told apart by a key.
        match equi.and_then(|(c, left_key)| Some((c, left_key, Family::of(part.kinds[c])?))) {
            Some((c, left_key, family)) => {
                let mut table: HashMap<JoinKey, Vec<usize>> = HashMap::new();
                // Rows whose value is not of the column's type: tried with every row.
                let mut loose: Vec<usize> = Vec::new();
                let mut keys = self.limits.hold();
                for (i, r) in right.iter().enumerate() {
                    match JoinKey::of(&r[c], family) {
                        Ok(Some(k)) => {
                            keys.add(k.bytes() + KEY_BYTES)?;
                            table.entry(k).or_default().push(i);
                        }
                        Ok(None) => {}
                        Err(()) => loose.push(i),
                    }
                }
                for l in left {
                    if full(&out) {
                        break;
                    }
                    self.limits.tick()?;
                    let env = Env { scope, row: l, tail: &[], group: None, outer, used_outer: used, output: None };
                    let lk = self.eval(left_key, &env)?;
                    let mut matched = false;
                    match JoinKey::of(&lk, family) {
                        Ok(Some(k)) => {
                            let found = table.get(&k).map(Vec::as_slice).unwrap_or_default();
                            // The rows of the key and the loose ones, in their order.
                            let (mut a, mut b) = (found.iter().copied().peekable(), loose.iter().copied().peekable());
                            loop {
                                let i = match (a.peek(), b.peek()) {
                                    (Some(&x), Some(&y)) if x < y => a.next(),
                                    (Some(_), Some(_)) | (None, Some(_)) => b.next(),
                                    (Some(_), None) => a.next(),
                                    (None, None) => None,
                                };
                                let Some(i) = i else { break };
                                pair(&mut out, l, &right[i], &mut matched, false)?;
                                if full(&out) {
                                    break;
                                }
                            }
                        }
                        // NULL equals nothing.
                        Ok(None) => {}
                        // A value of another type than the column's: SQL may still find it
                        // equal to some of them (`1` and `'01'`), each one is asked.
                        Err(()) => {
                            for r in right {
                                pair(&mut out, l, r, &mut matched, false)?;
                                if full(&out) {
                                    break;
                                }
                            }
                        }
                    }
                    if !matched && left_join {
                        pair(&mut out, l, &nulls, &mut matched, true)?;
                    }
                }
            }
            None => {
                for l in left {
                    if full(&out) {
                        break;
                    }
                    let mut matched = false;
                    for r in right {
                        pair(&mut out, l, r, &mut matched, false)?;
                        if full(&out) {
                            break;
                        }
                    }
                    if !matched && left_join {
                        pair(&mut out, l, &nulls, &mut matched, true)?;
                    }
                }
            }
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Statements
    // -----------------------------------------------------------------------

    /// Runs one statement: under its deadline, within the memory of the
    /// request, and with no memory of the statements before it.
    pub(crate) fn run(&self, stmt: &Stmt) -> Result<SqlResult> {
        let _at_work = self.limits.begin();
        let before = self.limits.held();
        // What a statement remembers of its subqueries is its own: the next one reads again.
        self.cache.borrow_mut().clear();
        let result = self.statement(stmt);
        self.cache.borrow_mut().clear();
        // Of what it held, only its answer stays, until the request has answered.
        self.limits.settle(before.saturating_add(result.as_ref().map_or(0, |r| rows_bytes(&r.rows))));
        result
    }

    fn statement(&self, stmt: &Stmt) -> Result<SqlResult> {
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
        self.eval(
            e,
            &Env { scope: &scope, row: &[], tail: &[], group: None, outer: None, used_outer: &used, output: None },
        )
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
        let mut hold = self.limits.hold();
        let used = Cell::new(false);
        for r in rows {
            self.limits.tick()?;
            let env = Env { scope, row: r, tail: &[], group: None, outer: None, used_outer: &used, output: None };
            let mut row = Vec::with_capacity(columns_of.len());
            let mut live = self.limits.live();
            for c in &columns_of {
                let x = match c {
                    Err(i) => r[*i].clone(),
                    Ok(e) => self.eval(e, &env)?,
                };
                live.add(&x)?;
                row.push(x);
            }
            let bytes = live.bytes() + size_of::<Vec<Value>>();
            drop(live);
            hold.add(bytes)?;
            out.push(row);
        }
        Ok(SqlResult { columns: unique_headers(names), rows: out, returns_rows: true, ..SqlResult::default() })
    }

    /// The rows of `schema` matching `filter`, given to `each` a chunk at a
    /// time; returns how many. Only the primary keys of the rows the indexable
    /// part of the filter names are held, and one chunk of rows: an `UPDATE` or
    /// a `DELETE` of a whole table holds no more. A rest of the filter with a
    /// subquery (it may read the rows being changed) is asked of every row
    /// before any is changed, as one chunk.
    fn each_target(
        &self,
        schema: &'static TableSchema,
        alias: Option<&str>,
        filter: Option<&Expr>,
        mut each: impl FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<usize> {
        let scope = Scope::of_schema(alias.unwrap_or(schema.name), schema);
        let used = Cell::new(false);
        let mut parts = Vec::new();
        if let Some(f) = filter {
            conj(f, &mut parts);
        }
        let mut pushed = Vec::new();
        let mut residual = Vec::new();
        for p in parts {
            match self.to_m(p, &scope, Some(0), None, &used) {
                Some(m) => pushed.push(m),
                None => residual.push(p),
            }
        }
        // The rows that the rest of the filter accepts.
        let keep = |rows: Vec<Vec<Value>>, hold: &mut Hold| -> Result<Vec<Vec<Value>>> {
            if residual.is_empty() {
                return Ok(rows);
            }
            let mut kept = Vec::new();
            for r in rows {
                self.limits.tick()?;
                let env = Env {
                    scope: &scope,
                    row: &r,
                    tail: &[],
                    group: None,
                    outer: None,
                    used_outer: &used,
                    output: None,
                };
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
            hold.set(rows_bytes(&kept))?;
            Ok(kept)
        };
        let reads_rows = residual.iter().any(|w| {
            let mut sub = false;
            w.walk(&mut |x| sub |= matches!(x, Expr::Exists(..) | Expr::Scalar(_) | Expr::InSelect(..)));
            sub
        });
        if reads_rows {
            let rows = self.read_table(schema, and_all(pushed), None)?;
            let mut hold = self.limits.hold_rows(&rows)?;
            let rows = keep(rows, &mut hold)?;
            let n = rows.len();
            each(rows)?;
            return Ok(n);
        }
        let c = &schema.columns[schema.primary_key];
        let key = M::Column(ColumnRef { table: schema.name, name: c.name, index: schema.primary_key, kind: c.kind });
        let mut ast = SelectAst::new(schema);
        ast.filter = and_all(pushed);
        ast.projection = Projection::Exprs(vec![key.clone()]);
        let keys = self.tx.fetch_rows(&Query::Select(&ast))?;
        let _keys_held = self.limits.hold_rows(&keys)?;
        let mut n = 0;
        for chunk in keys.chunks(CHANGE_CHUNK) {
            let list = chunk.iter().filter_map(|r| r.first().cloned()).collect();
            let by_key = M::InList { expr: Box::new(key.clone()), list, negated: false };
            let rows = self.read_table(schema, Some(by_key), None)?;
            let mut hold = self.limits.hold_rows(&rows)?;
            let rows = keep(rows, &mut hold)?;
            n += rows.len();
            each(rows)?;
        }
        Ok(n)
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
                self.limits.take_declined();
                match self.tx.execute_now(&Query::Update(&UpdateAst { table: schema, sets, filter })) {
                    Ok(r) => return Ok(SqlResult { rows_affected: r.rows_affected, ..SqlResult::default() }),
                    // The engine would have held more rows than the statement may (it wrote
                    // nothing): they are changed below, a chunk at a time.
                    Err(_) if self.limits.take_declined() => {}
                    Err(e) => return Err(e),
                }
            }
        }
        let mut after = Vec::new();
        let mut after_hold = self.limits.hold();
        let changed = self.each_target(schema, alias, filter, |rows| {
            for old in &rows {
                self.limits.tick()?;
                let env = Env {
                    scope: &scope,
                    row: old,
                    tail: &[],
                    group: None,
                    outer: None,
                    used_outer: &used,
                    output: None,
                };
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
                // The new rows are kept for RETURNING only.
                if returning.is_some() {
                    after_hold.add(row_bytes(&new))?;
                    after.push(new);
                }
            }
            Ok(())
        })?;
        let mut result = match returning {
            Some(items) => self.returning(items, &scope, &after)?,
            None => SqlResult::default(),
        };
        result.rows_affected = changed as u64;
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
                self.limits.take_declined();
                match self.tx.execute_now(&Query::Delete(&DeleteAst { table: schema, filter })) {
                    Ok(r) => return Ok(SqlResult { rows_affected: r.rows_affected, ..SqlResult::default() }),
                    // As for UPDATE: a chunk at a time below.
                    Err(_) if self.limits.take_declined() => {}
                    Err(e) => return Err(e),
                }
            }
        }
        let mut gone = Vec::new();
        let mut gone_hold = self.limits.hold();
        let deleted = self.each_target(schema, alias, filter, |rows| {
            for r in rows {
                self.limits.tick()?;
                self.tx.execute_now(&Query::DeleteByKey { table: schema, key: &r[schema.primary_key] })?;
                // The rows are kept for RETURNING only.
                if returning.is_some() {
                    gone_hold.add(row_bytes(&r))?;
                    gone.push(r);
                }
            }
            Ok(())
        })?;
        let mut result = match returning {
            Some(items) => {
                let scope = Scope::of_schema(alias.unwrap_or(schema.name), schema);
                self.returning(items, &scope, &gone)?
            }
            None => SqlResult::default(),
        };
        result.rows_affected = deleted as u64;
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
        let (source_rows, _source_hold): (Vec<Vec<Value>>, Hold) = match source {
            InsertSource::Values(rows) => {
                // Each row counted as it is computed: a row of large values fails before the next one is.
                let mut values = Vec::with_capacity(rows.len());
                let mut hold = self.limits.hold();
                for r in rows {
                    self.limits.tick()?;
                    let mut row = Vec::with_capacity(r.len());
                    let mut live = self.limits.live();
                    for e in r {
                        let x = self.eval_const(e)?;
                        live.add(&x)?;
                        row.push(x);
                    }
                    let bytes = live.bytes() + size_of::<Vec<Value>>();
                    drop(live);
                    hold.add(bytes)?;
                    values.push(row);
                }
                (values, hold)
            }
            InsertSource::Select(sel) => {
                let Rows { rows, hold, .. } = self.select(sel, None, &Cell::new(false))?;
                (rows, hold)
            }
            InsertSource::Defaults => (vec![Vec::new()], self.limits.hold()),
        };
        let mut rows = Vec::with_capacity(source_rows.len());
        let mut rows_hold = self.limits.hold();
        for r in source_rows {
            self.limits.tick()?;
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
            let row = self.full_row(schema, &given)?;
            rows_hold.add(row_bytes(&row))?;
            rows.push(row);
        }
        let pk = schema.primary_key;
        let scope = Scope::of_schema(schema.name, schema);
        // Plain inserts: one engine statement (keys given for an
        // auto-increment column go one by one, below).
        let explicit_keys = schema.columns[pk].auto_increment && rows.iter().any(|r| !r[pk].is_null());
        if on_conflict.is_none() && !replace && returning.is_none() && !explicit_keys {
            let count = rows.len() as u64;
            let last_given = rows.last().map(|r| r[pk].clone()).filter(|v| !v.is_null());
            let r: ExecResult = if rows.len() == 1 {
                self.tx.execute_now(&Query::InsertRow { table: schema, values: &rows[0] })?
            } else {
                // The rows are given, not copied: a large INSERT is held once.
                self.tx.execute_now(&Query::Insert(&memoire_orm::ast::InsertAst { table: schema, rows }))?
            };
            return Ok(SqlResult {
                rows_affected: r.rows_affected.max(count),
                last_insert_id: r.last_insert_id.or(last_given),
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
        let mut done_hold = self.limits.hold();
        for row in rows {
            self.limits.tick()?;
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
                        tail: &[],
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
                    if returning.is_some() {
                        done_hold.add(row_bytes(&new))?;
                        done.push(new);
                    }
                }
                (Some(old), None) => {
                    // REPLACE: the old row goes, the new one takes its place.
                    self.tx.execute_now(&Query::DeleteByKey { table: schema, key: &old[pk] })?;
                    let id = self.put_new(schema, &row)?;
                    last = Some(id.clone());
                    affected += 1;
                    if returning.is_some() {
                        let stored = self.by_key(schema, &id)?.unwrap_or(row);
                        done_hold.add(row_bytes(&stored))?;
                        done.push(stored);
                    }
                }
                (None, _) => {
                    let id = self.put_new(schema, &row)?;
                    last = Some(id.clone());
                    affected += 1;
                    if returning.is_some() {
                        let stored = self.by_key(schema, &id)?.unwrap_or(row);
                        done_hold.add(row_bytes(&stored))?;
                        done.push(stored);
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
        // The rows of `AS SELECT` are held until they are written.
        let mut _rows_hold = None;
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
            _rows_hold = Some(r.hold);
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
                self.limits.tick()?;
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
