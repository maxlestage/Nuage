//! Types of a statement's parameters and result columns, from the columns of
//! its tables: what PostgreSQL drivers ask before they send values
//! (`id <= $1` makes `$1` the type of `id`).

use memoire_orm::ColumnType;

use super::ast::*;
use super::exec::{Runner, header, unique_headers};

/// Types found for a statement: of each parameter, and of each result column.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Typing {
    pub params: Vec<Option<ColumnType>>,
    pub columns: Vec<Option<ColumnType>>,
    /// Names of the result columns (empty when some are unknown).
    pub names: Vec<String>,
}

/// A table a `SELECT` reads, as `*` expands it: its alias, and its name or
/// the `SELECT` it is made of.
struct Own<'s> {
    alias: Option<String>,
    source: &'s Source,
}

/// The tables of a `SELECT` core that `*` expands to.
fn own_of(c: &Core) -> Vec<Own<'_>> {
    c.from
        .iter()
        .chain(c.joins.iter().map(|j| &j.table))
        .map(|t| Own { alias: t.alias.clone(), source: &t.source })
        .collect()
}

struct Infer<'r, 'a> {
    runner: &'r Runner<'a>,
    /// Tables of the statement, with their aliases.
    tables: Vec<(Option<String>, String)>,
    params: Vec<Option<ColumnType>>,
}

impl Infer<'_, '_> {
    fn column(&self, table: Option<&str>, name: &str) -> Option<ColumnType> {
        self.tables
            .iter()
            .filter(|(alias, t)| {
                table.is_none_or(|q| {
                    alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(q)) || t.eq_ignore_ascii_case(q)
                })
            })
            .find_map(|(_, t)| {
                let schema = self.runner.schema(t).ok()?;
                schema.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name)).map(|c| c.kind)
            })
    }

    fn type_of(&self, e: &Expr) -> Option<ColumnType> {
        match e {
            Expr::Col { table, name } => self.column(table.as_deref(), name),
            Expr::Cast(_, t) => Some(*t),
            Expr::Param(n) => self.params.get(*n).copied().flatten(),
            Expr::Lit(v) => match v {
                memoire_orm::Value::I32(_) | memoire_orm::Value::I64(_) => Some(ColumnType::BigInt),
                memoire_orm::Value::F64(_) => Some(ColumnType::Double),
                memoire_orm::Value::Text(_) => Some(ColumnType::Text),
                memoire_orm::Value::Bool(_) => Some(ColumnType::Bool),
                _ => None,
            },
            Expr::Chain(first, rest) => match rest.first()?.0 {
                Op::Concat => Some(ColumnType::Text),
                // A date moved by an interval: a date gives a timestamp, the others stay what they are.
                Op::Add | Op::Sub if rest.iter().any(|(_, x)| x.interval().is_some()) => None,
                Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod => {
                    self.type_of(first).or_else(|| rest.iter().find_map(|(_, x)| self.type_of(x)))
                }
                _ => None,
            },
            Expr::Like { .. } => Some(ColumnType::Text),
            Expr::Func { name, star, args, .. } => match name.as_str() {
                "count" => Some(ColumnType::BigInt),
                "sum" | "min" | "max" | "coalesce" | "abs" => {
                    args.first().and_then(|a| self.type_of(a)).or(if *star { Some(ColumnType::BigInt) } else { None })
                }
                "avg" | "total" => Some(ColumnType::Double),
                "bool_and" | "bool_or" | "every" => Some(ColumnType::Bool),
                "lower" | "upper" | "trim" | "substr" | "substring" | "replace" | "concat" | "group_concat"
                | "string_agg" => Some(ColumnType::Text),
                "length" => Some(ColumnType::BigInt),
                "now" => Some(ColumnType::TimestampTz),
                _ => None,
            },
            _ => None,
        }
    }

    fn set(&mut self, e: &Expr, t: Option<ColumnType>) {
        if let (Expr::Param(n), Some(t)) = (e, t)
            && let Some(slot) = self.params.get_mut(*n)
            && slot.is_none()
        {
            *slot = Some(t);
        }
    }

    fn expr(&mut self, e: &Expr) {
        match e {
            Expr::Bin(_, a, b) => {
                let (ta, tb) = (self.type_of(a), self.type_of(b));
                self.set(a, tb);
                self.set(b, ta);
                self.expr(a);
                self.expr(b);
            }
            Expr::Chain(first, rest) => {
                // Arithmetic and `||`: each operand has the type of what stands before it
                // (and the first, of the second), as pairs applied from the left have.
                if let Some((op, second)) = rest.first()
                    && !matches!(op, Op::And | Op::Or)
                {
                    let mut before = self.type_of(first);
                    let after = self.type_of(second);
                    self.set(first, after);
                    for (op, x) in rest {
                        self.set(x, before);
                        before =
                            if *op == Op::Concat { Some(ColumnType::Text) } else { before.or_else(|| self.type_of(x)) };
                    }
                }
                self.expr(first);
                rest.iter().for_each(|(_, x)| self.expr(x));
            }
            Expr::In(x, list, _) => {
                let t = self.type_of(x);
                for item in list {
                    self.set(item, t);
                    self.expr(item);
                }
                self.expr(x);
            }
            Expr::Between(x, lo, hi, _) => {
                let t = self.type_of(x);
                self.set(lo, t);
                self.set(hi, t);
                self.expr(x);
                self.expr(lo);
                self.expr(hi);
            }
            Expr::Like { expr, pattern, escape, .. } => {
                self.set(expr, Some(ColumnType::Text));
                self.set(pattern, Some(ColumnType::Text));
                if let Some(esc) = escape {
                    self.set(esc, Some(ColumnType::Text));
                }
            }
            Expr::Cast(x, t) => {
                self.set(x, Some(*t));
                self.expr(x);
            }
            Expr::Neg(x) | Expr::Not(x) | Expr::IsNull(x, _) => self.expr(x),
            Expr::InSelect(x, sel, _) => {
                self.expr(x);
                self.select(sel);
            }
            Expr::Exists(sel, _) | Expr::Scalar(sel) => self.select(sel),
            Expr::Case { operand, whens, otherwise } => {
                for x in operand.iter().chain(otherwise.iter()) {
                    self.expr(x);
                }
                for (w, t) in whens {
                    self.expr(w);
                    self.expr(t);
                }
            }
            Expr::Func { args, .. } => args.iter().for_each(|a| self.expr(a)),
            Expr::Lit(_) | Expr::Param(_) | Expr::Col { .. } => {}
        }
    }

    fn core(&mut self, c: &Core) {
        for item in &c.items {
            if let SelItem::Expr(e, _) = item {
                self.expr(e);
            }
        }
        if let Some(Source::Select(s)) = c.from.as_ref().map(|t| &t.source) {
            self.select(s);
        }
        for j in &c.joins {
            if let Source::Select(s) = &j.table.source {
                self.select(s);
            }
            if let Some(on) = &j.on {
                self.expr(on);
            }
        }
        for e in c.filter.iter().chain(&c.group_by).chain(c.having.iter()) {
            self.expr(e);
        }
    }

    fn select(&mut self, s: &Select) {
        self.core(&s.first);
        for (_, c) in &s.rest {
            self.core(c);
        }
        for o in &s.order {
            self.expr(&o.expr);
        }
        for e in s.limit.iter().chain(s.offset.iter()) {
            self.set(e, Some(ColumnType::BigInt));
        }
    }

    fn assign(&mut self, table: &str, sets: &[(String, Expr)]) {
        for (col, e) in sets {
            let t = self.column(Some(table), col);
            self.set(e, t);
            self.expr(e);
        }
    }

    /// The result columns of `items`, `*` expanding to the columns of `own`
    /// (the tables of that `SELECT`, or the table of `RETURNING`), named as a
    /// run names them: an expression without an alias after its place
    /// (`column2`), and in a `SELECT` (`qualify`), a name met twice after its
    /// table (`b.id`). Empty when some are unknown.
    fn items(&self, items: &[SelItem], own: &[Own<'_>], qualify: bool) -> (Vec<Option<ColumnType>>, Vec<String>) {
        let mut out = Vec::new();
        // Each name, and the table that qualifies it if it is met twice:
        // `Some(None)` for a table a run may spell its own way.
        let mut names: Vec<(String, Option<Option<String>>)> = Vec::new();
        for (n, item) in items.iter().enumerate() {
            match item {
                SelItem::Expr(e, alias) => {
                    out.push(self.type_of(e));
                    let table = match e {
                        Expr::Col { table: Some(t), .. } => Some(Some(t.clone())),
                        _ => None,
                    };
                    names.push((alias.clone().unwrap_or_else(|| header(e, n)), table));
                }
                SelItem::All | SelItem::TableAll(_) => {
                    let wanted = match item {
                        SelItem::TableAll(t) => Some(t.as_str()),
                        _ => None,
                    };
                    for o in own {
                        let name = match o.source {
                            Source::Table(name) => Some(name.as_str()),
                            Source::Select(_) => None,
                        };
                        if wanted.is_some_and(|w| {
                            !(o.alias.as_deref() == Some(w) || name.is_some_and(|t| t.eq_ignore_ascii_case(w)))
                        }) {
                            continue;
                        }
                        let (kinds, columns, spelled) = match o.source {
                            Source::Table(t) => match self.runner.schema(t) {
                                Ok(schema) => (
                                    schema.columns.iter().map(|c| Some(c.kind)).collect(),
                                    schema.columns.iter().map(|c| c.name.to_owned()).collect(),
                                    schema.name == t.as_str(),
                                ),
                                Err(_) => return (Vec::new(), Vec::new()),
                            },
                            // A derived table: the columns of its `SELECT`.
                            Source::Select(sub) => {
                                let (kinds, columns) = self.select_items(sub);
                                if columns.is_empty() {
                                    return (Vec::new(), Vec::new());
                                }
                                (kinds, columns, false)
                            }
                        };
                        let table = match (&o.alias, name) {
                            (Some(alias), _) => Some(alias.clone()),
                            (None, Some(t)) if spelled => Some(t.to_owned()),
                            _ => None,
                        };
                        out.extend(kinds);
                        names.extend(columns.into_iter().map(|c| (c, Some(table.clone()))));
                    }
                }
            }
        }
        if !qualify {
            return (out, names.into_iter().map(|(n, _)| n).collect());
        }
        let unique = unique_headers(names.iter().map(|(n, t)| (n.clone(), t.clone().flatten())).collect());
        // A name met twice whose table a run may spell otherwise: unknown.
        for (i, (name, table)) in names.iter().enumerate() {
            if *table == Some(None) && unique[..i].iter().any(|u| u.eq_ignore_ascii_case(name)) {
                return (Vec::new(), Vec::new());
            }
        }
        (out, unique)
    }

    /// The result columns of `RETURNING items` of `table`: a run names them
    /// without their table.
    fn returning(
        &self,
        items: &[SelItem],
        alias: Option<String>,
        table: &str,
    ) -> (Vec<Option<ColumnType>>, Vec<String>) {
        let source = Source::Table(table.to_owned());
        self.items(items, &[Own { alias, source: &source }], false)
    }

    /// The result columns of a `SELECT`: those of its first part.
    fn select_items(&self, s: &Select) -> (Vec<Option<ColumnType>>, Vec<String>) {
        self.items(&s.first.items, &own_of(&s.first), true)
    }
}

fn tables_of_select(s: &Select, out: &mut Vec<(Option<String>, String)>) {
    for c in std::iter::once(&s.first).chain(s.rest.iter().map(|(_, c)| c)) {
        for t in c.from.iter().chain(c.joins.iter().map(|j| &j.table)) {
            match &t.source {
                Source::Table(name) => out.push((t.alias.clone(), name.clone())),
                Source::Select(sub) => tables_of_select(sub, out),
            }
        }
        let mut subs = Vec::new();
        for e in c.filter.iter().chain(c.having.iter()) {
            collect_subselects(e, &mut subs);
        }
        for item in &c.items {
            if let SelItem::Expr(e, _) = item {
                collect_subselects(e, &mut subs);
            }
        }
        for sub in subs {
            tables_of_select(&sub, out);
        }
    }
}

fn collect_subselects(e: &Expr, out: &mut Vec<Select>) {
    e.walk(&mut |x| match x {
        Expr::InSelect(_, s, _) | Expr::Exists(s, _) | Expr::Scalar(s) => out.push((**s).clone()),
        _ => {}
    });
}

/// The types of `stmt`'s `params` parameters and of its result columns.
pub(crate) fn infer(runner: &Runner<'_>, stmt: &Stmt, params: usize) -> Typing {
    let mut tables = Vec::new();
    match stmt {
        Stmt::Select(s) => tables_of_select(s, &mut tables),
        Stmt::Explain(inner) => return infer(runner, inner, params),
        Stmt::Insert { table, source, .. } => {
            tables.push((None, table.clone()));
            if let InsertSource::Select(s) = source {
                tables_of_select(s, &mut tables);
            }
        }
        Stmt::Update { table, alias, filter, .. } | Stmt::Delete { table, alias, filter, .. } => {
            tables.push((alias.clone(), table.clone()));
            let mut subs = Vec::new();
            if let Some(e) = filter {
                collect_subselects(e, &mut subs);
            }
            for s in subs {
                tables_of_select(&s, &mut tables);
            }
        }
        _ => {}
    }
    let mut inf = Infer { runner, tables, params: vec![None; params] };
    let (columns, names) = match stmt {
        Stmt::Select(s) => {
            inf.select(s);
            inf.select_items(s)
        }
        Stmt::Insert { table, columns, source, on_conflict, returning, .. } => {
            let names: Vec<String> = match columns {
                Some(c) => c.clone(),
                None => runner
                    .schema(table)
                    .map(|s| s.columns.iter().map(|c| c.name.to_owned()).collect())
                    .unwrap_or_default(),
            };
            match source {
                InsertSource::Values(rows) => {
                    for row in rows {
                        for (i, e) in row.iter().enumerate() {
                            let t = names.get(i).and_then(|n| inf.column(Some(table), n));
                            inf.set(e, t);
                            inf.expr(e);
                        }
                    }
                }
                InsertSource::Select(s) => inf.select(s),
                InsertSource::Defaults => {}
            }
            if let Some((_, Conflict::Update(sets, filter))) = on_conflict {
                inf.assign(table, sets);
                if let Some(f) = filter {
                    inf.expr(f);
                }
            }
            returning.as_deref().map(|r| inf.returning(r, None, table)).unwrap_or_default()
        }
        Stmt::Update { table, alias, sets, filter, returning } => {
            inf.assign(table, sets);
            if let Some(f) = filter {
                inf.expr(f);
            }
            returning.as_deref().map(|r| inf.returning(r, alias.clone(), table)).unwrap_or_default()
        }
        Stmt::Delete { table, alias, filter, returning } => {
            if let Some(f) = filter {
                inf.expr(f);
            }
            returning.as_deref().map(|r| inf.returning(r, alias.clone(), table)).unwrap_or_default()
        }
        _ => (Vec::new(), Vec::new()),
    };
    Typing { params: inf.params, columns, names }
}
