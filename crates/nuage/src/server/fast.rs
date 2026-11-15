//! What keeps a request short: reads that run where they are, answers
//! written straight as their text, and a compressor that is made once.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use memoire_orm::nuage::wire::OwnedQuery;
use memoire_orm::{Error, Expr, Result, Value};

use crate::sql::SqlResult;

/// A read that takes longer than this, on average, goes to the blocking pool.
const SLOW_MICROS: u32 = 500;

/// How long the reads of each shape take.
///
/// A read served from the page cache takes a few microseconds: handing it to
/// another thread and waiting for the answer costs more than the read. So a
/// read runs where it is, on the connection's own task, as long as reads of
/// its shape (the same table, the same clauses) are short; a shape that turns
/// out slow (a scan, a cold disk) goes to the blocking pool, where it holds up
/// nobody, and comes back when it is short again. Writes always go to the
/// pool: they wait for the disk.
pub(crate) struct ReadCosts {
    /// The average of each shape, in microseconds; shapes that share a slot
    /// share their average, which only makes the choice more careful.
    slots: [AtomicU32; 256],
}

impl ReadCosts {
    pub(crate) fn new() -> ReadCosts {
        ReadCosts { slots: std::array::from_fn(|_| AtomicU32::new(0)) }
    }

    fn slot(&self, shape: u64) -> &AtomicU32 {
        &self.slots[((shape ^ (shape >> 32)) & 255) as usize]
    }

    pub(crate) fn slow(&self, shape: u64) -> bool {
        self.slot(shape).load(Ordering::Relaxed) > SLOW_MICROS
    }

    pub(crate) fn record(&self, shape: u64, took: Duration) {
        let micros = took.as_micros().min(10_000_000) as u32;
        let slot = self.slot(shape);
        let old = slot.load(Ordering::Relaxed);
        // The first sample counts whole, then a moving average.
        let new = if old == 0 { micros.max(1) } else { ((u64::from(old) * 3 + u64::from(micros)) / 4).max(1) as u32 };
        slot.store(new, Ordering::Relaxed);
    }

    /// Runs the read `f`, of this `shape`: where it is when the shape is
    /// short, on the blocking pool when it is slow.
    pub(crate) async fn read<T: Send + 'static>(
        self: &std::sync::Arc<Self>,
        shape: u64,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T> {
        if self.slow(shape) {
            let costs = self.clone();
            return tokio::task::spawn_blocking(move || {
                let started = Instant::now();
                let out = f();
                costs.record(shape, started.elapsed());
                out
            })
            .await
            .map_err(|e| Error::Pool(e.to_string()));
        }
        let started = Instant::now();
        let out = f();
        self.record(shape, started.elapsed());
        Ok(out)
    }
}

fn expr_shape(e: &Expr, h: &mut impl Hasher) {
    std::mem::discriminant(e).hash(h);
    match e {
        Expr::Column(c) => {
            c.table.hash(h);
            c.name.hash(h);
        }
        Expr::Binary { op, left, right } => {
            std::mem::discriminant(op).hash(h);
            expr_shape(left, h);
            expr_shape(right, h);
        }
        Expr::Not(inner) => expr_shape(inner, h),
        Expr::IsNull { expr, .. } => expr_shape(expr, h),
        Expr::InList { expr, list, .. } => {
            expr_shape(expr, h);
            list.len().min(16).hash(h);
        }
        _ => {}
    }
}

/// The shape of a read: its table and its clauses, not its values. `None`
/// for a statement that writes (or raw SQL, which has its own path).
pub(crate) fn shape(query: &OwnedQuery) -> Option<u64> {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match query {
        OwnedQuery::FindByKey(table, _) => {
            1u8.hash(&mut h);
            table.name.hash(&mut h);
        }
        OwnedQuery::Select(s) => {
            2u8.hash(&mut h);
            s.from.name.hash(&mut h);
            (s.joins.len(), s.group_by.len(), s.order.len(), s.limit.is_some(), s.distinct).hash(&mut h);
            if let Some(filter) = &s.filter {
                expr_shape(filter, &mut h);
            }
        }
        _ => return None,
    }
    Some(h.finish())
}

/// The shape of SQL statements: their text, not their parameters.
pub(crate) fn sql_shape<'a>(texts: impl Iterator<Item = &'a str>) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    3u8.hash(&mut h);
    for text in texts {
        text.hash(&mut h);
    }
    h.finish()
}

thread_local! {
    /// The compressor of this thread: making one allocates and clears a few
    /// hundred kilobytes, more work than compressing a page of rows.
    static DEFLATE: RefCell<flate2::Compress> =
        RefCell::new(flate2::Compress::new(flate2::Compression::fast(), false));
}

/// `data` as gzip.
pub(crate) fn gzip(data: &[u8]) -> Option<Vec<u8>> {
    DEFLATE.with(|deflate| {
        let mut deflate = deflate.try_borrow_mut().ok()?;
        deflate.reset();
        let mut out = Vec::with_capacity(data.len() / 3 + 64);
        // The header: gzip, deflate, no name, no date, an unknown system.
        out.extend_from_slice(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff]);
        loop {
            let taken = deflate.total_in() as usize;
            match deflate.compress_vec(&data[taken..], &mut out, flate2::FlushCompress::Finish).ok()? {
                flate2::Status::StreamEnd => break,
                // The output is full: more room, and on.
                _ => out.reserve(out.capacity().max(4096)),
            }
        }
        let mut crc = flate2::Crc::new();
        crc.update(data);
        out.extend_from_slice(&crc.sum().to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        Some(out)
    })
}

/// A value of `/v1/sql`, as the text of [`crate::sql::value_to_json`].
fn write_sql_value(out: &mut Vec<u8>, v: &Value) {
    use std::io::Write as _;
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::I32(i) => {
            let _ = write!(out, "{i}");
        }
        Value::I64(i) => {
            let _ = write!(out, "{i}");
        }
        Value::Text(s) => {
            let _ = serde_json::to_writer(&mut *out, s.as_str());
        }
        // The rarer kinds: as the JSON they are, then its text.
        other => {
            let _ = serde_json::to_writer(&mut *out, &crate::sql::value_to_json(other));
        }
    }
}

/// The answer of one statement of `/v1/sql`, written straight as its text:
/// what serializing its JSON gives, without building it (no map per row, no
/// column name copied per value). `false` when it cannot be (two columns of
/// the same name: an object keeps the last one), and `out` is left as it was.
pub(crate) fn write_sql_result(out: &mut Vec<u8>, r: &SqlResult, arrays: bool) -> bool {
    use std::io::Write as _;
    if r.returns_rows && !arrays {
        let mut names: Vec<&str> = r.columns.iter().map(String::as_str).collect();
        names.sort_unstable();
        if names.windows(2).any(|w| w[0] == w[1]) || r.rows.iter().any(|row| row.len() != r.columns.len()) {
            return false;
        }
    }
    out.push(b'{');
    let mut first = true;
    let mut field = |out: &mut Vec<u8>, name: &str| {
        if !std::mem::take(&mut first) {
            out.push(b',');
        }
        let _ = write!(out, "\"{name}\":");
    };
    if r.returns_rows {
        field(out, "columns");
        let _ = serde_json::to_writer(&mut *out, &r.columns);
        field(out, "rows");
        // The names, quoted and escaped once.
        let keys: Vec<Vec<u8>> = if arrays {
            Vec::new()
        } else {
            r.columns.iter().map(|c| serde_json::to_vec(c).unwrap_or_default()).collect()
        };
        out.push(b'[');
        for (i, row) in r.rows.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.push(if arrays { b'[' } else { b'{' });
            for (j, value) in row.iter().enumerate() {
                if j > 0 {
                    out.push(b',');
                }
                if !arrays {
                    out.extend_from_slice(&keys[j]);
                    out.push(b':');
                }
                write_sql_value(out, value);
            }
            out.push(if arrays { b']' } else { b'}' });
        }
        out.push(b']');
    }
    if !r.returns_rows || r.rows_affected > 0 {
        field(out, "rows_affected");
        let _ = write!(out, "{}", r.rows_affected);
        field(out, "last_insert_id");
        match &r.last_insert_id {
            Some(v) => write_sql_value(out, v),
            None => out.extend_from_slice(b"null"),
        }
    }
    out.push(b'}');
    true
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;

    #[test]
    fn gzip_is_gzip() {
        let texts: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"a".to_vec(),
            br#"{"rows":[[1,"deux"],[3,"quatre"]]}"#.repeat(200),
            // Nothing to gain: the output outgrows its first guess.
            (0..300_000u32).flat_map(|i| i.wrapping_mul(2_654_435_761).to_le_bytes()).collect(),
        ];
        // Twice: the compressor of the thread is reused.
        for text in texts.iter().chain(texts.iter()) {
            let packed = gzip(text).unwrap();
            let mut back = Vec::new();
            flate2::read::GzDecoder::new(&packed[..]).read_to_end(&mut back).unwrap();
            assert_eq!(&back, text);
        }
        assert!(gzip(&texts[2]).unwrap().len() < texts[2].len() / 10);
    }

    #[test]
    fn slow_shapes_leave_and_come_back() {
        let costs = ReadCosts::new();
        assert!(!costs.slow(7));
        for _ in 0..10 {
            costs.record(7, Duration::from_micros(20));
        }
        assert!(!costs.slow(7));
        // One slow read (a cold page): a few on the pool, then back.
        costs.record(7, Duration::from_millis(5));
        assert!(costs.slow(7));
        assert!(!costs.slow(8), "another shape is not touched");
        let mut on_the_pool = 0;
        while costs.slow(7) {
            costs.record(7, Duration::from_micros(20));
            on_the_pool += 1;
        }
        assert!((2..10).contains(&on_the_pool), "{on_the_pool}");
        // A shape that stays slow stays on the pool.
        for _ in 0..10 {
            costs.record(9, Duration::from_millis(3));
        }
        assert!(costs.slow(9));
    }

    #[test]
    fn sql_answers_written_straight() {
        let json = |r: &SqlResult, arrays: bool| -> serde_json::Value {
            let mut out = Vec::new();
            assert!(write_sql_result(&mut out, r, arrays));
            serde_json::from_slice(&out).unwrap()
        };
        let rows = SqlResult {
            columns: vec!["id".into(), "a \"name\"".into(), "x".into()],
            rows: vec![
                vec![Value::I64(1), Value::Text("é \"q\" \n".into()), Value::Null],
                vec![Value::I32(-2), Value::Bool(true), Value::F64(1.5)],
                vec![Value::I64(i64::MIN), Value::Bytes(vec![1, 255]), Value::Json(serde_json::json!({"k": [1]}))],
            ],
            returns_rows: true,
            ..SqlResult::default()
        };
        for arrays in [false, true] {
            assert_eq!(json(&rows, arrays), crate::server::sql_json(&rows, arrays), "arrays: {arrays}");
        }
        let written = SqlResult { rows_affected: 3, last_insert_id: Some(Value::I64(9)), ..SqlResult::default() };
        assert_eq!(json(&written, false), crate::server::sql_json(&written, false));
        let returning = SqlResult {
            columns: vec!["id".into()],
            rows: vec![vec![Value::I64(4)]],
            rows_affected: 1,
            returns_rows: true,
            ..SqlResult::default()
        };
        assert_eq!(json(&returning, false), crate::server::sql_json(&returning, false));
        let empty = SqlResult { columns: vec!["id".into()], returns_rows: true, ..SqlResult::default() };
        assert_eq!(json(&empty, false), crate::server::sql_json(&empty, false));
        // Two columns of one name: left to the JSON map, which keeps the last.
        let twice = SqlResult {
            columns: vec!["n".into(), "n".into()],
            rows: vec![vec![Value::I64(1), Value::I64(2)]],
            returns_rows: true,
            ..SqlResult::default()
        };
        let mut out = b"before".to_vec();
        assert!(!write_sql_result(&mut out, &twice, false));
        assert_eq!(out, b"before");
        assert!(write_sql_result(&mut out, &twice, true), "as arrays, nothing is lost");
    }
}
