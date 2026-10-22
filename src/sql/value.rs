//! Scalar values and result rows.

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

impl Value {
    pub(super) fn as_str(&self) -> Option<&str> {
        if let Value::Str(s) = self {
            Some(s)
        } else {
            None
        }
    }
    pub(super) fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            Value::Float(f) => Some(*f as i64),
            _ => None,
        }
    }
    pub(super) fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            Value::Int(n) => Some(*n as f64),
            _ => None,
        }
    }
    pub(super) fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Int(n) => *n != 0,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Null => false,
        }
    }
    pub(super) fn display(&self) -> String {
        match self {
            Value::Str(s) => s.clone(),
            Value::Int(n) => n.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Null => "NULL".to_string(),
        }
    }
    /// SQL `=`: like `==`, but an integer also equals a string holding that
    /// integer's canonical form (`1` and `'1'`, not `'01'`). Custom table
    /// cells are stored as text, so this lets them join against integers.
    pub(super) fn sql_eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Int(i), Value::Str(s)) | (Value::Str(s), Value::Int(i)) => {
                canonical_int(s) == Some(*i)
            }
            _ => self == other,
        }
    }

    /// A total order for `ORDER BY`: `NULL`, then booleans, then numbers
    /// (integers, floats and canonical integer strings, by exact numeric
    /// value; `NaN` last), then other strings. `NULL` sorts lowest, so it
    /// comes last under `DESC`.
    pub(super) fn sort_cmp(&self, other: &Value) -> std::cmp::Ordering {
        fn rank(v: &Value) -> u8 {
            match v {
                Value::Null => 0,
                Value::Bool(_) => 1,
                _ if sort_number(v).is_some() => 2,
                _ => 3,
            }
        }
        match (self, other) {
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Str(a), Value::Str(b)) if rank(self) == 3 && rank(other) == 3 => a.cmp(b),
            _ => match (sort_number(self), sort_number(other)) {
                (Some(a), Some(b)) => cmp_num(a, b),
                _ => rank(self).cmp(&rank(other)),
            },
        }
    }

    pub(super) fn cmp_val(&self, other: &Value) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Value::Int(a), Value::Str(s)) => canonical_int(s).map(|b| a.cmp(&b)),
            (Value::Str(s), Value::Int(b)) => canonical_int(s).map(|a| a.cmp(b)),
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
            (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
            (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
            (Value::Null, Value::Null) => Some(std::cmp::Ordering::Equal),
            _ => None,
        }
    }
}

/// A numeric `ORDER BY` key.
#[derive(Clone, Copy)]
enum SortNum {
    Int(i64),
    Float(f64),
}

fn sort_number(v: &Value) -> Option<SortNum> {
    match v {
        Value::Int(i) => Some(SortNum::Int(*i)),
        Value::Float(f) => Some(SortNum::Float(*f)),
        Value::Str(s) => canonical_int(s).map(SortNum::Int),
        _ => None,
    }
}

/// Exact numeric comparison, with `NaN` above everything (including infinity).
fn cmp_num(a: SortNum, b: SortNum) -> std::cmp::Ordering {
    match (a, b) {
        (SortNum::Int(a), SortNum::Int(b)) => a.cmp(&b),
        (SortNum::Int(a), SortNum::Float(b)) => cmp_i64_f64(a, b),
        (SortNum::Float(a), SortNum::Int(b)) => cmp_i64_f64(b, a).reverse(),
        (SortNum::Float(a), SortNum::Float(b)) => match (a.is_nan(), b.is_nan()) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
        },
    }
}

fn cmp_i64_f64(i: i64, f: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    if f.is_nan() || f >= TWO_POW_63 {
        return Less;
    }
    if f < -TWO_POW_63 {
        return Greater;
    }
    // In range, so the truncation is exact.
    let t = f.trunc();
    match i.cmp(&(t as i64)) {
        Equal => 0.0f64.partial_cmp(&(f - t)).unwrap_or(Equal),
        o => o,
    }
}

/// The integer `s` spells in canonical form (no sign prefix, padding or spaces).
fn canonical_int(s: &str) -> Option<i64> {
    s.parse::<i64>().ok().filter(|n| n.to_string() == s)
}

/// Hashable projection of [`Value`], mirroring [`Value::sql_eq`] (a canonical
/// integer string keys like the integer, `NULL` equals `NULL`, `NaN` matches
/// nothing).
#[derive(PartialEq, Eq, Hash)]
pub(super) enum JoinKey {
    Str(String),
    Int(i64),
    Bool(bool),
    FloatBits(u64),
    Null,
}

pub(super) fn value_join_key(v: &Value) -> Option<JoinKey> {
    match v {
        Value::Str(s) => Some(match canonical_int(s) {
            Some(i) => JoinKey::Int(i),
            None => JoinKey::Str(s.clone()),
        }),
        Value::Int(i) => Some(JoinKey::Int(*i)),
        Value::Bool(b) => Some(JoinKey::Bool(*b)),
        Value::Null => Some(JoinKey::Null),
        Value::Float(f) if f.is_nan() => None, // NaN matches nothing
        Value::Float(f) => {
            let normalized = if *f == 0.0 { 0.0 } else { *f };
            Some(JoinKey::FloatBits(normalized.to_bits()))
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct Row {
    pub(super) columns: Vec<String>,
    pub(super) values: Vec<Value>,
}

impl Row {
    pub(super) fn get(&self, col: &str) -> Option<&Value> {
        if let Some(i) = self
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(col))
        {
            return self.values.get(i);
        }
        let short = col.rsplit('.').next().unwrap_or(col);
        self.columns
            .iter()
            .position(|c| {
                c.eq_ignore_ascii_case(col)
                    || c.rsplit('.')
                        .next()
                        .unwrap_or(c)
                        .eq_ignore_ascii_case(short)
            })
            .and_then(|i| self.values.get(i))
    }
}

#[cfg(test)]
mod tests {
    use super::Value;
    use std::cmp::Ordering::*;

    fn s(x: &str) -> Value {
        Value::Str(x.to_string())
    }

    #[test]
    fn sort_cmp_orders_integers_and_canonical_strings_numerically() {
        assert_eq!(Value::Int(10).sort_cmp(&s("2")), Greater);
        assert_eq!(s("2").sort_cmp(&Value::Int(10)), Less);
        assert_eq!(Value::Int(2).sort_cmp(&s("2")), Equal);
        assert_eq!(Value::Float(2.5).sort_cmp(&s("10")), Less);
        // Non-canonical and non-numeric strings stay above all numbers.
        assert_eq!(Value::Int(10).sort_cmp(&s("02")), Less);
        assert_eq!(s("02").sort_cmp(&s("abc")), Less);
    }

    #[test]
    fn sort_cmp_is_exact_beyond_f64_precision() {
        let a = Value::Int(9_007_199_254_740_992);
        let b = Value::Int(9_007_199_254_740_993);
        let f = Value::Float(9_007_199_254_740_992.0);
        assert_eq!(a.sort_cmp(&f), Equal);
        assert_eq!(b.sort_cmp(&f), Greater);
        assert_eq!(f.sort_cmp(&b), Less);
        assert_eq!(a.sort_cmp(&b), Less);
        assert_eq!(
            Value::Int(i64::MAX).sort_cmp(&Value::Float(2f64.powi(63))),
            Less
        );
        assert_eq!(
            Value::Int(i64::MIN).sort_cmp(&Value::Float(-(2f64.powi(63)))),
            Equal
        );
        assert_eq!(Value::Int(0).sort_cmp(&Value::Float(0.5)), Less);
        assert_eq!(Value::Int(-1).sort_cmp(&Value::Float(-0.5)), Less);
    }

    #[test]
    fn sort_cmp_handles_nan_and_infinities() {
        let nan = Value::Float(f64::NAN);
        let inf = Value::Float(f64::INFINITY);
        assert_eq!(nan.sort_cmp(&inf), Greater);
        assert_eq!(nan.sort_cmp(&Value::Int(i64::MAX)), Greater);
        assert_eq!(Value::Int(i64::MAX).sort_cmp(&inf), Less);
        assert_eq!(
            Value::Int(i64::MIN).sort_cmp(&Value::Float(f64::NEG_INFINITY)),
            Greater
        );
        assert_eq!(nan.sort_cmp(&Value::Float(f64::NAN)), Equal);
        assert_eq!(nan.sort_cmp(&s("x")), Less);
    }

    #[test]
    fn sort_cmp_is_transitive_over_mixed_values() {
        let vals = vec![
            Value::Null,
            Value::Bool(false),
            Value::Bool(true),
            Value::Int(-3),
            Value::Int(2),
            Value::Int(10),
            Value::Int(9_007_199_254_740_992),
            Value::Int(9_007_199_254_740_993),
            Value::Int(i64::MAX),
            Value::Int(i64::MIN),
            Value::Float(-0.0),
            Value::Float(2.0),
            Value::Float(2.5),
            Value::Float(9_007_199_254_740_992.0),
            Value::Float(f64::INFINITY),
            Value::Float(f64::NEG_INFINITY),
            Value::Float(f64::NAN),
            s("2"),
            s("10"),
            s("02"),
            s("abc"),
            s(""),
        ];
        for a in &vals {
            for b in &vals {
                assert_eq!(a.sort_cmp(b), b.sort_cmp(a).reverse(), "{a:?} {b:?}");
                for c in &vals {
                    if a.sort_cmp(b).is_le() && b.sort_cmp(c).is_le() {
                        assert!(a.sort_cmp(c).is_le(), "{a:?} {b:?} {c:?}");
                    }
                }
            }
        }
    }
}
