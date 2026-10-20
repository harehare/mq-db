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

    /// A total order for `ORDER BY`: `NULL`, then booleans, then numbers,
    /// then strings. `NULL` sorts lowest, so it comes last under `DESC`.
    pub(super) fn sort_cmp(&self, other: &Value) -> std::cmp::Ordering {
        fn rank(v: &Value) -> u8 {
            match v {
                Value::Null => 0,
                Value::Bool(_) => 1,
                Value::Int(_) | Value::Float(_) => 2,
                Value::Str(_) => 3,
            }
        }
        match (self, other) {
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Int(a), Value::Float(b)) => (*a as f64).total_cmp(b),
            (Value::Float(a), Value::Int(b)) => a.total_cmp(&(*b as f64)),
            (Value::Float(a), Value::Float(b)) => a.total_cmp(b),
            (Value::Str(a), Value::Str(b)) => a.cmp(b),
            _ => rank(self).cmp(&rank(other)),
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
