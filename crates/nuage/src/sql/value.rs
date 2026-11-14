//! Values in SQL: comparison, arithmetic, conversion and the scalar
//! functions. Comparison, truth and arithmetic follow Memoire's in-memory
//! engine exactly (the statements it runs give the same answers), plus the
//! conversions SQL written by hand needs (a date written as text, numbers in
//! text).

use std::cmp::Ordering;

use memoire_orm::{ColumnType, Error, Result, Value};
use serde_json::{Value as Json, json};

use super::ast::Op;
use super::lex::syntax;

fn conversion(msg: impl Into<String>) -> Error {
    Error::Conversion(msg.into())
}

/// A value of `kind` read from text, `None` when it is not one.
fn parse_as(s: &str, kind: ColumnType) -> Option<Value> {
    let s = s.trim();
    Some(match kind {
        ColumnType::Bool => match s.to_ascii_lowercase().as_str() {
            "1" | "t" | "true" | "yes" | "on" => Value::Bool(true),
            "0" | "f" | "false" | "no" | "off" => Value::Bool(false),
            _ => return None,
        },
        ColumnType::SmallInt | ColumnType::Int => Value::I32(s.parse().ok()?),
        ColumnType::BigInt => Value::I64(s.parse().ok()?),
        ColumnType::Real | ColumnType::Double => Value::F64(s.parse().ok()?),
        ColumnType::Uuid => Value::Uuid(s.parse().ok()?),
        ColumnType::Timestamp => Value::Timestamp(
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f"))
                .or_else(|_| chrono::DateTime::parse_from_rfc3339(s).map(|d| d.naive_utc()))
                .or_else(|_| s.parse::<chrono::NaiveDate>().map(|d| d.and_hms_opt(0, 0, 0).unwrap_or_default()))
                .ok()?,
        ),
        ColumnType::TimestampTz => Value::TimestampTz(
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|d| d.with_timezone(&chrono::Utc))
                .or_else(|_| {
                    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
                        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f"))
                        .map(|n| n.and_utc())
                })
                .or_else(|_| {
                    s.parse::<chrono::NaiveDate>().map(|d| d.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc())
                })
                .ok()?,
        ),
        ColumnType::Date => Value::Date(
            s.parse()
                .ok()
                .or_else(|| chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.date_naive()))
                .or_else(|| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f").ok().map(|d| d.date()))?,
        ),
        ColumnType::Time => Value::Time(s.parse().ok()?),
        ColumnType::Json => Value::Json(serde_json::from_str(s).ok()?),
        ColumnType::Bytes => Value::Bytes(s.as_bytes().to_vec()),
        _ => Value::Text(s.to_owned()),
    })
}

/// The column type of a value (text for NULL).
pub(crate) fn kind_of(v: &Value) -> Option<ColumnType> {
    Some(match v {
        Value::Bool(_) => ColumnType::Bool,
        Value::I32(_) => ColumnType::Int,
        Value::I64(_) => ColumnType::BigInt,
        Value::F64(_) => ColumnType::Double,
        Value::Text(_) => ColumnType::Text,
        Value::Bytes(_) => ColumnType::Bytes,
        Value::Uuid(_) => ColumnType::Uuid,
        Value::Timestamp(_) => ColumnType::Timestamp,
        Value::TimestampTz(_) => ColumnType::TimestampTz,
        Value::Date(_) => ColumnType::Date,
        Value::Time(_) => ColumnType::Time,
        Value::Json(_) => ColumnType::Json,
        _ => return None,
    })
}

fn is_number(v: &Value) -> bool {
    matches!(v, Value::I32(_) | Value::I64(_) | Value::F64(_))
}

/// SQL comparison: `None` when a side is NULL or they cannot be compared.
pub(crate) fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    use Value::*;
    match (a, b) {
        (Null, _) | (_, Null) => None,
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (I32(x), I32(y)) => Some(x.cmp(y)),
        (I32(_) | I64(_), I32(_) | I64(_)) => Some(a.as_i64()?.cmp(&b.as_i64()?)),
        (I32(_) | I64(_) | F64(_), I32(_) | I64(_) | F64(_)) => a.as_f64()?.partial_cmp(&b.as_f64()?),
        (Text(x), Text(y)) => Some(x.as_str().cmp(y.as_str())),
        (Bytes(x), Bytes(y)) => Some(x.cmp(y)),
        (Uuid(x), Uuid(y)) => Some(x.cmp(y)),
        (Timestamp(x), Timestamp(y)) => Some(x.cmp(y)),
        (TimestampTz(x), TimestampTz(y)) => Some(x.cmp(y)),
        (Timestamp(x), TimestampTz(y)) => Some(x.cmp(&y.naive_utc())),
        (TimestampTz(x), Timestamp(y)) => Some(x.naive_utc().cmp(y)),
        (Date(x), Date(y)) => Some(x.cmp(y)),
        // A date against a timestamp: the date at midnight, as in PostgreSQL.
        (Date(x), Timestamp(_) | TimestampTz(_)) => compare(&Timestamp(x.and_hms_opt(0, 0, 0)?), b),
        (Timestamp(_) | TimestampTz(_), Date(y)) => compare(a, &Timestamp(y.and_hms_opt(0, 0, 0)?)),
        (Time(x), Time(y)) => Some(x.cmp(y)),
        (Json(x), Json(y)) => Some(if x == y { Ordering::Equal } else { x.to_string().cmp(&y.to_string()) }),
        // Text against a typed value: the text read as that type (`'2026-11-13'`).
        (Text(t), other) if !is_number(other) => compare(&parse_as(t, kind_of(other)?)?, other),
        (other, Text(t)) if !is_number(other) => compare(other, &parse_as(t, kind_of(other)?)?),
        (Text(t), n) => compare(&Value::F64(t.trim().parse().ok()?), n),
        (n, Text(t)) => compare(n, &Value::F64(t.trim().parse().ok()?)),
        (Bool(x), I32(_) | I64(_)) => compare(&I64(i64::from(*x)), b),
        (I32(_) | I64(_), Bool(y)) => compare(a, &I64(i64::from(*y))),
        _ => None,
    }
}

fn type_rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::I32(_) | Value::I64(_) | Value::F64(_) => 2,
        Value::Text(_) => 3,
        Value::Bytes(_) => 4,
        _ => 5,
    }
}

/// The order of `ORDER BY`: NULLs are the largest values (as in PostgreSQL
/// and Memoire) unless `nulls_first` says otherwise.
pub(crate) fn order_cmp(a: &Value, b: &Value, desc: bool, nulls_first: Option<bool>) -> Ordering {
    let nulls_first = nulls_first.unwrap_or(desc);
    let ord = match (a.is_null(), b.is_null()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return if nulls_first { Ordering::Less } else { Ordering::Greater },
        (false, true) => return if nulls_first { Ordering::Greater } else { Ordering::Less },
        (false, false) => compare(a, b).unwrap_or_else(|| match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => x.is_nan().cmp(&y.is_nan()).then_with(|| x.total_cmp(&y)),
            _ => type_rank(a).cmp(&type_rank(b)),
        }),
    };
    if desc { ord.reverse() } else { ord }
}

/// Truth in WHERE, ON, HAVING and CASE.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::I32(i) => *i != 0,
        Value::I64(i) => *i != 0,
        Value::F64(f) => *f != 0.0,
        _ => false,
    }
}

/// Three-valued truth: `None` for NULL.
pub(crate) fn tri(v: &Value) -> Option<bool> {
    if v.is_null() { None } else { Some(truthy(v)) }
}

pub(crate) fn bool_or_null(b: Option<bool>) -> Value {
    b.map_or(Value::Null, Value::Bool)
}

/// A number from a value (text holding a number included).
fn numeric(v: &Value) -> Option<Value> {
    match v {
        Value::I32(_) | Value::I64(_) | Value::F64(_) => Some(v.clone()),
        Value::Bool(b) => Some(Value::I64(i64::from(*b))),
        Value::Text(t) => {
            let t = t.trim();
            t.parse::<i64>().map(Value::I64).ok().or_else(|| t.parse::<f64>().ok().map(Value::F64))
        }
        _ => None,
    }
}

/// `+ - * / %` (integers stay integers; dividing by zero is an error, as in
/// Memoire), `||`.
pub(crate) fn arith(op: Op, a: &Value, b: &Value) -> Result<Value> {
    use Value::*;
    if a.is_null() || b.is_null() {
        return Ok(Null);
    }
    if op == Op::Concat {
        return Ok(Text(format!("{}{}", to_text(a).unwrap_or_default(), to_text(b).unwrap_or_default())));
    }
    let name = match op {
        Op::Add => "+",
        Op::Sub => "-",
        Op::Mul => "*",
        Op::Div => "/",
        _ => "%",
    };
    let (x, y) = match (numeric(a), numeric(b)) {
        (Some(x), Some(y)) => (x, y),
        _ => {
            // Dates move by a number of days.
            if let (Some(d), Some(n)) = (a.as_date_like(), b.as_i64()) {
                return match op {
                    Op::Add => d.plus_days(n),
                    Op::Sub => d.plus_days(-n),
                    _ => Err(conversion(format!("cannot apply `{name}` to {} and {}", a.type_name(), b.type_name()))),
                };
            }
            return Err(conversion(format!("cannot apply `{name}` to {} and {}", a.type_name(), b.type_name())));
        }
    };
    let zero = || conversion("division by zero");
    match (&x, &y) {
        (I32(_) | I64(_), I32(_) | I64(_)) => {
            let (p, q) = (x.as_i64().unwrap_or(0), y.as_i64().unwrap_or(0));
            let r = match op {
                Op::Add => p.checked_add(q),
                Op::Sub => p.checked_sub(q),
                Op::Mul => p.checked_mul(q),
                Op::Div => {
                    if q == 0 {
                        return Err(zero());
                    }
                    p.checked_div(q)
                }
                _ => {
                    if q == 0 {
                        return Err(zero());
                    }
                    p.checked_rem(q)
                }
            }
            .ok_or_else(|| conversion(format!("integer overflow in {p} {name} {q}")))?;
            Ok(if matches!((&x, &y), (I32(_), I32(_))) { i32::try_from(r).map(I32).unwrap_or(I64(r)) } else { I64(r) })
        }
        _ => {
            let (p, q) = (x.as_f64().unwrap_or(0.0), y.as_f64().unwrap_or(0.0));
            Ok(F64(match op {
                Op::Add => p + q,
                Op::Sub => p - q,
                Op::Mul => p * q,
                Op::Div => {
                    if q == 0.0 {
                        return Err(zero());
                    }
                    p / q
                }
                _ => {
                    if q == 0.0 {
                        return Err(zero());
                    }
                    p % q
                }
            }))
        }
    }
}

trait DateLike {
    fn as_date_like(&self) -> Option<&Value>;
}

impl DateLike for Value {
    fn as_date_like(&self) -> Option<&Value> {
        matches!(self, Value::Date(_) | Value::Timestamp(_) | Value::TimestampTz(_)).then_some(self)
    }
}

trait PlusDays {
    fn plus_days(&self, n: i64) -> Result<Value>;
}

impl PlusDays for &Value {
    fn plus_days(&self, n: i64) -> Result<Value> {
        let d = chrono::Duration::days(n);
        Ok(match self {
            Value::Date(x) => Value::Date(*x + d),
            Value::Timestamp(x) => Value::Timestamp(*x + d),
            Value::TimestampTz(x) => Value::TimestampTz(*x + d),
            _ => Value::Null,
        })
    }
}

/// A value as text (`None` for NULL): what `||`, `lower()`... work on.
pub(crate) fn to_text(v: &Value) -> Option<String> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::I32(i) => i.to_string(),
        Value::I64(i) => i.to_string(),
        Value::F64(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        Value::Text(s) => s.clone(),
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Uuid(u) => u.to_string(),
        Value::Timestamp(t) => t.format("%Y-%m-%d %H:%M:%S%.f").to_string(),
        Value::TimestampTz(t) => t.to_rfc3339(),
        Value::Date(d) => d.to_string(),
        Value::Time(t) => t.to_string(),
        Value::Json(j) => match j {
            Json::String(s) => s.clone(),
            other => other.to_string(),
        },
        other => other.to_string(),
    })
}

/// `CAST(v AS kind)`, and the conversion of a value written into a column.
pub(crate) fn cast(v: &Value, kind: ColumnType) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    if kind_of(v) == Some(kind) || (matches!(kind, ColumnType::Text) && matches!(v, Value::Text(_))) {
        return Ok(v.clone());
    }
    let bad =
        || conversion(format!("cannot convert {} `{}` to {kind:?}", v.type_name(), to_text(v).unwrap_or_default()));
    Ok(match (kind, v) {
        (ColumnType::SmallInt | ColumnType::Int, _) => {
            let n = match numeric(v).ok_or_else(bad)? {
                Value::F64(f) => f.trunc() as i64,
                other => other.as_i64().ok_or_else(bad)?,
            };
            Value::I32(i32::try_from(n).map_err(|_| bad())?)
        }
        (ColumnType::BigInt, _) => match numeric(v).ok_or_else(bad)? {
            Value::F64(f) => Value::I64(f.trunc() as i64),
            other => Value::I64(other.as_i64().ok_or_else(bad)?),
        },
        (ColumnType::Real | ColumnType::Double, _) => Value::F64(numeric(v).and_then(|n| n.as_f64()).ok_or_else(bad)?),
        (ColumnType::Bool, Value::I32(_) | Value::I64(_) | Value::F64(_)) => Value::Bool(truthy(v)),
        (ColumnType::Json, Value::Text(t)) => {
            Value::Json(serde_json::from_str(t).unwrap_or_else(|_| Json::String(t.clone())))
        }
        (ColumnType::Json, other) => Value::Json(to_json(other)),
        (ColumnType::Bytes, Value::Text(t)) => Value::Bytes(t.as_bytes().to_vec()),
        (ColumnType::Date, Value::Timestamp(t)) => Value::Date(t.date()),
        (ColumnType::Date, Value::TimestampTz(t)) => Value::Date(t.date_naive()),
        (ColumnType::Timestamp, Value::TimestampTz(t)) => Value::Timestamp(t.naive_utc()),
        (ColumnType::Timestamp, Value::Date(d)) => Value::Timestamp(d.and_hms_opt(0, 0, 0).unwrap_or_default()),
        (ColumnType::TimestampTz, Value::Timestamp(t)) => Value::TimestampTz(t.and_utc()),
        (ColumnType::TimestampTz, Value::Date(d)) => {
            Value::TimestampTz(d.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc())
        }
        (ColumnType::Text, other) => Value::Text(to_text(other).unwrap_or_default()),
        (kind, other) => parse_as(&to_text(other).ok_or_else(bad)?, kind).ok_or_else(bad)?,
    })
}

/// `LIKE`: `%` any run, `_` any character, `escape` before a literal one.
pub(crate) fn like(s: &str, pattern: &str, ci: bool, escape: Option<char>) -> bool {
    let (s, p): (Vec<char>, Vec<char>) = if ci {
        (s.to_lowercase().chars().collect(), pattern.to_lowercase().chars().collect())
    } else {
        (s.chars().collect(), pattern.chars().collect())
    };
    // Tokens: Some(c) a literal, None('%'), and `_`.
    #[derive(Clone, Copy, PartialEq)]
    enum T {
        Lit(char),
        One,
        Any,
    }
    let mut toks = Vec::with_capacity(p.len());
    let mut i = 0;
    while i < p.len() {
        if Some(p[i]) == escape && i + 1 < p.len() {
            toks.push(T::Lit(p[i + 1]));
            i += 2;
            continue;
        }
        toks.push(match p[i] {
            '%' => T::Any,
            '_' => T::One,
            c => T::Lit(c),
        });
        i += 1;
    }
    // Greedy matching with backtracking on the last `%`.
    let (mut si, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        match toks.get(ti) {
            Some(T::Lit(c)) if *c == s[si] => {
                si += 1;
                ti += 1;
            }
            Some(T::One) => {
                si += 1;
                ti += 1;
            }
            Some(T::Any) => {
                star = Some(ti);
                mark = si;
                ti += 1;
            }
            _ => match star {
                Some(st) => {
                    ti = st + 1;
                    mark += 1;
                    si = mark;
                }
                None => return false,
            },
        }
    }
    toks[ti..].iter().all(|t| *t == T::Any)
}

/// A key for grouping, DISTINCT and hash joins: equal values, equal keys
/// (`1`, `1.0` and `'1'`... are told apart from text only where SQL does).
pub(crate) fn key(v: &Value, out: &mut String) {
    use std::fmt::Write as _;
    match v {
        Value::Null => out.push_str("\u{0}n"),
        Value::I32(i) => {
            let _ = write!(out, "\u{0}i{i}");
        }
        Value::I64(i) => {
            let _ = write!(out, "\u{0}i{i}");
        }
        Value::F64(f) if f.fract() == 0.0 && f.abs() < 9e15 => {
            let _ = write!(out, "\u{0}i{}", *f as i64);
        }
        Value::F64(f) => {
            let _ = write!(out, "\u{0}f{}", f.to_bits());
        }
        Value::Bool(b) => {
            let _ = write!(out, "\u{0}b{b}");
        }
        Value::Text(s) => {
            out.push_str("\u{0}t");
            out.push_str(s);
        }
        other => {
            out.push_str("\u{0}o");
            out.push_str(&to_text(other).unwrap_or_default());
        }
    }
}

/// A parameter of a request, as a value.
pub(crate) fn from_json(j: &Json) -> Value {
    match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => n.as_i64().map(Value::I64).unwrap_or_else(|| Value::F64(n.as_f64().unwrap_or(0.0))),
        Json::String(s) => Value::Text(s.clone()),
        other => Value::Json(other.clone()),
    }
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
        other => memoire_orm::nuage::wire::value_to_json(other),
    }
}

fn json_value(j: &Json) -> Value {
    match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => n.as_i64().map(Value::I64).unwrap_or_else(|| Value::F64(n.as_f64().unwrap_or(0.0))),
        Json::String(s) => Value::Text(s.clone()),
        other => Value::Json(other.clone()),
    }
}

/// `$.a.b[0]` in a JSON document.
fn json_path<'a>(doc: &'a Json, path: &str) -> Option<&'a Json> {
    let path = path.strip_prefix('$').unwrap_or(path);
    let mut at = doc;
    let mut rest = path;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('.') {
            let end = r.find(['.', '[']).unwrap_or(r.len());
            at = at.get(r[..end].trim_matches('"'))?;
            rest = &r[end..];
        } else if let Some(r) = rest.strip_prefix('[') {
            let end = r.find(']')?;
            let inner = r[..end].trim_matches(|c| c == '"' || c == '\'');
            at = match inner.parse::<usize>() {
                Ok(i) => at.get(i)?,
                Err(_) => at.get(inner)?,
            };
            rest = &r[end + 1..];
        } else {
            at = at.get(rest)?;
            break;
        }
    }
    Some(at)
}

fn arg_count(name: &str, args: &[Value], min: usize, max: usize) -> Result<()> {
    if args.len() < min || args.len() > max {
        let want = if min == max { min.to_string() } else { format!("{min} to {max}") };
        return Err(syntax(format!("{name}() takes {want} argument(s), not {}", args.len())));
    }
    Ok(())
}

fn text_arg(v: &Value) -> Option<String> {
    to_text(v)
}

fn int_arg(name: &str, v: &Value) -> Result<Option<i64>> {
    if v.is_null() {
        return Ok(None);
    }
    numeric(v)
        .and_then(|n| match n {
            Value::F64(f) => Some(f as i64),
            other => other.as_i64(),
        })
        .map(Some)
        .ok_or_else(|| conversion(format!("{name}(): `{}` is not a number", to_text(v).unwrap_or_default())))
}

/// The scalar functions: `name` in lower case.
/// An `INTERVAL '...'` as calendar months and microseconds: `1 year 2 months`,
/// `3 days`, `-1 hour`, `90 minutes`, `1 week`, `2 h 30 min`, `1.5 seconds`.
pub(crate) fn interval_months_and_micros(text: &str) -> Result<(i32, i64)> {
    let bad = || {
        syntax(format!(
            "INTERVAL '{text}': write it as `<number> <unit>` (years, months, weeks, days, hours, minutes, seconds)"
        ))
    };
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() || !words.len().is_multiple_of(2) {
        return Err(bad());
    }
    let (mut months, mut micros) = (0i64, 0f64);
    for pair in words.chunks(2) {
        let n: f64 = pair[0].parse().map_err(|_| bad())?;
        let unit = pair[1].to_ascii_lowercase();
        let unit = unit.trim_end_matches('s');
        match unit {
            "year" | "yr" | "y" => months += (n * 12.0) as i64,
            "mon" | "month" => months += n as i64,
            "week" | "w" => micros += n * 7.0 * 86_400e6,
            "day" | "d" => micros += n * 86_400e6,
            "hour" | "hr" | "h" => micros += n * 3_600e6,
            "minute" | "min" | "m" => micros += n * 60e6,
            "second" | "sec" => micros += n * 1e6,
            "millisecond" | "msec" | "ms" => micros += n * 1e3,
            "microsecond" | "u" | "usec" => micros += n,
            _ => return Err(bad()),
        }
    }
    Ok((i32::try_from(months).map_err(|_| bad())?, micros.round() as i64))
}

/// `x + INTERVAL '...'` (`sign` 1) or `x - INTERVAL '...'` (`sign` -1): months
/// first, on the calendar (31 January + 1 month = 28 or 29 February), then
/// the rest. A date gives a timestamp, as in PostgreSQL.
fn shift(x: &Value, text: &str, sign: i64) -> Result<Value> {
    use chrono::{Duration, Months, NaiveDateTime};
    let (months, micros) = interval_months_and_micros(text)?;
    let months = i64::from(months) * sign;
    let step = |t: NaiveDateTime| -> Result<NaiveDateTime> {
        let m = Months::new(months.unsigned_abs() as u32);
        let t = if months >= 0 { t.checked_add_months(m) } else { t.checked_sub_months(m) };
        t.and_then(|t| t.checked_add_signed(Duration::microseconds(micros * sign)))
            .ok_or_else(|| conversion(format!("date out of range after INTERVAL '{text}'")))
    };
    Ok(match x {
        Value::Null => Value::Null,
        Value::TimestampTz(t) => Value::TimestampTz(step(t.naive_utc())?.and_utc()),
        Value::Timestamp(t) => Value::Timestamp(step(*t)?),
        Value::Date(d) => Value::Timestamp(step(d.and_hms_opt(0, 0, 0).expect("midnight"))?),
        other => match cast(other, ColumnType::Timestamp)? {
            Value::Timestamp(t) => Value::Timestamp(step(t)?),
            _ => return Err(conversion("INTERVAL: not a date")),
        },
    })
}

/// Functions that give a set of rows: one row per element of a JSON array.
pub(crate) const SET_RETURNING: [&str; 4] =
    ["jsonb_array_elements_text", "json_array_elements_text", "jsonb_array_elements", "json_array_elements"];

/// The rows of a [`SET_RETURNING`] function: the elements of the JSON array
/// `arg`, as text for the `_text` variants (like PostgreSQL), as values for the
/// others. `NULL` gives no row.
pub(crate) fn set_returning(name: &str, arg: &Value) -> Result<Vec<Value>> {
    let doc = match arg {
        Value::Null => return Ok(Vec::new()),
        Value::Json(j) => j.clone(),
        other => serde_json::from_str(&text_arg(other).unwrap_or_default())
            .map_err(|_| conversion(format!("{name}(): not JSON")))?,
    };
    let Json::Array(items) = doc else {
        return Err(conversion(format!("{name}(): cannot extract elements from a JSON value that is not an array")));
    };
    let text = name.ends_with("_text");
    Ok(items
        .into_iter()
        .map(|j| match j {
            Json::Null => Value::Null,
            Json::String(s) if text => Value::Text(s),
            other if text => Value::Text(other.to_string()),
            other => json_value(&other),
        })
        .collect())
}

pub(crate) fn scalar(name: &str, args: Vec<Value>) -> Result<Value> {
    let a = &args;
    let null_in = || a.iter().any(Value::is_null);
    let num = |v: &Value| {
        numeric(v).ok_or_else(|| conversion(format!("{name}(): `{}` is not a number", to_text(v).unwrap_or_default())))
    };
    Ok(match name {
        "lower" | "upper" | "length" | "char_length" | "character_length" | "octet_length" | "trim" | "ltrim"
        | "rtrim" | "reverse" | "initcap" | "md5" | "sha256" | "hex" | "unhex" | "typeof" | "quote" => {
            let max = if matches!(name, "trim" | "ltrim" | "rtrim") { 2 } else { 1 };
            arg_count(name, a, 1, max)?;
            if name == "typeof" {
                return Ok(Value::Text(if a[0].is_null() {
                    "null".into()
                } else {
                    a[0].type_name().to_ascii_lowercase()
                }));
            }
            let Some(s) = text_arg(&a[0]) else { return Ok(Value::Null) };
            let chars: Option<Vec<char>> = a.get(1).and_then(text_arg).map(|c| c.chars().collect());
            let set = |c: char| chars.as_ref().map_or(c.is_whitespace(), |cs| cs.contains(&c));
            match name {
                "lower" => Value::Text(s.to_lowercase()),
                "upper" => Value::Text(s.to_uppercase()),
                "length" | "char_length" | "character_length" => match &a[0] {
                    Value::Bytes(b) => Value::I64(b.len() as i64),
                    _ => Value::I64(s.chars().count() as i64),
                },
                "octet_length" => Value::I64(s.len() as i64),
                "trim" => Value::Text(s.trim_matches(set).to_owned()),
                "ltrim" => Value::Text(s.trim_start_matches(set).to_owned()),
                "rtrim" => Value::Text(s.trim_end_matches(set).to_owned()),
                "reverse" => Value::Text(s.chars().rev().collect()),
                "initcap" => Value::Text(
                    s.split(' ')
                        .map(|w| {
                            let mut c = w.chars();
                            c.next().map_or(String::new(), |f| {
                                f.to_uppercase().chain(c.flat_map(char::to_lowercase)).collect()
                            })
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                "md5" => return Err(syntax("md5() is not available: use sha256()")),
                "sha256" => Value::Text(crate::hash::sha256(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()),
                "hex" => Value::Text(match &a[0] {
                    Value::Bytes(b) => b.iter().map(|x| format!("{x:02X}")).collect(),
                    _ => s.bytes().map(|x| format!("{x:02X}")).collect(),
                }),
                "unhex" => {
                    let bytes: Option<Vec<u8>> = (0..s.len())
                        .step_by(2)
                        .map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
                        .collect();
                    bytes.map_or(Value::Null, Value::Bytes)
                }
                _ => Value::Text(match &a[0] {
                    Value::Text(t) => format!("'{}'", t.replace('\'', "''")),
                    _ => s,
                }),
            }
        }
        "substr" | "substring" => {
            arg_count(name, a, 2, 3)?;
            if null_in() {
                return Ok(Value::Null);
            }
            let s: Vec<char> = text_arg(&a[0]).unwrap_or_default().chars().collect();
            let start = int_arg(name, &a[1])?.unwrap_or(1);
            let len = match a.get(2) {
                Some(v) => int_arg(name, v)?,
                None => None,
            };
            // 1-based; a start of 0 or less eats into the length, as in SQL.
            let from = (start - 1).max(0) as usize;
            let end = match len {
                Some(l) => ((start - 1 + l).max(0) as usize).min(s.len()),
                None => s.len(),
            };
            Value::Text(if from >= end { String::new() } else { s[from..end].iter().collect() })
        }
        "left" | "right" => {
            arg_count(name, a, 2, 2)?;
            if null_in() {
                return Ok(Value::Null);
            }
            let s: Vec<char> = text_arg(&a[0]).unwrap_or_default().chars().collect();
            let n = (int_arg(name, &a[1])?.unwrap_or(0).max(0) as usize).min(s.len());
            Value::Text(if name == "left" { s[..n].iter().collect() } else { s[s.len() - n..].iter().collect() })
        }
        "replace" => {
            arg_count(name, a, 3, 3)?;
            if null_in() {
                return Ok(Value::Null);
            }
            let (s, from, to) = (
                text_arg(&a[0]).unwrap_or_default(),
                text_arg(&a[1]).unwrap_or_default(),
                text_arg(&a[2]).unwrap_or_default(),
            );
            Value::Text(if from.is_empty() { s } else { s.replace(&from, &to) })
        }
        "instr" | "strpos" | "position" => {
            arg_count(name, a, 2, 2)?;
            if null_in() {
                return Ok(Value::Null);
            }
            let (s, sub) = (text_arg(&a[0]).unwrap_or_default(), text_arg(&a[1]).unwrap_or_default());
            Value::I64(s.find(&sub).map_or(0, |i| s[..i].chars().count() as i64 + 1))
        }
        "lpad" | "rpad" => {
            arg_count(name, a, 2, 3)?;
            if null_in() {
                return Ok(Value::Null);
            }
            let s: Vec<char> = text_arg(&a[0]).unwrap_or_default().chars().collect();
            let n = int_arg(name, &a[1])?.unwrap_or(0).max(0) as usize;
            let fill: Vec<char> = a.get(2).and_then(text_arg).unwrap_or_else(|| " ".into()).chars().collect();
            if n <= s.len() || fill.is_empty() {
                return Ok(Value::Text(s[..n.min(s.len())].iter().collect()));
            }
            let pad: String = fill.iter().cycle().take(n - s.len()).collect();
            let s: String = s.iter().collect();
            Value::Text(if name == "lpad" { pad + &s } else { s + &pad })
        }
        "repeat" => {
            arg_count(name, a, 2, 2)?;
            if null_in() {
                return Ok(Value::Null);
            }
            let n = int_arg(name, &a[1])?.unwrap_or(0).clamp(0, 1_000_000) as usize;
            Value::Text(text_arg(&a[0]).unwrap_or_default().repeat(n))
        }
        "concat" => Value::Text(a.iter().filter_map(text_arg).collect()),
        "concat_ws" => {
            if a.is_empty() {
                return Err(syntax("concat_ws() needs a separator"));
            }
            let Some(sep) = text_arg(&a[0]) else { return Ok(Value::Null) };
            Value::Text(a[1..].iter().filter_map(text_arg).collect::<Vec<_>>().join(&sep))
        }
        "coalesce" | "ifnull" | "nvl" => {
            if a.is_empty() {
                return Err(syntax(format!("{name}() needs arguments")));
            }
            a.iter().find(|v| !v.is_null()).cloned().unwrap_or(Value::Null)
        }
        "nullif" => {
            arg_count(name, a, 2, 2)?;
            if compare(&a[0], &a[1]) == Some(Ordering::Equal) { Value::Null } else { a[0].clone() }
        }
        "_same" => Value::Bool(match (a[0].is_null(), a[1].is_null()) {
            (true, true) => true,
            (false, false) => compare(&a[0], &a[1]) == Some(Ordering::Equal),
            _ => false,
        }),
        "iif" | "if" => {
            arg_count(name, a, 3, 3)?;
            if truthy(&a[0]) { a[1].clone() } else { a[2].clone() }
        }
        "greatest" | "least" | "max" | "min" => {
            // `max(a, b)` of SQLite: the scalar form (one argument is the aggregate).
            let want = if matches!(name, "greatest" | "max") { Ordering::Greater } else { Ordering::Less };
            let mut best: Option<&Value> = None;
            for v in a.iter().filter(|v| !v.is_null()) {
                best = match best {
                    Some(b) if compare(v, b) != Some(want) => Some(b),
                    _ => Some(v),
                };
            }
            best.cloned().unwrap_or(Value::Null)
        }
        "abs" | "sign" | "floor" | "ceil" | "ceiling" | "sqrt" | "exp" | "ln" | "log10" | "trunc" => {
            arg_count(name, a, 1, 1)?;
            if a[0].is_null() {
                return Ok(Value::Null);
            }
            let n = num(&a[0])?;
            match (name, &n) {
                ("abs", Value::I64(i)) => {
                    Value::I64(i.checked_abs().ok_or_else(|| conversion("integer overflow in abs()"))?)
                }
                ("abs", Value::I32(i)) => {
                    Value::I32(i.checked_abs().ok_or_else(|| conversion("integer overflow in abs()"))?)
                }
                ("sign", Value::I32(_) | Value::I64(_)) => Value::I64(n.as_i64().unwrap_or(0).signum()),
                ("floor" | "ceil" | "ceiling" | "trunc", Value::I32(_) | Value::I64(_)) => n,
                _ => {
                    let f = n.as_f64().unwrap_or(0.0);
                    Value::F64(match name {
                        "abs" => f.abs(),
                        "sign" => {
                            if f == 0.0 {
                                0.0
                            } else {
                                f.signum()
                            }
                        }
                        "floor" => f.floor(),
                        "ceil" | "ceiling" => f.ceil(),
                        "trunc" => f.trunc(),
                        "sqrt" => f.sqrt(),
                        "exp" => f.exp(),
                        "ln" => f.ln(),
                        _ => f.log10(),
                    })
                }
            }
        }
        "round" => {
            arg_count(name, a, 1, 2)?;
            if null_in() {
                return Ok(Value::Null);
            }
            let n = num(&a[0])?;
            let digits = match a.get(1) {
                Some(d) => int_arg(name, d)?.unwrap_or(0),
                None => 0,
            };
            match n {
                Value::I32(_) | Value::I64(_) if digits >= 0 => n,
                _ => {
                    let m = 10f64.powi(digits.clamp(-15, 15) as i32);
                    Value::F64((n.as_f64().unwrap_or(0.0) * m).round() / m)
                }
            }
        }
        "power" | "pow" | "mod" | "atan2" | "log" => {
            arg_count(name, a, 2, 2)?;
            if null_in() {
                return Ok(Value::Null);
            }
            if name == "mod" {
                return arith(Op::Mod, &a[0], &a[1]);
            }
            let (x, y) = (num(&a[0])?.as_f64().unwrap_or(0.0), num(&a[1])?.as_f64().unwrap_or(0.0));
            Value::F64(match name {
                "atan2" => x.atan2(y),
                "log" => y.log(x),
                _ => x.powf(y),
            })
        }
        "pi" => Value::F64(std::f64::consts::PI),
        "random" => {
            let r: [u8; 8] = crate::crypto::random();
            Value::I64(i64::from_le_bytes(r))
        }
        "gen_random_uuid" | "uuid" | "uuid_generate_v4" => {
            let mut b: [u8; 16] = crate::crypto::random();
            b[6] = (b[6] & 0x0f) | 0x40;
            b[8] = (b[8] & 0x3f) | 0x80;
            Value::Uuid(uuid::Uuid::from_bytes(b))
        }
        "now" | "current_timestamp" => Value::TimestampTz(chrono::Utc::now()),
        "current_date" => Value::Date(chrono::Utc::now().date_naive()),
        "current_time" => Value::Time(chrono::Utc::now().time()),
        "date" | "datetime" | "time" => {
            arg_count(name, a, 1, 1)?;
            let v = if matches!(&a[0], Value::Text(t) if t.eq_ignore_ascii_case("now")) {
                Value::TimestampTz(chrono::Utc::now())
            } else {
                a[0].clone()
            };
            cast(
                &v,
                match name {
                    "date" => ColumnType::Date,
                    "time" => ColumnType::Time,
                    _ => ColumnType::Timestamp,
                },
            )?
        }
        "extract" | "date_part" => {
            arg_count(name, a, 2, 2)?;
            if null_in() {
                return Ok(Value::Null);
            }
            use chrono::{Datelike, Timelike};
            let field = text_arg(&a[0]).unwrap_or_default().to_ascii_lowercase();
            let t = match cast(&a[1], ColumnType::Timestamp) {
                Ok(Value::Timestamp(t)) => t,
                _ => {
                    return Err(conversion(format!(
                        "{name}(): `{}` is not a date",
                        to_text(&a[1]).unwrap_or_default()
                    )));
                }
            };
            Value::I64(match field.as_str() {
                "year" => i64::from(t.year()),
                "month" => i64::from(t.month()),
                "day" => i64::from(t.day()),
                "hour" => i64::from(t.hour()),
                "minute" => i64::from(t.minute()),
                "second" => i64::from(t.second()),
                "dow" => i64::from(t.weekday().num_days_from_sunday()),
                "doy" => i64::from(t.ordinal()),
                "epoch" => t.and_utc().timestamp(),
                other => return Err(syntax(format!("unknown date part `{other}`"))),
            })
        }
        "strftime" => {
            arg_count(name, a, 2, 2)?;
            if null_in() {
                return Ok(Value::Null);
            }
            let fmt = text_arg(&a[0]).unwrap_or_default();
            let t = match cast(&a[1], ColumnType::Timestamp) {
                Ok(Value::Timestamp(t)) => t,
                _ => return Err(conversion("strftime(): not a date")),
            };
            Value::Text(t.format(&fmt.replace("%f", "%.3f")).to_string())
        }
        "json_extract" | "json_value" => {
            arg_count(name, a, 2, 2)?;
            if null_in() {
                return Ok(Value::Null);
            }
            let doc = match &a[0] {
                Value::Json(j) => j.clone(),
                other => serde_json::from_str(&text_arg(other).unwrap_or_default())
                    .map_err(|_| conversion(format!("{name}(): not JSON")))?,
            };
            json_path(&doc, &text_arg(&a[1]).unwrap_or_default()).map_or(Value::Null, json_value)
        }
        "json_array_length" => {
            arg_count(name, a, 1, 1)?;
            match &a[0] {
                Value::Json(Json::Array(l)) => Value::I64(l.len() as i64),
                Value::Text(t) => match serde_json::from_str::<Json>(t) {
                    Ok(Json::Array(l)) => Value::I64(l.len() as i64),
                    _ => Value::Null,
                },
                _ => Value::Null,
            }
        }
        "json_object" => {
            let mut m = serde_json::Map::new();
            for pair in a.chunks(2) {
                if let [k, v] = pair {
                    m.insert(text_arg(k).unwrap_or_default(), to_json(v));
                }
            }
            Value::Json(Json::Object(m))
        }
        "json_array" => Value::Json(Json::Array(a.iter().map(to_json).collect())),
        "_interval" => {
            arg_count(name, a, 1, 1)?;
            Value::Text(text_arg(&a[0]).unwrap_or_default())
        }
        "_shift" => {
            arg_count(name, a, 3, 3)?;
            let sign = if matches!(a[2], Value::I64(-1)) { -1 } else { 1 };
            shift(&a[0], &text_arg(&a[1]).unwrap_or_default(), sign)?
        }
        // PostgreSQL's advisory locks. A transaction of Nuage DB is checked at
        // its commit: if another commit changed a table it read since its
        // snapshot, it fails with a conflict (`40001`). The section that the
        // lock would serialize (read, then write) is therefore already safe,
        // and taking the lock has nothing more to do.
        "pg_advisory_lock" | "pg_advisory_xact_lock" | "pg_advisory_lock_shared" | "pg_advisory_xact_lock_shared" => {
            arg_count(name, a, 1, 2)?;
            Value::Null
        }
        "pg_advisory_unlock" | "pg_advisory_unlock_shared" | "pg_try_advisory_lock" | "pg_try_advisory_xact_lock" => {
            arg_count(name, a, 1, 2)?;
            Value::Bool(true)
        }
        "pg_advisory_unlock_all" => {
            arg_count(name, a, 0, 0)?;
            Value::Null
        }
        // A stable 64-bit hash of a text (and a seed): the key of an advisory
        // lock. Its values are not PostgreSQL's, only as stable.
        "hashtext" | "hashtextextended" => {
            arg_count(name, a, 1, if name == "hashtext" { 1 } else { 2 })?;
            if null_in() {
                return Ok(Value::Null);
            }
            let seed = match a.get(1) {
                Some(Value::I64(n)) => *n as u64,
                Some(Value::I32(n)) => *n as u64,
                Some(other) => to_text(other).and_then(|t| t.parse::<i64>().ok()).unwrap_or(0) as u64,
                None => 0,
            };
            let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ seed;
            for b in text_arg(&a[0]).unwrap_or_default().bytes() {
                h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
            }
            if name == "hashtext" { Value::I32(h as i32) } else { Value::I64(h as i64) }
        }
        n if SET_RETURNING.contains(&n) => {
            return Err(syntax(format!(
                "{n}() gives a set of rows: use it alone in a SELECT without FROM, as in `x IN (SELECT {n}(...))`"
            )));
        }
        "last_insert_id" | "last_insert_rowid" | "changes" => {
            return Err(syntax(format!("{name}() is not available: use RETURNING, or `last_insert_id` in the answer")));
        }
        other => return Err(syntax(format!("unknown function {other}()"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un paramètre JSON de `/v1/sql` arrive bit pour bit : une latitude réelle
    /// (copiée d'une base PostgreSQL) était lue un ULP plus loin.
    #[test]
    fn json_doubles_exacts() {
        for x in [44.833800000000004_f64, 0.1 + 0.2, 1e-310, f64::MAX, -2.2250738585072014e-308] {
            let j: Json = serde_json::from_str(&serde_json::to_string(&x).unwrap()).unwrap();
            assert_eq!(from_json(&j), Value::F64(x), "{x:?}");
        }
    }

    #[test]
    fn values() {
        assert_eq!(compare(&Value::I32(3), &Value::F64(3.0)), Some(Ordering::Equal));
        assert_eq!(
            compare(&Value::Text("2026-11-13".into()), &Value::Date("2026-11-13".parse().unwrap())),
            Some(Ordering::Equal)
        );
        assert_eq!(compare(&Value::Null, &Value::I64(1)), None);
        let day = |s: &str| Value::Date(s.parse().unwrap());
        let at = |s: &str| Value::Timestamp(chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap());
        assert_eq!(compare(&at("2026-02-28 00:00:00"), &day("2026-03-01")), Some(Ordering::Less));
        assert_eq!(compare(&day("2026-03-01"), &at("2026-03-01 00:00:00")), Some(Ordering::Equal));
        assert_eq!(compare(&day("2026-03-01"), &at("2026-03-01 09:00:00")), Some(Ordering::Less));
        assert_eq!(arith(Op::Div, &Value::I64(7), &Value::I64(2)).unwrap(), Value::I64(3));
        assert_eq!(arith(Op::Mod, &Value::I64(7), &Value::I64(2)).unwrap(), Value::I64(1));
        assert!(arith(Op::Div, &Value::I64(1), &Value::I64(0)).is_err());
        assert_eq!(arith(Op::Concat, &Value::Text("a".into()), &Value::I64(1)).unwrap(), Value::Text("a1".into()));
        assert_eq!(arith(Op::Add, &Value::Null, &Value::I64(1)).unwrap(), Value::Null);
        assert_eq!(cast(&Value::Text("42".into()), ColumnType::Int).unwrap(), Value::I32(42));
        assert_eq!(cast(&Value::F64(2.9), ColumnType::BigInt).unwrap(), Value::I64(2));
        assert!(cast(&Value::Text("x".into()), ColumnType::Int).is_err());
        assert!(
            like("Hello", "H%o", false, None)
                && like("Hello", "h_LLO", true, None)
                && !like("Hello", "h%", false, None)
        );
        assert!(like("50%", "50!%", false, Some('!')) && !like("500", "50!%", false, Some('!')));
        assert!(like("abc", "%", false, None) && like("", "%", false, None) && !like("abc", "%d%", false, None));
        let (mut k1, mut k2) = (String::new(), String::new());
        key(&Value::I32(1), &mut k1);
        key(&Value::F64(1.0), &mut k2);
        assert_eq!(k1, k2);
    }

    #[test]
    fn functions() {
        let t = |s: &str| Value::Text(s.into());
        let f = |n: &str, a: Vec<Value>| scalar(n, a).unwrap();
        assert_eq!(f("upper", vec![t("été")]), t("ÉTÉ"));
        assert_eq!(f("length", vec![t("été")]), Value::I64(3));
        assert_eq!(f("substr", vec![t("bonjour"), Value::I64(4)]), t("jour"));
        assert_eq!(f("substr", vec![t("bonjour"), Value::I64(1), Value::I64(3)]), t("bon"));
        assert_eq!(f("trim", vec![t("  x ")]), t("x"));
        assert_eq!(f("replace", vec![t("a-b-c"), t("-"), t("+")]), t("a+b+c"));
        assert_eq!(f("coalesce", vec![Value::Null, Value::I64(2)]), Value::I64(2));
        assert_eq!(f("nullif", vec![Value::I64(2), Value::I64(2)]), Value::Null);
        assert_eq!(f("round", vec![Value::F64(2.345), Value::I64(2)]), Value::F64(2.35));
        assert_eq!(f("abs", vec![Value::I64(-3)]), Value::I64(3));
        assert_eq!(f("concat", vec![t("a"), Value::Null, Value::I64(1)]), t("a1"));
        assert_eq!(f("json_extract", vec![t(r#"{"a":{"b":[10,20]}}"#), t("$.a.b[1]")]), Value::I64(20));
        assert_eq!(f("max", vec![Value::I64(1), Value::I64(5), Value::Null]), Value::I64(5));
        assert_eq!(f("instr", vec![t("bonjour"), t("jour")]), Value::I64(4));
        assert_eq!(f("lpad", vec![t("7"), Value::I64(3), t("0")]), t("007"));
        assert_eq!(f("extract", vec![t("year"), t("2026-11-13 10:00:00")]), Value::I64(2026));
        assert!(matches!(f("now", vec![]), Value::TimestampTz(_)));
        assert!(scalar("nope", vec![]).is_err());
    }
}
