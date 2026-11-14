//! How Nuage DB lays its data out as ordered keys.
//!
//! Every branch is one ordered key-value space. Keys:
//!
//! | Key | Value |
//! |---|---|
//! | `C` + table name | the table's metadata (JSON: generation, columns, indexes) |
//! | `D` + generation + `r` + primary key | the row |
//! | `D` + generation + `i` + index number + indexed values + primary key | empty |
//! | `D` + generation + `s` | last generated id |
//! | `D` + generation + `n` | number of rows |
//! | `D` + generation + `b` | bytes of the stored rows |
//!
//! A table's data lives under its *generation*, a number given when the table
//! is created: dropping a table only removes its catalog entry, so a branch
//! made before the drop still reads the old generation.
//!
//! Values in keys use an order-preserving encoding that follows Memoire's key
//! equality (`1 = 1.0`, like the in-memory engine's indexes), so equal values
//! give equal bytes and integer keys sort numerically.

use memoire_orm::evaluator::{decode_value, encode_value};
use memoire_orm::{Result, Value};

pub(crate) const CATALOG: u8 = b'C';
pub(crate) const DATA: u8 = b'D';
pub(crate) const ROW: u8 = b'r';
pub(crate) const INDEX: u8 = b'i';
pub(crate) const SEQ: u8 = b's';
pub(crate) const COUNT: u8 = b'n';
pub(crate) const BYTES: u8 = b'b';

/// Text in a key: `0x00` is escaped as `0x00 0xFF` and the end is `0x00 0x00`,
/// so a shorter string sorts first and keys can follow it.
fn push_escaped(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
}

fn push_i64(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(&((v as u64) ^ (1 << 63)).to_be_bytes());
}

/// Appends the order-preserving form of `v`.
pub(crate) fn push_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(0),
        Value::Bool(b) => {
            out.push(1);
            out.push(u8::from(*b));
        }
        Value::I32(i) => {
            out.push(2);
            push_i64(out, i64::from(*i));
        }
        Value::I64(i) => {
            out.push(2);
            push_i64(out, *i);
        }
        Value::F64(f) if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 => {
            out.push(2);
            push_i64(out, *f as i64);
        }
        Value::F64(f) => {
            out.push(3);
            let bits = if f.is_nan() { u64::MAX } else { f.to_bits() };
            // Negative floats sort reversed: flip every bit; positive ones the sign.
            let ordered = if bits >> 63 == 1 { !bits } else { bits ^ (1 << 63) };
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        Value::Text(s) => {
            out.push(4);
            push_escaped(out, s.as_bytes());
        }
        Value::Bytes(b) => {
            out.push(5);
            push_escaped(out, b);
        }
        Value::Uuid(u) => {
            out.push(6);
            out.extend_from_slice(u.as_bytes());
        }
        Value::Timestamp(t) => {
            out.push(7);
            push_i64(out, t.and_utc().timestamp_nanos_opt().unwrap_or(i64::MAX));
        }
        Value::TimestampTz(t) => {
            out.push(7);
            push_i64(out, t.timestamp_nanos_opt().unwrap_or(i64::MAX));
        }
        Value::Date(d) => {
            out.push(8);
            push_i64(out, i64::from(chrono::Datelike::num_days_from_ce(d)));
        }
        Value::Time(t) => {
            use chrono::Timelike;
            out.push(9);
            push_i64(out, i64::from(t.num_seconds_from_midnight()) * 1_000_000_000 + i64::from(t.nanosecond()));
        }
        Value::Json(j) => {
            out.push(10);
            push_escaped(out, j.to_string().as_bytes());
        }
        // A value type added to Memoire later: its debug form keeps keys
        // distinct.
        other => {
            out.push(255);
            push_escaped(out, format!("{other:?}").as_bytes());
        }
    }
}

pub(crate) fn catalog_key(table: &str) -> Vec<u8> {
    let mut k = vec![CATALOG];
    push_escaped(&mut k, table.as_bytes());
    k
}

/// The catalog prefix: every table.
pub(crate) fn catalog_range() -> (Vec<u8>, Vec<u8>) {
    (vec![CATALOG], vec![CATALOG + 1])
}

/// The table name of a catalog key.
pub(crate) fn catalog_name(key: &[u8]) -> Option<String> {
    let body = key.strip_prefix(&[CATALOG])?;
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        match (body[i], body.get(i + 1)) {
            (0, Some(0)) => break,
            (0, Some(0xFF)) => {
                out.push(0);
                i += 2;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

pub(crate) fn data_prefix(generation: u64, kind: u8) -> Vec<u8> {
    let mut k = Vec::with_capacity(16);
    k.push(DATA);
    k.extend_from_slice(&generation.to_be_bytes());
    k.push(kind);
    k
}

/// Every key of a table generation.
pub(crate) fn generation_range(generation: u64) -> (Vec<u8>, Vec<u8>) {
    let mut start = vec![DATA];
    start.extend_from_slice(&generation.to_be_bytes());
    let mut end = vec![DATA];
    end.extend_from_slice(&(generation + 1).to_be_bytes());
    (start, end)
}

pub(crate) fn rows_range(generation: u64) -> (Vec<u8>, Vec<u8>) {
    (data_prefix(generation, ROW), data_prefix(generation, ROW + 1))
}

/// `D gen i <index> <values>`: the prefix of every entry with these values.
pub(crate) fn index_prefix(generation: u64, index: u16, values: &[&Value]) -> Vec<u8> {
    let mut k = data_prefix(generation, INDEX);
    k.extend_from_slice(&index.to_be_bytes());
    for v in values {
        push_value(&mut k, v);
    }
    k
}

/// The end of a prefix range: the prefix with its last byte raised, carrying
/// past `0xFF` bytes.
pub(crate) fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xFF {
            end.push(last + 1);
            return end;
        }
    }
    vec![0xFF; prefix.len() + 1]
}

pub(crate) fn seq_key(generation: u64) -> Vec<u8> {
    data_prefix(generation, SEQ)
}

pub(crate) fn count_key(generation: u64) -> Vec<u8> {
    data_prefix(generation, COUNT)
}

pub(crate) fn bytes_key(generation: u64) -> Vec<u8> {
    data_prefix(generation, BYTES)
}

/// A row, in the order of the table's stored columns.
pub(crate) fn encode_row(row: &[Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 * row.len());
    out.extend_from_slice(&(row.len() as u32).to_le_bytes());
    for v in row {
        encode_value(&mut out, v);
    }
    out
}

pub(crate) fn decode_row(bytes: &[u8]) -> Result<Vec<Value>> {
    let (n, mut rest) = bytes.split_at_checked(4).ok_or_else(|| super::storage_err("truncated row"))?;
    let n = u32::from_le_bytes(n.try_into().unwrap_or_default()) as usize;
    (0..n).map(|_| decode_value(&mut rest)).collect()
}

/// Moves `input` past one value written by Memoire's `encode_value`, without
/// making it: nothing is allocated for a text or a JSON nobody reads.
fn skip_value(input: &mut &[u8]) -> Result<()> {
    let fixed = |n: usize| Some(1 + n);
    let len = match input.first() {
        Some(0) => fixed(0),
        Some(1) => fixed(1),
        Some(2 | 10) => fixed(4),
        Some(3 | 4 | 11) => fixed(8),
        Some(7) => fixed(16),
        Some(8 | 9) => fixed(12),
        // Text, bytes, JSON: a length on four bytes, then the content.
        Some(5 | 6 | 12) => input.get(1..5).and_then(|n| Some(5 + u32::from_le_bytes(n.try_into().ok()?) as usize)),
        _ => None,
    };
    match len.and_then(|n| input.get(n..)) {
        Some(rest) => {
            *input = rest;
            Ok(())
        }
        // A kind this does not know, or a row cut short: decoding says what is wrong.
        None => decode_value(input).map(|_| ()),
    }
}

/// [`decode_row`] for the stored columns `want` marks: the others are left
/// `NULL`, unread.
pub(crate) fn decode_row_some(bytes: &[u8], want: &[bool]) -> Result<Vec<Value>> {
    let (n, mut rest) = bytes.split_at_checked(4).ok_or_else(|| super::storage_err("truncated row"))?;
    let n = u32::from_le_bytes(n.try_into().unwrap_or_default()) as usize;
    (0..n)
        .map(|i| {
            if want.get(i).copied().unwrap_or(false) {
                decode_value(&mut rest)
            } else {
                skip_value(&mut rest).map(|()| Value::Null)
            }
        })
        .collect()
}

pub(crate) fn encode_i64(v: i64) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

pub(crate) fn decode_i64(bytes: &[u8]) -> i64 {
    bytes.try_into().map(i64::from_le_bytes).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(v: Value) -> Vec<u8> {
        let mut out = Vec::new();
        push_value(&mut out, &v);
        out
    }

    #[test]
    fn keys_sort_like_values() {
        assert!(enc(Value::I64(-5)) < enc(Value::I64(3)));
        assert!(enc(Value::I64(3)) < enc(Value::I64(300)));
        assert_eq!(enc(Value::I32(7)), enc(Value::I64(7)));
        assert_eq!(enc(Value::F64(7.0)), enc(Value::I64(7)));
        assert!(enc(Value::F64(-2.5)) < enc(Value::F64(-1.5)));
        assert!(enc(Value::F64(-1.5)) < enc(Value::F64(0.5)));
        assert!(enc(Value::Text("ab".into())) < enc(Value::Text("abc".into())));
        assert!(enc(Value::Text("a\0z".into())) > enc(Value::Text("a".into())));
        assert_eq!(catalog_name(&catalog_key("we\0ird")).as_deref(), Some("we\0ird"));
        assert_eq!(prefix_end(&[1, 0xFF]), vec![2]);
    }

    #[test]
    fn rows_round_trip() {
        let row = vec![Value::I64(1), Value::Text("é".into()), Value::Null, Value::Bytes(vec![0, 1])];
        assert_eq!(decode_row(&encode_row(&row)).unwrap(), row);
    }

    /// Any subset of the columns of a row with every kind of value: the ones
    /// asked for come out as they went in, the others are skipped whole.
    #[test]
    fn columns_not_asked_for_are_skipped() {
        let at = chrono::DateTime::from_timestamp(1_700_000_000, 123).unwrap();
        let row = vec![
            Value::Null,
            Value::Bool(true),
            Value::I32(-7),
            Value::I64(1 << 40),
            Value::F64(2.5),
            Value::Text("été".into()),
            Value::Text(String::new()),
            Value::Bytes(vec![0, 255, 3]),
            Value::Uuid(uuid::Uuid::from_bytes([9; 16])),
            Value::Timestamp(at.naive_utc()),
            Value::TimestampTz(at),
            Value::Date(at.date_naive()),
            Value::Time(at.time()),
            Value::Json(serde_json::json!({ "a": [1, "x", null] })),
            Value::I64(42),
        ];
        let bytes = encode_row(&row);
        for mask in 0u32..(1 << row.len()) {
            // Every 37th subset, and the ones with a single column or all but one.
            let ones = mask.count_ones() as usize;
            if mask % 37 != 0 && ones != 1 && ones != row.len() - 1 {
                continue;
            }
            let want: Vec<bool> = (0..row.len()).map(|i| mask & (1 << i) != 0).collect();
            let expected: Vec<Value> =
                row.iter().zip(&want).map(|(v, w)| if *w { v.clone() } else { Value::Null }).collect();
            assert_eq!(decode_row_some(&bytes, &want).unwrap(), expected, "{want:?}");
        }
        // A shorter mask leaves the rest unread; a row cut short is an error either way.
        assert_eq!(decode_row_some(&bytes, &[false, true]).unwrap()[1], Value::Bool(true));
        assert!(decode_row_some(&bytes[..bytes.len() - 3], &[]).is_err());
        assert!(decode_row_some(&bytes[..40], &[]).is_err());
    }
}
