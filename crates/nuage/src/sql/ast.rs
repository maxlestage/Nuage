//! The syntax of SQL statements, as the parser reads them.

use memoire_orm::{ColumnType, Value};

/// Binary operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Expr {
    Lit(Value),
    /// The `n`th parameter (from 0).
    Param(usize),
    Col {
        table: Option<String>,
        name: String,
    },
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(Op, Box<Expr>, Box<Expr>),
    IsNull(Box<Expr>, bool),
    In(Box<Expr>, Vec<Expr>, bool),
    InSelect(Box<Expr>, Box<Select>, bool),
    Exists(Box<Select>, bool),
    /// A subquery giving one value.
    Scalar(Box<Select>),
    Between(Box<Expr>, Box<Expr>, Box<Expr>, bool),
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        ci: bool,
        negated: bool,
        escape: Option<Box<Expr>>,
    },
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    Cast(Box<Expr>, ColumnType),
    /// A function: `name` in lower case; `star` for `count(*)`.
    Func {
        name: String,
        args: Vec<Expr>,
        distinct: bool,
        star: bool,
    },
}

/// Aggregate functions (with one argument, or `count(*)`).
pub(crate) const AGGREGATES: [&str; 12] = [
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "total",
    "group_concat",
    "string_agg",
    "array_agg",
    "bool_and",
    "bool_or",
    "every",
];

impl Expr {
    pub(crate) fn is_aggregate(&self) -> bool {
        matches!(self, Expr::Func { name, args, star, .. }
            if AGGREGATES.contains(&name.as_str()) && (*star || args.len() == 1 || (args.len() == 2 && matches!(name.as_str(), "group_concat" | "string_agg"))))
    }

    /// It, or an expression in it (outside subqueries), is an aggregate.
    pub(crate) fn has_aggregate(&self) -> bool {
        let mut found = false;
        self.walk(&mut |e| found |= e.is_aggregate());
        found
    }

    /// Calls `f` on it and every expression in it, subqueries excepted.
    pub(crate) fn walk(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        match self {
            Expr::Lit(_) | Expr::Param(_) | Expr::Col { .. } | Expr::Exists(..) | Expr::Scalar(_) => {}
            Expr::Neg(e) | Expr::Not(e) | Expr::IsNull(e, _) | Expr::Cast(e, _) | Expr::InSelect(e, _, _) => e.walk(f),
            Expr::Bin(_, a, b) => {
                a.walk(f);
                b.walk(f);
            }
            Expr::In(e, list, _) => {
                e.walk(f);
                list.iter().for_each(|x| x.walk(f));
            }
            Expr::Between(a, b, c, _) => {
                a.walk(f);
                b.walk(f);
                c.walk(f);
            }
            Expr::Like { expr, pattern, escape, .. } => {
                expr.walk(f);
                pattern.walk(f);
                if let Some(e) = escape {
                    e.walk(f);
                }
            }
            Expr::Case { operand, whens, otherwise } => {
                if let Some(o) = operand {
                    o.walk(f);
                }
                for (w, t) in whens {
                    w.walk(f);
                    t.walk(f);
                }
                if let Some(o) = otherwise {
                    o.walk(f);
                }
            }
            Expr::Func { args, .. } => args.iter().for_each(|x| x.walk(f)),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SelItem {
    /// `*`
    All,
    /// `t.*`
    TableAll(String),
    Expr(Expr, Option<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JoinKind {
    Inner,
    Left,
    Cross,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Source {
    Table(String),
    Select(Box<Select>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TableRef {
    pub source: Source,
    pub alias: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Join {
    pub kind: JoinKind,
    pub table: TableRef,
    pub on: Option<Expr>,
}

/// One `SELECT` (without `ORDER BY` and `LIMIT`, which belong to the whole
/// compound).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Core {
    pub distinct: bool,
    pub items: Vec<SelItem>,
    pub from: Option<TableRef>,
    pub joins: Vec<Join>,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SetOp {
    Union,
    UnionAll,
    Intersect,
    Except,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OrderItem {
    pub expr: Expr,
    pub desc: bool,
    pub nulls_first: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Select {
    pub first: Core,
    pub rest: Vec<(SetOp, Core)>,
    pub order: Vec<OrderItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ColDef {
    pub name: String,
    /// The wire protocol's name of the type (`int`, `text`...).
    pub kind: &'static str,
    pub primary_key: bool,
    pub auto_increment: bool,
    pub not_null: bool,
    pub unique: bool,
    pub indexed: bool,
    /// The `DEFAULT` expression, as SQL.
    pub default: Option<String>,
    pub references: Option<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Conflict {
    Nothing,
    Update(Vec<(String, Expr)>, Option<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Select(Box<Select>),
    Defaults,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Stmt {
    Select(Select),
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        source: InsertSource,
        /// Conflict target (empty: any unique key) and what to do.
        on_conflict: Option<(Vec<String>, Conflict)>,
        /// `INSERT OR REPLACE`, `REPLACE INTO`.
        replace: bool,
        returning: Option<Vec<SelItem>>,
    },
    Update {
        table: String,
        alias: Option<String>,
        sets: Vec<(String, Expr)>,
        filter: Option<Expr>,
        returning: Option<Vec<SelItem>>,
    },
    Delete {
        table: String,
        alias: Option<String>,
        filter: Option<Expr>,
        returning: Option<Vec<SelItem>>,
    },
    CreateTable {
        name: String,
        if_not_exists: bool,
        columns: Vec<ColDef>,
        /// `UNIQUE (a, b)`, `INDEX (a)`: columns and uniqueness.
        indexes: Vec<(Vec<String>, bool)>,
        /// `CREATE TABLE t AS SELECT ...`
        as_select: Option<Box<Select>>,
    },
    CreateIndex {
        name: String,
        table: String,
        columns: Vec<String>,
        unique: bool,
        if_not_exists: bool,
    },
    DropIndex {
        name: String,
        if_exists: bool,
    },
    AddColumn {
        table: String,
        column: ColDef,
    },
    DropTable {
        names: Vec<String>,
        if_exists: bool,
    },
    /// `BEGIN`, `COMMIT`, `ROLLBACK`: a request is one transaction already.
    Transaction(&'static str),
    /// `EXPLAIN`: how a `SELECT` runs.
    Explain(Box<Stmt>),
}

impl Stmt {
    /// It changes something.
    pub(crate) fn writes(&self) -> bool {
        !matches!(self, Stmt::Select(_) | Stmt::Transaction(_) | Stmt::Explain(_))
    }
}
