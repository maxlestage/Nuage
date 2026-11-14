//! Types of a statement's parameters and result columns, from the columns of
//! its tables: what PostgreSQL drivers ask before they send values
//! (`id <= $1` makes `$1` the type of `id`).

use memoire_orm::ColumnType;

use super::ast::*;
use super::exec::Runner;

/// Types found for a statement: of each parameter, and of each result column.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Typing {
    pub params: Vec<Option<ColumnType>>,
    pub columns: Vec<Option<ColumnType>>,
    /// Names of the result columns (empty when some are unknown).
    pub names: Vec<String>,
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
            Expr::Bin(Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod, a, b) => {
                self.type_of(a).or_else(|| self.type_of(b))
            }
            Expr::Bin(Op::Concat, ..) | Expr::Like { .. } => Some(ColumnType::Text),
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
            Expr::Bin(op, a, b) => {
                if !matches!(op, Op::And | Op::Or) {
                    let (ta, tb) = (self.type_of(a), self.type_of(b));
                    self.set(a, tb);
                    self.set(b, ta);
                }
                self.expr(a);
                self.expr(b);
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
    /// (the tables of that `SELECT`, or the table of `RETURNING`).
    fn items(&self, items: &[SelItem], own: &[(Option<String>, String)]) -> (Vec<Option<ColumnType>>, Vec<String>) {
        let mut out = Vec::new();
        let mut names = Vec::new();
        for item in items {
            match item {
                SelItem::Expr(e, alias) => {
                    out.push(self.type_of(e));
                    names.push(match (alias, e) {
                        (Some(a), _) => a.clone(),
                        (None, Expr::Col { name, .. }) => name.clone(),
                        (None, Expr::Func { name, .. }) => name.clone(),
                        (None, Expr::Cast(inner, _)) => match &**inner {
                            Expr::Col { name, .. } => name.clone(),
                            _ => "?column?".into(),
                        },
                        _ => "?column?".into(),
                    });
                }
                SelItem::All | SelItem::TableAll(_) => {
                    let wanted = match item {
                        SelItem::TableAll(t) => Some(t.as_str()),
                        _ => None,
                    };
                    for (alias, t) in own {
                        if wanted.is_some_and(|w| !(alias.as_deref() == Some(w) || t.eq_ignore_ascii_case(w))) {
                            continue;
                        }
                        match self.runner.schema(t) {
                            Ok(schema) => {
                                out.extend(schema.columns.iter().map(|c| Some(c.kind)));
                                names.extend(schema.columns.iter().map(|c| c.name.to_owned()));
                            }
                            // A derived table: its columns are unknown here.
                            Err(_) => return (Vec::new(), Vec::new()),
                        }
                    }
                }
            }
        }
        (out, names)
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
            let mut own = Vec::new();
            for t in s.first.from.iter().chain(s.first.joins.iter().map(|j| &j.table)) {
                match &t.source {
                    Source::Table(name) => own.push((t.alias.clone(), name.clone())),
                    Source::Select(_) => own.push((t.alias.clone(), String::new())),
                }
            }
            inf.items(&s.first.items, &own)
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
            returning.as_deref().map(|r| inf.items(r, &[(None, table.clone())])).unwrap_or_default()
        }
        Stmt::Update { table, alias, sets, filter, returning } => {
            inf.assign(table, sets);
            if let Some(f) = filter {
                inf.expr(f);
            }
            returning.as_deref().map(|r| inf.items(r, &[(alias.clone(), table.clone())])).unwrap_or_default()
        }
        Stmt::Delete { table, alias, filter, returning } => {
            if let Some(f) = filter {
                inf.expr(f);
            }
            returning.as_deref().map(|r| inf.items(r, &[(alias.clone(), table.clone())])).unwrap_or_default()
        }
        _ => (Vec::new(), Vec::new()),
    };
    Typing { params: inf.params, columns, names }
}
