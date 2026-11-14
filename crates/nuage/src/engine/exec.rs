//! Statements on the store: Memoire's evaluator over rows read from disk.
//!
//! Only the rows a statement needs are read: primary-key and index lookups
//! read the matching rows, other filters stream the table in chunks and keep
//! the matching rows, `LIMIT` without `ORDER BY` stops early, `ORDER BY ...
//! LIMIT n` keeps the best `n` rows while streaming, and `COUNT(*)` without a
//! filter reads a counter. The rows kept are then evaluated by Memoire's
//! in-memory engine ([`memoire_orm::evaluator`]), so every statement behaves
//! exactly as on `MemoryDb`. Memory use follows the rows a statement keeps
//! (its result, or the rows a join or a `GROUP BY` combines), not the size of
//! the tables.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, OnceLock};

use memoire_orm::ast::{Projection, SelectAst};
use memoire_orm::evaluator::{self, Tables};
use memoire_orm::nuage::wire::{SchemaCache, schema_to_json};
use memoire_orm::{AggFunc, BinOp, ColumnType, Error, ExecResult, Expr, JoinKind, Query, Result, TableSchema, Value};
use serde_json::{Value as Json, json};

use super::keys::{self, push_value};
use super::store::{Overlay, Parsed, Snapshot, Store};
use super::wal::Op;

/// Rows read per step of a table scan.
pub(crate) const CHUNK: usize = 2048;

/// A stored index: its columns (by name) and whether it is unique. Indexes
/// are numbered by their place in the list (their keys carry it): a dropped
/// index stays as a tombstone with no column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexMeta {
    pub columns: Vec<String>,
    pub unique: bool,
    /// The name `CREATE INDEX` gave it.
    pub name: Option<String>,
}

impl IndexMeta {
    pub(crate) fn live(&self) -> bool {
        !self.columns.is_empty()
    }
}

/// A table as the catalog records it.
#[derive(Clone, Debug)]
pub(crate) struct TableMeta {
    pub generation: u64,
    /// Stored columns, in storage order.
    pub columns: Vec<String>,
    /// Position of the primary key in `columns`.
    pub pk: usize,
    pub indexes: Vec<IndexMeta>,
    /// The schema the table was created (or last widened) with, as JSON.
    pub schema: Json,
    /// The type of each stored column, worked out from `schema` once.
    kinds: OnceLock<Vec<Option<ColumnType>>>,
}

impl TableMeta {
    fn new(schema: &TableSchema, generation: u64) -> TableMeta {
        let name = |i: usize| schema.columns[i].name.to_owned();
        let mut indexes = Vec::new();
        // Same order as the in-memory engine: unique first, then the others.
        for unique in [true, false] {
            for (i, c) in schema.columns.iter().enumerate() {
                if i != schema.primary_key && c.unique == unique && (unique || c.indexed) {
                    indexes.push(IndexMeta { columns: vec![name(i)], unique, name: None });
                }
            }
            for ix in schema.indexes.iter().filter(|ix| ix.unique == unique) {
                let index_name = (!ix.name.is_empty()).then(|| ix.name.to_owned());
                indexes.push(IndexMeta {
                    columns: ix.columns.iter().map(|&i| name(i)).collect(),
                    unique,
                    name: index_name,
                });
            }
        }
        TableMeta {
            generation,
            columns: schema.columns.iter().map(|c| c.name.to_owned()).collect(),
            pk: schema.primary_key,
            indexes,
            schema: schema_to_json(schema),
            kinds: OnceLock::new(),
        }
    }

    /// The type of each stored column, from the table's schema (`None` for a
    /// column the schema does not name).
    pub(crate) fn kinds(&self) -> &[Option<ColumnType>] {
        self.kinds.get_or_init(|| self.read_kinds())
    }

    /// The columns or the schema changed: their types are read again.
    fn changed(&mut self) {
        self.kinds = OnceLock::new();
    }

    fn read_kinds(&self) -> Vec<Option<ColumnType>> {
        const NAMES: [(&str, ColumnType); 14] = [
            ("bool", ColumnType::Bool),
            ("smallint", ColumnType::SmallInt),
            ("int", ColumnType::Int),
            ("bigint", ColumnType::BigInt),
            ("real", ColumnType::Real),
            ("double", ColumnType::Double),
            ("text", ColumnType::Text),
            ("bytes", ColumnType::Bytes),
            ("uuid", ColumnType::Uuid),
            ("timestamp", ColumnType::Timestamp),
            ("timestamptz", ColumnType::TimestampTz),
            ("date", ColumnType::Date),
            ("time", ColumnType::Time),
            ("json", ColumnType::Json),
        ];
        let declared: Vec<(&str, &str)> = self
            .schema
            .get("columns")
            .and_then(Json::as_array)
            .map(|cols| {
                cols.iter().filter_map(|c| Some((c.get("name")?.as_str()?, c.get("kind")?.as_str()?))).collect()
            })
            .unwrap_or_default();
        self.columns
            .iter()
            .map(|name| {
                let kind = declared.iter().find(|(n, _)| n == name)?.1;
                NAMES.iter().find(|(n, _)| *n == kind).map(|(_, k)| *k)
            })
            .collect()
    }

    fn to_json(&self) -> Vec<u8> {
        json!({
            "generation": self.generation,
            "columns": self.columns,
            "pk": self.pk,
            "indexes": self.indexes.iter().map(|i| json!({ "columns": i.columns, "unique": i.unique, "name": i.name })).collect::<Vec<_>>(),
            "schema": self.schema,
        })
        .to_string()
        .into_bytes()
    }

    pub(crate) fn from_json(bytes: &[u8]) -> Result<TableMeta> {
        let bad = || super::storage_err("invalid table metadata");
        let j: Json = serde_json::from_slice(bytes).map_err(|_| bad())?;
        let strings = |v: Option<&Json>| -> Result<Vec<String>> {
            v.and_then(Json::as_array)
                .ok_or_else(bad)?
                .iter()
                .map(|s| s.as_str().map(str::to_owned).ok_or_else(bad))
                .collect()
        };
        Ok(TableMeta {
            generation: j.get("generation").and_then(Json::as_u64).ok_or_else(bad)?,
            columns: strings(j.get("columns"))?,
            pk: j.get("pk").and_then(Json::as_u64).ok_or_else(bad)? as usize,
            indexes: j
                .get("indexes")
                .and_then(Json::as_array)
                .ok_or_else(bad)?
                .iter()
                .map(|i| {
                    Ok(IndexMeta {
                        columns: strings(i.get("columns"))?,
                        unique: i.get("unique").and_then(Json::as_bool).unwrap_or(false),
                        name: i.get("name").and_then(Json::as_str).map(str::to_owned),
                    })
                })
                .collect::<Result<_>>()?,
            schema: j.get("schema").cloned().unwrap_or(Json::Null),
            kinds: OnceLock::new(),
        })
    }

    fn position(&self, column: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == column)
    }

    /// For each column of `schema`, its position in the stored row.
    fn map(&self, schema: &TableSchema) -> Vec<Option<usize>> {
        schema.columns.iter().map(|c| self.position(c.name)).collect()
    }

    fn index_positions(&self, index: &IndexMeta) -> Option<Vec<usize>> {
        index.columns.iter().map(|c| self.position(c)).collect()
    }
}

/// One statement (or transaction) at work on a branch: what it reads, what
/// it writes.
pub(crate) struct Ctx<'a> {
    pub store: &'a Store,
    pub snap: &'a Snapshot,
    pub branch: &'a str,
    pub overlay: &'a mut Overlay,
    /// Operations applied after the overlay (dropped table generations).
    pub extra: &'a mut Vec<Op>,
    /// Tables read or written: what a transaction's commit is checked against.
    pub touched: &'a mut BTreeSet<String>,
    pub written: &'a mut BTreeSet<String>,
    /// Catalog entries already read, by table: a statement asks for the same
    /// ones several times. Emptied when the catalog is written.
    pub memo: &'a mut Vec<(String, Arc<Parsed>)>,
}

fn pk_enc(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    push_value(&mut out, v);
    out
}

fn row_key_raw(generation: u64, pk: &[u8]) -> Vec<u8> {
    let mut k = keys::data_prefix(generation, keys::ROW);
    k.extend_from_slice(pk);
    k
}

impl Ctx<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.snap.get(Some(self.overlay), key)
    }

    fn put(&mut self, key: Vec<u8>, value: Option<Vec<u8>>) {
        if key.first() == Some(&keys::CATALOG) {
            self.memo.clear();
        }
        self.overlay.insert(key, value);
    }

    fn touch(&mut self, table: &str) {
        if !self.touched.contains(table) {
            self.touched.insert(table.to_owned());
        }
    }

    fn parsed(&mut self, table: &str) -> Result<Option<Arc<Parsed>>> {
        if let Some((_, parsed)) = self.memo.iter().find(|(name, _)| name == table) {
            return Ok(Some(parsed.clone()));
        }
        self.touch(table);
        let key = keys::catalog_key(table);
        let parsed = match self.overlay.get(&key) {
            // An entry this transaction wrote: by its bytes.
            Some(pending) => pending.clone().map(|b| self.store.parsed(b)).transpose()?,
            // The stored one: read in place, and the same as last time for nearly every statement.
            None => self.snap.with(&key, |bytes| self.store.parsed_as(table, bytes))?.transpose()?,
        };
        if let Some(p) = &parsed {
            self.memo.push((table.to_owned(), p.clone()));
        }
        Ok(parsed)
    }

    /// The table's metadata (parsed once per distinct catalog entry, see [`Store::parsed`]).
    pub(crate) fn meta(&mut self, table: &str) -> Result<Option<Arc<TableMeta>>> {
        Ok(self.parsed(table)?.map(|p| p.meta.clone()))
    }

    /// The table's metadata, to change it.
    fn meta_mut(&mut self, table: &str) -> Result<Option<TableMeta>> {
        Ok(self.meta(table)?.map(|m| (*m).clone()))
    }

    /// The table's `'static` schema, made once per distinct catalog entry.
    pub(crate) fn table_schema(&mut self, table: &str, schemas: &SchemaCache) -> Result<Option<&'static TableSchema>> {
        let Some(p) = self.parsed(table)? else { return Ok(None) };
        if let Some(s) = p.schema.get() {
            return Ok(Some(s));
        }
        let s = schemas.schema(&p.meta.schema)?;
        Ok(Some(p.schema.get_or_init(|| s)))
    }

    fn put_meta(&mut self, table: &str, meta: &TableMeta) {
        self.put(keys::catalog_key(table), Some(meta.to_json()));
    }

    fn counter(&self, key: &[u8]) -> Result<i64> {
        Ok(self.get(key)?.map_or(0, |b| keys::decode_i64(&b)))
    }

    /// Every table of the branch.
    pub(crate) fn table_names(&self) -> Result<Vec<String>> {
        let (start, end) = keys::catalog_range();
        let mut out = Vec::new();
        let mut at = start;
        loop {
            let (entries, next) = self.snap.scan(Some(self.overlay), &at, &end, CHUNK)?;
            out.extend(entries.iter().filter_map(|(k, _)| keys::catalog_name(k)));
            match next {
                Some(n) => at = n,
                None => return Ok(out),
            }
        }
    }

    pub(crate) fn row_count(&self, meta: &TableMeta) -> Result<i64> {
        self.counter(&keys::count_key(meta.generation))
    }

    /// Bytes of the table's stored rows.
    pub(crate) fn table_bytes(&self, meta: &TableMeta) -> Result<i64> {
        self.counter(&keys::bytes_key(meta.generation))
    }

    /// A stored row in the order of the model's columns. `want`: the stored
    /// columns to read (see [`wanted`]), all of them when `None`.
    fn decode(&self, want: Option<&[bool]>, map: &[Option<usize>], bytes: &[u8]) -> Result<Vec<Value>> {
        let mut stored = match want {
            Some(want) => keys::decode_row_some(bytes, want)?,
            None => keys::decode_row(bytes)?,
        };
        // The model's columns in the stored order (nearly always): the row as it is.
        if stored.len() == map.len() && map.iter().enumerate().all(|(i, m)| *m == Some(i)) {
            return Ok(stored);
        }
        Ok(map.iter().map(|m| m.and_then(|i| stored.get_mut(i).map(std::mem::take)).unwrap_or(Value::Null)).collect())
    }

    fn rows_by_pk(
        &self,
        meta: &TableMeta,
        map: &[Option<usize>],
        want: Option<&[bool]>,
        pks: &[Vec<u8>],
    ) -> Result<Vec<Vec<Value>>> {
        let mut out = Vec::with_capacity(pks.len());
        for pk in pks {
            if let Some(b) = self.get(&row_key_raw(meta.generation, pk))? {
                out.push(self.decode(want, map, &b)?);
            }
        }
        Ok(out)
    }

    /// Primary keys of the rows whose index `index` holds `values`.
    fn index_lookup(&self, meta: &TableMeta, index: usize, values: &[&Value]) -> Result<Vec<Vec<u8>>> {
        let prefix = keys::index_prefix(meta.generation, index as u16, values);
        let end = keys::prefix_end(&prefix);
        let mut out = Vec::new();
        self.snap.each(Some(self.overlay), &prefix, &end, |k, _| {
            out.push(k[prefix.len()..].to_vec());
            Ok(true)
        })?;
        Ok(out)
    }

    /// Number of rows whose index `index` holds `values`: no key is kept.
    fn index_count(&self, meta: &TableMeta, index: usize, values: &[&Value]) -> Result<i64> {
        let prefix = keys::index_prefix(meta.generation, index as u16, values);
        let end = keys::prefix_end(&prefix);
        let mut n = 0;
        self.snap.each(Some(self.overlay), &prefix, &end, |_, _| {
            n += 1;
            Ok(true)
        })?;
        Ok(n)
    }

    /// Calls `f` with each chunk of rows of the table, in primary-key order,
    /// until it returns `false`.
    fn scan_rows(
        &self,
        meta: &TableMeta,
        map: &[Option<usize>],
        want: Option<&[bool]>,
        mut f: impl FnMut(Vec<Vec<Value>>) -> Result<bool>,
    ) -> Result<()> {
        let (start, end) = keys::rows_range(meta.generation);
        let mut rows = Vec::new();
        let mut stopped = false;
        // Rows are decoded where they are read, a chunk at a time.
        self.snap.each(Some(self.overlay), &start, &end, |_, v| {
            rows.push(self.decode(want, map, v)?);
            if rows.len() == CHUNK && !f(std::mem::take(&mut rows))? {
                stopped = true;
            }
            Ok(!stopped)
        })?;
        if !stopped && !rows.is_empty() {
            f(rows)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

fn conjuncts(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Binary { op: BinOp::And, left, right } => {
            let mut v = conjuncts(left);
            v.extend(conjuncts(right));
            v
        }
        other => vec![other],
    }
}

fn and_all(parts: &[&Expr]) -> Option<Expr> {
    let mut it = parts.iter();
    let first = (*it.next()?).clone();
    Some(it.fold(first, |acc, e| Expr::binary(BinOp::And, acc, (*e).clone())))
}

/// Tables whose columns `e` reads, outside subqueries.
fn tables_read(e: &Expr, out: &mut BTreeSet<&'static str>) {
    match e {
        Expr::Column(c) => {
            out.insert(c.table);
        }
        Expr::Value(_) => {}
        Expr::Binary { left, right, .. } => {
            tables_read(left, out);
            tables_read(right, out);
        }
        Expr::Not(x) | Expr::IsNull { expr: x, .. } | Expr::InList { expr: x, .. } => tables_read(x, out),
        Expr::Between { expr, low, high, .. } => {
            tables_read(expr, out);
            tables_read(low, out);
            tables_read(high, out);
        }
        Expr::Like { expr, pattern, .. } => {
            tables_read(expr, out);
            tables_read(pattern, out);
        }
        Expr::Match(m) => tables_read(&Expr::Column(m_column(m)), out),
        Expr::Aggregate { arg, .. } => {
            if let Some(a) = arg {
                tables_read(a, out);
            }
            // An aggregate is never a row filter.
            out.insert("");
        }
        Expr::InSubquery { expr, .. } => tables_read(expr, out),
        #[allow(unreachable_patterns)]
        _ => {
            out.insert("");
        }
    }
}

/// The column a full-text search reads (its debug form names it; the
/// in-memory engine needs no more).
fn m_column(m: &memoire_orm::TextSearch) -> memoire_orm::ColumnRef {
    match m.expr() {
        Expr::Column(c) => *c,
        _ => memoire_orm::ColumnRef { table: "", name: "", index: 0, kind: memoire_orm::ColumnType::Text },
    }
}

/// The tables of the subqueries of a statement, recursively.
fn subquery_tables(ast: &SelectAst, out: &mut Vec<&'static TableSchema>) {
    fn walk(e: &Expr, out: &mut Vec<&'static TableSchema>) {
        match e {
            Expr::InSubquery { expr, query, .. } => {
                walk(expr, out);
                for t in query.tables() {
                    if !out.iter().any(|o| o.name == t.name) {
                        out.push(t);
                    }
                }
                subquery_tables(query, out);
            }
            Expr::Binary { left, right, .. } => {
                walk(left, out);
                walk(right, out);
            }
            Expr::Not(x) => walk(x, out),
            _ => {}
        }
    }
    for e in ast.filter.iter().chain(ast.having.iter()).chain(ast.joins.iter().map(|j| &j.on)) {
        walk(e, out);
    }
    if let Projection::Exprs(list) = &ast.projection {
        for e in list {
            walk(e, out);
        }
    }
}

fn filter_tables(filter: Option<&Expr>, out: &mut Vec<&'static TableSchema>) {
    if let Some(f) = filter {
        let mut fake = SelectAst::new(&EMPTY);
        fake.filter = Some(f.clone());
        subquery_tables(&fake, out);
    }
}

static EMPTY: TableSchema = TableSchema {
    name: "",
    columns: &[],
    primary_key: 0,
    indexes: &[],
    sql: [memoire_orm::StaticSql { columns: "", select: "", find: "", insert: "", update: "", delete: "", upsert: "" };
        3],
};

/// A column a lookup went by (its place in the schema) and the values it may hold.
struct Pin<'a> {
    column: usize,
    allowed: Vec<&'a Value>,
    /// The allowed values by their key form, made when there are many of them.
    by_key: std::cell::OnceCell<HashMap<Vec<u8>, &'a Value>>,
}

impl<'a> Pin<'a> {
    fn new(column: usize, allowed: Vec<&'a Value>) -> Pin<'a> {
        Pin { column, allowed, by_key: std::cell::OnceCell::new() }
    }

    /// `held` is exactly one of the allowed values.
    fn holds(&self, held: &Value) -> bool {
        // NULL equals nothing and a float may be NaN: those go to the evaluator.
        if matches!(held, Value::Null | Value::F64(_)) {
            return false;
        }
        if self.allowed.len() <= 8 {
            return self.allowed.contains(&held);
        }
        let by_key = self.by_key.get_or_init(|| self.allowed.iter().map(|v| (pk_enc(v), *v)).collect());
        by_key.get(&pk_enc(held)).is_some_and(|v| *v == held)
    }
}

/// Keys found through an index.
struct Pushed<'a> {
    /// Encoded primary keys of the rows to read.
    keys: Vec<Vec<u8>>,
    /// When the lookup stands for the whole filter: the columns it went by.
    /// A row that holds exactly the values it was looked up by matches,
    /// without asking the evaluator.
    whole: Option<Vec<Pin<'a>>>,
}

impl Pushed<'_> {
    /// Every row holds exactly the values it was looked up by, and the lookup was the whole filter.
    fn settles(&self, rows: &[Vec<Value>]) -> bool {
        self.whole.as_ref().is_some_and(|pins| {
            rows.iter().all(|row| pins.iter().all(|pin| row.get(pin.column).is_some_and(|held| pin.holds(held))))
        })
    }
}

/// The columns of `schema` a `SELECT` reads, by their place in the schema:
/// the others need not be decoded. `None` when it may read any of them
/// (every column is selected, or an expression this does not look into).
fn columns_read(ast: &SelectAst, schema: &TableSchema) -> Option<Vec<bool>> {
    fn mark(e: &Expr, schema: &TableSchema, mask: &mut [bool]) -> Option<()> {
        match e {
            Expr::Column(c) => {
                if c.table == schema.name {
                    *mask.get_mut(c.index)? = true;
                }
                Some(())
            }
            Expr::Value(_) => Some(()),
            Expr::Binary { left, right, .. } => {
                mark(left, schema, mask)?;
                mark(right, schema, mask)
            }
            Expr::Not(x) | Expr::IsNull { expr: x, .. } | Expr::InList { expr: x, .. } => mark(x, schema, mask),
            Expr::Between { expr, low, high, .. } => {
                mark(expr, schema, mask)?;
                mark(low, schema, mask)?;
                mark(high, schema, mask)
            }
            Expr::Like { expr, pattern, .. } => {
                mark(expr, schema, mask)?;
                mark(pattern, schema, mask)
            }
            Expr::Aggregate { arg, .. } => match arg {
                Some(a) => mark(a, schema, mask),
                None => Some(()),
            },
            // A subquery (it may name this table's columns), a text search, anything else.
            #[allow(unreachable_patterns)]
            Expr::InSubquery { .. } | Expr::Match(_) | _ => None,
        }
    }
    let mut mask = vec![false; schema.columns.len()];
    // The primary key always: rows are told apart by it.
    *mask.get_mut(schema.primary_key)? = true;
    match &ast.projection {
        Projection::Tables(tables) => {
            if tables.iter().any(|t| t.name == schema.name) {
                return None;
            }
        }
        Projection::Exprs(list) => {
            for e in list {
                mark(e, schema, &mut mask)?;
            }
        }
    }
    let others = ast
        .filter
        .iter()
        .chain(ast.having.iter())
        .chain(ast.group_by.iter())
        .chain(ast.order.iter().map(|o| &o.expr))
        .chain(ast.joins.iter().map(|j| &j.on));
    for e in others {
        mark(e, schema, &mut mask)?;
    }
    // Every column: the plain decoding is the fastest.
    if mask.iter().all(|m| *m) { None } else { Some(mask) }
}

/// [`columns_read`] as places in the stored row.
fn wanted(needed: Option<&[bool]>, meta: &TableMeta, map: &[Option<usize>]) -> Option<Vec<bool>> {
    let needed = needed?;
    let mut want = vec![false; meta.columns.len()];
    for (i, m) in map.iter().enumerate() {
        if let Some(p) = m
            && needed.get(i).copied().unwrap_or(true)
            && let Some(slot) = want.get_mut(*p)
        {
            *slot = true;
        }
    }
    Some(want)
}

/// Keys found through an index for the pinned conjuncts, when there is one.
fn pushdown<'e>(
    ctx: &Ctx<'_>,
    meta: &TableMeta,
    schema: &TableSchema,
    parts: &[&'e Expr],
) -> Result<Option<Pushed<'e>>> {
    let column = |e: &Expr| match e {
        Expr::Column(c) if c.table == schema.name => Some((schema.columns.get(c.index)?.name, c.index)),
        _ => None,
    };
    // The lookup is the whole filter when it used the only conjunct.
    let only = parts.len() == 1;
    let mut pins: HashMap<&str, (&'e Value, usize)> = HashMap::new();
    let mut pinned = 0;
    for part in parts {
        match part {
            Expr::Binary { op: BinOp::Eq, left, right } => {
                let pin = match (&**left, &**right) {
                    (l, Expr::Value(v)) => column(l).map(|c| (c, v)),
                    (Expr::Value(v), r) => column(r).map(|c| (c, v)),
                    _ => None,
                };
                if let Some(((c, at), v)) = pin {
                    if c == meta.columns[meta.pk] {
                        return Ok(Some(Pushed {
                            keys: vec![pk_enc(v)],
                            whole: only.then(|| vec![Pin::new(at, vec![v])]),
                        }));
                    }
                    pins.insert(c, (v, at));
                    pinned += 1;
                }
            }
            Expr::InList { expr, list, negated: false } => {
                if let Some((c, at)) = column(expr) {
                    let whole = only.then(|| vec![Pin::new(at, list.iter().collect())]);
                    if c == meta.columns[meta.pk] {
                        let mut keys: Vec<Vec<u8>> = list.iter().map(pk_enc).collect();
                        keys.sort();
                        keys.dedup();
                        return Ok(Some(Pushed { keys, whole }));
                    }
                    if let Some(i) = meta.indexes.iter().position(|ix| ix.columns.len() == 1 && ix.columns[0] == c) {
                        let mut keys = Vec::new();
                        for v in list {
                            keys.extend(ctx.index_lookup(meta, i, &[v])?);
                        }
                        keys.sort();
                        keys.dedup();
                        return Ok(Some(Pushed { keys, whole }));
                    }
                }
            }
            _ => {}
        }
    }
    // The index with the most columns that are all pinned.
    let best = meta
        .indexes
        .iter()
        .enumerate()
        .filter(|(_, ix)| ix.live() && ix.columns.iter().all(|c| pins.contains_key(c.as_str())))
        .max_by_key(|(_, ix)| ix.columns.len());
    match best {
        Some((i, ix)) => {
            let values: Vec<&Value> = ix.columns.iter().map(|c| pins[c.as_str()].0).collect();
            // NULL is never indexed (and never equal).
            if values.iter().any(|v| v.is_null()) {
                return Ok(Some(Pushed { keys: Vec::new(), whole: None }));
            }
            // The whole filter: every conjunct pins a column of its own, and the index has them all.
            let whole = (pinned == parts.len() && pins.len() == pinned && ix.columns.len() == pinned)
                .then(|| ix.columns.iter().map(|c| Pin::new(pins[c.as_str()].1, vec![pins[c.as_str()].0])).collect());
            Ok(Some(Pushed { keys: ctx.index_lookup(meta, i, &values)?, whole }))
        }
        None => Ok(None),
    }
}

/// `Some(descending)` when `order` is the primary key of `schema` alone.
fn pk_order(order: &[memoire_orm::Order], schema: &TableSchema) -> Option<bool> {
    match order {
        [o] => match &o.expr {
            Expr::Column(c) if c.table == schema.name && c.index == schema.primary_key => Some(o.desc),
            _ => None,
        },
        _ => None,
    }
}

/// When a single-table `SELECT` may stop reading early.
#[derive(Clone)]
enum Stop {
    Never,
    /// No order: the first `n` matches are the answer.
    First(usize),
    /// `ORDER BY ... LIMIT`: keep the best `n` while reading.
    Best(Vec<memoire_orm::Order>, usize),
}

/// Rows of `schema` matching `parts`, read through an index when possible.
/// `needed`: the columns the statement reads (see [`columns_read`]); the
/// others come out `NULL`.
fn matching_rows(
    ctx: &mut Ctx<'_>,
    schema: &'static TableSchema,
    parts: &[&Expr],
    side: &Tables,
    side_schemas: &[&'static TableSchema],
    stop: Stop,
    needed: Option<&[bool]>,
) -> Result<Option<(Vec<Vec<Value>>, i64)>> {
    let Some(meta) = ctx.meta(schema.name)? else { return Ok(None) };
    let map = meta.map(schema);
    let want = wanted(needed, &meta, &map);
    let want = want.as_deref();
    let seq = ctx.counter(&keys::seq_key(meta.generation))?;
    let filter = and_all(parts);
    // Keeps the rows of `chunk` that match, evaluated by Memoire.
    let keep = |chunk: Vec<Vec<Value>>, ast: &SelectAst| -> Result<Vec<Vec<Value>>> {
        let mut t = side.clone();
        t.insert(schema, chunk, seq);
        evaluator::fetch(&t, Query::Select(ast))
    };
    let mut select = SelectAst::new(schema);
    select.filter = filter.clone();
    let _ = side_schemas;
    if let Some(mut pushed) = pushdown(ctx, &meta, schema, parts)? {
        let mut pks = std::mem::take(&mut pushed.keys);
        // The keys come in primary-key order: for `LIMIT n` with no order, or
        // ordered by the primary key, read rows in that order (or reversed)
        // and stop at the `n`th match, instead of reading every match.
        let early = match &stop {
            Stop::First(n) => Some((*n, false)),
            Stop::Best(order, n) => pk_order(order, schema).map(|desc| (*n, desc)),
            Stop::Never => None,
        };
        if let Some((n, desc)) = early
            && pks.len() > n
        {
            if desc {
                pks.reverse();
            }
            let mut out = Vec::with_capacity(n);
            for chunk in pks.chunks(n.max(16)) {
                let rows = ctx.rows_by_pk(&meta, &map, want, chunk)?;
                out.extend(if filter.is_some() && !pushed.settles(&rows) { keep(rows, &select)? } else { rows });
                if out.len() >= n {
                    break;
                }
            }
            return Ok(Some((out, seq)));
        }
        let rows = ctx.rows_by_pk(&meta, &map, want, &pks)?;
        let rows = if filter.is_some() && !pushed.settles(&rows) { keep(rows, &select)? } else { rows };
        return Ok(Some((rows, seq)));
    }
    let mut out: Vec<Vec<Value>> = Vec::new();
    ctx.scan_rows(&meta, &map, want, |chunk| {
        let matched = if filter.is_some() { keep(chunk, &select)? } else { chunk };
        out.extend(matched);
        Ok(match &stop {
            Stop::Never => true,
            Stop::First(n) => out.len() < *n,
            Stop::Best(order, n) => {
                if out.len() > *n {
                    let mut best = SelectAst::new(schema);
                    best.order = order.clone();
                    best.limit = Some(*n as u64);
                    out = keep(std::mem::take(&mut out), &best)?;
                }
                true
            }
        })
    })?;
    Ok(Some((out, seq)))
}

fn load_all(ctx: &mut Ctx<'_>, schema: &'static TableSchema, into: &mut Tables) -> Result<()> {
    if into.contains(schema.name) {
        return Ok(());
    }
    if let Some((rows, seq)) = matching_rows(ctx, schema, &[], &Tables::new(), &[], Stop::Never, None)? {
        into.insert(schema, rows, seq);
    }
    Ok(())
}

fn is_count_all(ast: &SelectAst) -> bool {
    ast.joins.is_empty()
        && ast.filter.is_none()
        && ast.group_by.is_empty()
        && ast.having.is_none()
        && ast.limit.is_none()
        && ast.offset.unwrap_or(0) == 0
        && !ast.distinct
        && matches!(&ast.projection, Projection::Exprs(e)
            if matches!(e.as_slice(), [Expr::Aggregate { func: AggFunc::Count, arg: None, distinct: false }]))
}

/// `COUNT(*)` of one table whose filter is equalities that pin exactly the
/// columns of one of its indexes (`WHERE a = 1 AND b = 'x'` with an index on
/// `(a, b)`): the number of index entries is the answer, no row is read.
/// `None` when the statement is not of that form.
fn count_by_index(ctx: &mut Ctx<'_>, ast: &SelectAst) -> Result<Option<i64>> {
    let Some(filter) = &ast.filter else { return Ok(None) };
    let counts_all = ast.joins.is_empty()
        && ast.group_by.is_empty()
        && ast.having.is_none()
        && ast.limit.is_none()
        && ast.offset.unwrap_or(0) == 0
        && !ast.distinct
        && matches!(&ast.projection, Projection::Exprs(e)
            if matches!(e.as_slice(), [Expr::Aggregate { func: AggFunc::Count, arg: None, distinct: false }]));
    if !counts_all {
        return Ok(None);
    }
    let schema = ast.from;
    let mut pins: HashMap<&str, &Value> = HashMap::new();
    for part in conjuncts(filter) {
        let Expr::Binary { op: BinOp::Eq, left, right } = part else { return Ok(None) };
        let (column, value) = match (&**left, &**right) {
            (Expr::Column(c), Expr::Value(v)) | (Expr::Value(v), Expr::Column(c)) if c.table == schema.name => (c, v),
            _ => return Ok(None),
        };
        let Some(col) = schema.columns.get(column.index) else { return Ok(None) };
        if pins.insert(col.name, value).is_some_and(|old| old != value) {
            return Ok(None);
        }
    }
    let Some(meta) = ctx.meta(schema.name)? else { return Err(Error::NoSuchTable(schema.name.to_owned())) };
    let Some(i) = meta.indexes.iter().position(|ix| {
        ix.live() && ix.columns.len() == pins.len() && ix.columns.iter().all(|c| pins.contains_key(c.as_str()))
    }) else {
        return Ok(None);
    };
    let values: Vec<&Value> = meta.indexes[i].columns.iter().map(|c| pins[c.as_str()]).collect();
    // NULL is never indexed (and never equal).
    if values.iter().any(|v| v.is_null()) {
        return Ok(Some(0));
    }
    Ok(Some(ctx.index_count(&meta, i, &values)?))
}

/// The answer of a `SELECT` of one table from the rows that match its whole
/// filter, when nothing is left for the evaluator to do: plain columns (or
/// all of them), no order to apply. `Err` gives the rows back otherwise.
fn plain_answer(ast: &SelectAst, rows: Vec<Vec<Value>>) -> std::result::Result<Vec<Vec<Value>>, Vec<Vec<Value>>> {
    if !ast.order.is_empty() && rows.len() > 1 {
        return Err(rows);
    }
    let columns: Option<Vec<usize>> = match &ast.projection {
        Projection::Tables(tables) if tables.len() == 1 && tables[0].name == ast.from.name => None,
        Projection::Exprs(list) => {
            let picked: Option<Vec<usize>> = list
                .iter()
                .map(|e| match e {
                    Expr::Column(c) if c.table == ast.from.name && c.index < ast.from.columns.len() => Some(c.index),
                    _ => None,
                })
                .collect();
            match picked {
                Some(p) => Some(p),
                None => return Err(rows),
            }
        }
        _ => return Err(rows),
    };
    if rows.iter().any(|r| r.len() != ast.from.columns.len()) {
        return Err(rows);
    }
    let skip = ast.offset.unwrap_or(0) as usize;
    let take = ast.limit.map_or(usize::MAX, |l| l as usize);
    let rows = rows.into_iter().skip(skip).take(take);
    Ok(match columns {
        None => rows.collect(),
        Some(columns) => {
            // A column asked once is moved out of the row; asked twice, copied.
            let once = columns.iter().enumerate().all(|(i, c)| !columns[..i].contains(c));
            rows.map(|mut row| {
                columns.iter().map(|&c| if once { std::mem::take(&mut row[c]) } else { row[c].clone() }).collect()
            })
            .collect()
        }
    })
}

/// The answer to a statement that returns rows.
pub(crate) enum Fetched {
    /// Rows already final.
    Rows(Vec<Vec<Value>>),
    /// Tables to evaluate the statement on.
    Tables(Tables),
}

/// Reads what `query` needs.
pub(crate) fn fetch(ctx: &mut Ctx<'_>, query: &Query<'_>) -> Result<Fetched> {
    match *query {
        Query::FindByKey { table, key } => {
            let Some(meta) = ctx.meta(table.name)? else { return Err(Error::NoSuchTable(table.name.to_owned())) };
            let map = meta.map(table);
            Ok(Fetched::Rows(ctx.rows_by_pk(&meta, &map, None, &[pk_enc(key)])?))
        }
        Query::Select(ast) => {
            if is_count_all(ast) {
                let Some(meta) = ctx.meta(ast.from.name)? else {
                    return Err(Error::NoSuchTable(ast.from.name.to_owned()));
                };
                return Ok(Fetched::Rows(vec![vec![Value::I64(ctx.row_count(&meta)?)]]));
            }
            if let Some(n) = count_by_index(ctx, ast)? {
                return Ok(Fetched::Rows(vec![vec![Value::I64(n)]]));
            }
            let mut side_schemas = Vec::new();
            subquery_tables(ast, &mut side_schemas);
            let mut side = Tables::new();
            for s in &side_schemas {
                load_all(ctx, s, &mut side)?;
            }
            let parts: Vec<&Expr> = ast.filter.as_ref().map(conjuncts).unwrap_or_default();
            let single = ast.joins.is_empty()
                && ast.group_by.is_empty()
                && ast.having.is_none()
                && !ast.distinct
                && !matches!(&ast.projection, Projection::Exprs(e) if e.iter().any(|x| matches!(x, Expr::Aggregate { .. })));
            let mut tables = side.clone();
            for (i, schema) in ast.tables().enumerate() {
                if tables.contains(schema.name) && !side_schemas.iter().any(|s| s.name == schema.name) {
                    continue;
                }
                // The nullable side of a LEFT JOIN is read whole: filtering it
                // first would change which rows get NULLs.
                let nullable = i > 0 && ast.joins[i - 1].kind == JoinKind::Left;
                let own: Vec<&Expr> = if nullable {
                    Vec::new()
                } else {
                    parts
                        .iter()
                        .copied()
                        .filter(|p| {
                            let mut t = BTreeSet::new();
                            tables_read(p, &mut t);
                            t.len() == 1 && t.contains(schema.name)
                        })
                        .collect()
                };
                // Stopping early is only right when this table's filter is the whole filter.
                let whole = own.len() == parts.len();
                let stop = match (single && whole, ast.limit) {
                    (true, Some(limit)) => {
                        let n = (limit + ast.offset.unwrap_or(0)) as usize;
                        if ast.order.is_empty() { Stop::First(n) } else { Stop::Best(ast.order.clone(), n) }
                    }
                    _ => Stop::Never,
                };
                let needed = columns_read(ast, schema);
                if let Some((rows, seq)) =
                    matching_rows(ctx, schema, &own, &side, &side_schemas, stop, needed.as_deref())?
                {
                    // One table, its whole filter applied: plain columns need no evaluator.
                    let rows = if single && whole {
                        match plain_answer(ast, rows) {
                            Ok(answer) => return Ok(Fetched::Rows(answer)),
                            Err(rows) => rows,
                        }
                    } else {
                        rows
                    };
                    tables.insert(schema, rows, seq);
                }
            }
            Ok(Fetched::Tables(tables))
        }
        Query::Raw { .. } | Query::Script { .. } => {
            Err(Error::Unsupported("raw SQL is not supported by Nuage DB (it runs Memoire statements, not SQL)".into()))
        }
        _ => Err(Error::Unsupported("statement does not return rows".into())),
    }
}

/// Rows of a statement returning rows.
pub(crate) fn fetch_rows(ctx: &mut Ctx<'_>, query: &Query<'_>) -> Result<Vec<Vec<Value>>> {
    match fetch(ctx, query)? {
        Fetched::Rows(rows) => Ok(rows),
        Fetched::Tables(t) => evaluator::fetch(&t, *query),
    }
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

fn index_entries(meta: &TableMeta, stored: &[Value], pk: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for (i, ix) in meta.indexes.iter().enumerate() {
        if !ix.live() {
            continue;
        }
        let Some(pos) = meta.index_positions(ix) else { continue };
        let values: Vec<&Value> = pos.iter().map(|&p| stored.get(p).unwrap_or(&Value::Null)).collect();
        if values.iter().any(|v| v.is_null()) {
            continue;
        }
        let mut k = keys::index_prefix(meta.generation, i as u16, &values);
        k.extend_from_slice(pk);
        out.push(k);
    }
    out
}

/// Runs a write statement.
pub(crate) fn execute(ctx: &mut Ctx<'_>, query: &Query<'_>) -> Result<ExecResult> {
    match *query {
        Query::CreateTable { table, if_not_exists } => {
            if ctx.meta(table.name)?.is_some() {
                return if if_not_exists {
                    Ok(ExecResult::default())
                } else {
                    Err(Error::TableExists(table.name.to_owned()))
                };
            }
            let meta = TableMeta::new(table, ctx.store.next_generation());
            ctx.put_meta(table.name, &meta);
            ctx.written.insert(table.name.to_owned());
            Ok(ExecResult::default())
        }
        Query::DropTable { name, if_exists } => {
            let Some(meta) = ctx.meta(name)? else {
                return if if_exists { Ok(ExecResult::default()) } else { Err(Error::NoSuchTable(name.to_owned())) };
            };
            ctx.put(keys::catalog_key(name), None);
            // The rows go too, unless a branch made before still reads them.
            if ctx.store.children_of(ctx.branch).is_empty() {
                let (start, end) = keys::generation_range(meta.generation);
                ctx.overlay.retain(|k, _| !(k.as_slice() >= start.as_slice() && k.as_slice() < end.as_slice()));
                ctx.extra.push(Op::DelRange { table: super::store::branch_table(ctx.branch), start, end });
            }
            ctx.written.insert(name.to_owned());
            Ok(ExecResult::default())
        }
        Query::Raw { .. } | Query::Script { .. } => {
            Err(Error::Unsupported("raw SQL is not supported by Nuage DB (it runs Memoire statements, not SQL)".into()))
        }
        Query::Select(_) | Query::FindByKey { .. } => {
            Err(Error::Unsupported("use `fetch_all` for statements returning rows".into()))
        }
        Query::Insert(i) => write_rows(ctx, query, i.table),
        Query::Update(u) => write_rows(ctx, query, u.table),
        Query::Delete(d) => write_rows(ctx, query, d.table),
        Query::InsertRow { table, .. }
        | Query::UpsertRow { table, .. }
        | Query::UpdateRow { table, .. }
        | Query::DeleteByKey { table, .. } => write_rows(ctx, query, table),
        _ => Err(Error::Unsupported("statement not supported by Nuage DB".into())),
    }
}

fn write_rows(ctx: &mut Ctx<'_>, query: &Query<'_>, schema: &'static TableSchema) -> Result<ExecResult> {
    let Some(mut meta) = ctx.meta(schema.name)? else { return Err(Error::NoSuchTable(schema.name.to_owned())) };
    // A model with new columns widens the stored rows.
    let missing: Vec<String> =
        schema.columns.iter().filter(|c| meta.position(c.name).is_none()).map(|c| c.name.to_owned()).collect();
    if !missing.is_empty() {
        let mut wider = (*meta).clone();
        wider.columns.extend(missing);
        // The new columns come from the model; the columns already there keep
        // the type the table declared (a model may name a DOUBLE as text).
        let mut widened = schema_to_json(schema);
        if let (Some(new), Some(old)) = (
            widened.get_mut("columns").and_then(Json::as_array_mut),
            wider.schema.get("columns").and_then(Json::as_array),
        ) {
            for c in new.iter_mut() {
                let name = c.get("name").and_then(Json::as_str).map(str::to_owned);
                if let Some(before) = old.iter().find(|o| o.get("name").and_then(Json::as_str) == name.as_deref())
                    && let (Some(slot), Some(kind)) = (c.get_mut("kind"), before.get("kind"))
                {
                    *slot = kind.clone();
                }
            }
        }
        wider.schema = widened;
        wider.changed();
        ctx.put_meta(schema.name, &wider);
        meta = Arc::new(wider);
    }
    let map = meta.map(schema);
    let pk = schema.primary_key;
    let seq = ctx.counter(&keys::seq_key(meta.generation))?;

    // The rows the statement may change, and the tables its filter reads.
    let mut side_schemas = Vec::new();
    let (candidates, side) = match *query {
        Query::InsertRow { .. } | Query::Insert(_) => (Vec::new(), Tables::new()),
        Query::UpsertRow { values, .. } | Query::UpdateRow { values, .. } => {
            let key = values.get(pk).ok_or_else(|| Error::Conversion(format!("`{}`: row too short", schema.name)))?;
            (ctx.rows_by_pk(&meta, &map, None, &[pk_enc(key)])?, Tables::new())
        }
        Query::DeleteByKey { key, .. } => (ctx.rows_by_pk(&meta, &map, None, &[pk_enc(key)])?, Tables::new()),
        Query::Update(u) => {
            filter_tables(u.filter.as_ref(), &mut side_schemas);
            let mut side = Tables::new();
            for s in &side_schemas {
                load_all(ctx, s, &mut side)?;
            }
            let parts: Vec<&Expr> = u.filter.as_ref().map(conjuncts).unwrap_or_default();
            let rows = matching_rows(ctx, schema, &parts, &side, &side_schemas, Stop::Never, None)?
                .map_or(Vec::new(), |r| r.0);
            (rows, side)
        }
        Query::Delete(d) => {
            filter_tables(d.filter.as_ref(), &mut side_schemas);
            let mut side = Tables::new();
            for s in &side_schemas {
                load_all(ctx, s, &mut side)?;
            }
            let parts: Vec<&Expr> = d.filter.as_ref().map(conjuncts).unwrap_or_default();
            let rows = matching_rows(ctx, schema, &parts, &side, &side_schemas, Stop::Never, None)?
                .map_or(Vec::new(), |r| r.0);
            (rows, side)
        }
        _ => unreachable!("write_rows is only called for row writes"),
    };

    // Memoire evaluates the statement on those rows.
    let mut tables = side;
    tables.insert(schema, candidates.clone(), seq);
    let result = evaluator::execute(&mut tables, *query)?;
    let after = tables.rows(schema.name).unwrap_or_default();

    let before: BTreeMap<Vec<u8>, &Vec<Value>> = candidates.iter().map(|r| (pk_enc(&r[pk]), r)).collect();
    let after_map: BTreeMap<Vec<u8>, &Vec<Value>> = after.iter().map(|r| (pk_enc(&r[pk]), r)).collect();
    let deleted: Vec<&Vec<u8>> = before.keys().filter(|k| !after_map.contains_key(*k)).collect();
    let changed: Vec<(&Vec<u8>, &Vec<Value>)> =
        after_map.iter().filter(|(k, r)| before.get(*k).is_none_or(|b| *b != **r)).map(|(k, r)| (k, *r)).collect();
    if deleted.is_empty() && changed.is_empty() {
        return Ok(result);
    }

    // Unique keys: the rows this statement did not read may hold the values.
    for (key, row) in &changed {
        if !before.contains_key(*key) && ctx.get(&row_key_raw(meta.generation, key))?.is_some() {
            return Err(Error::UniqueViolation {
                table: schema.name.to_owned(),
                column: meta.columns[meta.pk].clone(),
            });
        }
        let stored = to_stored(&meta, &map, row, None);
        for (i, ix) in meta.indexes.iter().enumerate().filter(|(_, ix)| ix.unique && ix.live()) {
            let Some(pos) = meta.index_positions(ix) else { continue };
            let values: Vec<&Value> = pos.iter().map(|&p| &stored[p]).collect();
            if values.iter().any(|v| v.is_null()) {
                continue;
            }
            for other in ctx.index_lookup(&meta, i, &values)? {
                if !before.contains_key(&other) && other != **key {
                    return Err(Error::UniqueViolation {
                        table: schema.name.to_owned(),
                        column: ix.columns.join(", "),
                    });
                }
            }
        }
    }

    // Old index entries out, new rows and entries in.
    let mut old_stored: HashMap<Vec<u8>, Vec<Value>> = HashMap::new();
    for key in before.keys() {
        if let Some(b) = ctx.get(&row_key_raw(meta.generation, key))? {
            old_stored.insert(key.clone(), keys::decode_row(&b)?);
        }
    }
    let mut count = ctx.row_count(&meta)?;
    let mut bytes = ctx.table_bytes(&meta)?;
    for key in &deleted {
        if let Some(old) = old_stored.get(*key) {
            for e in index_entries(&meta, old, key) {
                ctx.put(e, None);
            }
            ctx.put(row_key_raw(meta.generation, key), None);
            count -= 1;
            bytes -= keys::encode_row(old).len() as i64;
        }
    }
    for (key, row) in &changed {
        let old = old_stored.get(*key);
        if let Some(old) = old {
            for e in index_entries(&meta, old, key) {
                ctx.put(e, None);
            }
            bytes -= keys::encode_row(old).len() as i64;
        } else {
            count += 1;
        }
        let stored = to_stored(&meta, &map, row, old);
        for e in index_entries(&meta, &stored, key) {
            ctx.put(e, Some(Vec::new()));
        }
        let encoded = keys::encode_row(&stored);
        bytes += encoded.len() as i64;
        ctx.put(row_key_raw(meta.generation, key), Some(encoded));
    }
    ctx.put(keys::count_key(meta.generation), Some(keys::encode_i64(count)));
    ctx.put(keys::bytes_key(meta.generation), Some(keys::encode_i64(bytes.max(0))));
    if let Some(last) = tables.last_id(schema.name).filter(|&l| l > seq) {
        ctx.put(keys::seq_key(meta.generation), Some(keys::encode_i64(last)));
    }
    ctx.written.insert(schema.name.to_owned());
    Ok(result)
}

/// A row in storage order; stored columns the model does not know keep
/// their old value.
///
/// A text written to a column of numbers or dates is stored as the column's
/// type, as PostgreSQL does (`'48.86'` into a `DOUBLE PRECISION`): a model
/// that writes decimals as exact text (a `NUMERIC` in PostgreSQL) would
/// otherwise leave text among the numbers, and every comparison on the
/// column (`latitude BETWEEN ...`) would fail. A text that does not convert
/// is kept as it is.
fn to_stored(meta: &TableMeta, map: &[Option<usize>], row: &[Value], old: Option<&Vec<Value>>) -> Vec<Value> {
    let mut stored = old.cloned().unwrap_or_default();
    stored.resize(meta.columns.len(), Value::Null);
    let kinds = meta.kinds();
    for (i, m) in map.iter().enumerate() {
        if let Some(p) = m {
            stored[*p] = match (&row[i], kinds.get(*p).copied().flatten()) {
                (Value::Text(_), Some(kind)) if converts_text(kind) => {
                    crate::sql::value::cast(&row[i], kind).unwrap_or_else(|_| row[i].clone())
                }
                _ => row[i].clone(),
            };
        }
    }
    stored
}

/// Column types a text is converted to when it is written.
fn converts_text(kind: ColumnType) -> bool {
    matches!(
        kind,
        ColumnType::SmallInt
            | ColumnType::Int
            | ColumnType::BigInt
            | ColumnType::Real
            | ColumnType::Double
            | ColumnType::Bool
            | ColumnType::Timestamp
            | ColumnType::TimestampTz
            | ColumnType::Date
            | ColumnType::Time
    )
}

// ---------------------------------------------------------------------------
// Schema changes of SQL: indexes and columns on existing tables
// ---------------------------------------------------------------------------

/// A change to an existing table.
#[derive(Clone, Debug)]
pub(crate) enum Alter {
    /// `CREATE [UNIQUE] INDEX name ON table (columns)`: built from the rows
    /// already there.
    AddIndex { table: String, name: String, columns: Vec<String>, unique: bool, if_not_exists: bool },
    /// `DROP INDEX name`.
    DropIndex { name: String, if_exists: bool },
    /// `ALTER TABLE table ADD COLUMN`: `column` as the wire protocol writes a
    /// column, `fill` the value of the rows already there.
    AddColumn { table: String, column: Json, fill: Value },
}

fn index_name_taken(ctx: &mut Ctx<'_>, name: &str) -> Result<Option<(String, usize)>> {
    for table in ctx.table_names()? {
        if let Some(meta) = ctx.meta(&table)?
            && let Some(i) = meta.indexes.iter().position(|ix| ix.live() && ix.name.as_deref() == Some(name))
        {
            return Ok(Some((table, i)));
        }
    }
    Ok(None)
}

/// Every stored row of a table, with its encoded primary key.
fn all_stored(ctx: &Ctx<'_>, meta: &TableMeta) -> Result<Vec<(Vec<u8>, Vec<Value>)>> {
    let (start, end) = keys::rows_range(meta.generation);
    let prefix = keys::data_prefix(meta.generation, keys::ROW).len();
    let mut out = Vec::new();
    let mut at = start;
    loop {
        let (entries, next) = ctx.snap.scan(Some(ctx.overlay), &at, &end, CHUNK)?;
        for (k, v) in entries {
            out.push((k[prefix..].to_vec(), keys::decode_row(&v)?));
        }
        match next {
            Some(n) => at = n,
            None => return Ok(out),
        }
    }
}

/// Applies a schema change.
pub(crate) fn alter(ctx: &mut Ctx<'_>, change: &Alter) -> Result<()> {
    match change {
        Alter::AddIndex { table, name, columns, unique, if_not_exists } => {
            if index_name_taken(ctx, name)?.is_some() {
                return if *if_not_exists {
                    Ok(())
                } else {
                    Err(Error::Validation(format!("there is already an index named `{name}`")))
                };
            }
            let Some(mut meta) = ctx.meta_mut(table)? else { return Err(Error::NoSuchTable(table.clone())) };
            for c in columns {
                if meta.position(c).is_none() {
                    return Err(Error::Validation(format!("`{table}` has no column `{c}`")));
                }
            }
            let index = IndexMeta { columns: columns.clone(), unique: *unique, name: Some(name.clone()) };
            let n = meta.indexes.len();
            if n >= u16::MAX as usize {
                return Err(Error::Validation(format!("`{table}` has too many indexes")));
            }
            let pos = meta.index_positions(&index).unwrap_or_default();
            let mut seen = std::collections::HashSet::new();
            for (pk, stored) in all_stored(ctx, &meta)? {
                let values: Vec<&Value> = pos.iter().map(|&p| stored.get(p).unwrap_or(&Value::Null)).collect();
                if values.iter().any(|v| v.is_null()) {
                    continue;
                }
                let mut k = keys::index_prefix(meta.generation, n as u16, &values);
                if *unique && !seen.insert(k.clone()) {
                    return Err(Error::UniqueViolation { table: table.clone(), column: columns.join(", ") });
                }
                k.extend_from_slice(&pk);
                ctx.put(k, Some(Vec::new()));
            }
            meta.indexes.push(index);
            ctx.put_meta(table, &meta);
            ctx.written.insert(table.clone());
            Ok(())
        }
        Alter::DropIndex { name, if_exists } => {
            let Some((table, i)) = index_name_taken(ctx, name)? else {
                return if *if_exists {
                    Ok(())
                } else {
                    Err(Error::Validation(format!("there is no index named `{name}`")))
                };
            };
            let Some(mut meta) = ctx.meta_mut(&table)? else { return Err(Error::NoSuchTable(table)) };
            let start = keys::index_prefix(meta.generation, i as u16, &[]);
            let end = keys::prefix_end(&start);
            let mut at = start.clone();
            loop {
                let (entries, next) = ctx.snap.scan(Some(ctx.overlay), &at, &end, CHUNK)?;
                for (k, _) in entries {
                    ctx.put(k, None);
                }
                match next {
                    Some(n) => at = n,
                    None => break,
                }
            }
            // Its number stays taken: later indexes keep theirs.
            meta.indexes[i] = IndexMeta { columns: Vec::new(), unique: false, name: None };
            ctx.put_meta(&table, &meta);
            ctx.written.insert(table);
            Ok(())
        }
        Alter::AddColumn { table, column, fill } => {
            let Some(mut meta) = ctx.meta_mut(table)? else { return Err(Error::NoSuchTable(table.clone())) };
            let name = column.get("name").and_then(Json::as_str).unwrap_or_default().to_owned();
            if meta.position(&name).is_some() {
                return Err(Error::Validation(format!("`{table}` already has a column `{name}`")));
            }
            meta.columns.push(name);
            if let Some(cols) = meta.schema.get_mut("columns").and_then(Json::as_array_mut) {
                cols.push(column.clone());
            }
            meta.changed();
            if !fill.is_null() {
                let width = meta.columns.len();
                let mut bytes = ctx.table_bytes(&meta)?;
                for (pk, mut stored) in all_stored(ctx, &meta)? {
                    let before = keys::encode_row(&stored).len() as i64;
                    stored.resize(width, Value::Null);
                    stored[width - 1] = fill.clone();
                    let encoded = keys::encode_row(&stored);
                    bytes += encoded.len() as i64 - before;
                    ctx.put(row_key_raw(meta.generation, &pk), Some(encoded));
                }
                ctx.put(keys::bytes_key(meta.generation), Some(keys::encode_i64(bytes.max(0))));
            }
            ctx.put_meta(table, &meta);
            ctx.written.insert(table.clone());
            Ok(())
        }
    }
}
