//! Row-level join helpers.

use super::expr::eval_expr;
use super::output::QueryOutput;
use super::rows::qualify_row;
use super::value::{JoinKey, Row, Value, value_join_key};
use rustc_hash::FxHashMap;
use sqlparser::ast::Expr;
use std::cell::RefCell;
use std::sync::Arc;

pub(super) fn parse_display_value(s: &str) -> Value {
    if let Ok(n) = s.parse::<i64>() {
        Value::Int(n)
    } else if let Ok(f) = s.parse::<f64>() {
        Value::Float(f)
    } else {
        Value::Str(s.to_string())
    }
}

pub(super) fn output_to_rows(out: &QueryOutput, prefix: &str) -> Vec<Row> {
    out.rows
        .iter()
        .map(|r| {
            qualify_row(
                Row {
                    columns: out.columns.clone().into(),
                    values: r.iter().map(|v| parse_display_value(v)).collect(),
                },
                prefix,
            )
        })
        .collect()
}

type Columns = Arc<[String]>;

thread_local! {
    /// Last `(left, right, combined)` column sets. Join loops combine the same
    /// pair of column sets for every row, so this saves rebuilding the names.
    /// It owns the `Arc`s, so a cached pointer can never be reused by a
    /// different allocation.
    static LAST_COMBINED: RefCell<Option<(Columns, Columns, Columns)>> = const { RefCell::new(None) };
}

fn combined_columns(l: &Columns, r: &Columns) -> Columns {
    LAST_COMBINED.with(|cell| {
        if let Some((cl, cr, out)) = cell.borrow().as_ref()
            && Arc::ptr_eq(cl, l)
            && Arc::ptr_eq(cr, r)
        {
            return out.clone();
        }
        let out: Columns = l.iter().chain(r.iter()).cloned().collect();
        *cell.borrow_mut() = Some((l.clone(), r.clone(), out.clone()));
        out
    })
}

pub(super) fn combine_rows(l: &Row, r: &Row) -> Row {
    let mut vals = Vec::with_capacity(l.values.len() + r.values.len());
    vals.extend(l.values.iter().cloned());
    vals.extend(r.values.iter().cloned());
    Row {
        columns: combined_columns(&l.columns, &r.columns),
        values: vals,
    }
}

/// Extends a left row with NULLs for every right-hand column, for a `LEFT
/// JOIN` left row that found no matching partner.
pub(super) fn null_extend_row(l: &Row, right_cols: &[String]) -> Row {
    let mut vals = l.values.clone();
    vals.extend(right_cols.iter().map(|_| Value::Null));
    Row {
        columns: l.columns.iter().chain(right_cols).cloned().collect(),
        values: vals,
    }
}

pub(super) fn cross_join(left: Vec<Row>, right: Vec<Row>) -> Vec<Row> {
    let mut out = Vec::with_capacity(left.len() * right.len());
    for l in &left {
        for r in &right {
            out.push(combine_rows(l, r));
        }
    }
    out
}

/// Equi-join fast path: hashes `right` by `right_key_expr` and probes it with
/// `left_key_expr` per left row instead of the full `left * right` cross
/// product. `full_predicate` is still checked per candidate pair, so results
/// match `cross_join` + `.retain(full_predicate)` exactly. When `outer` is
/// set (a `LEFT JOIN`), a left row with no matching (or no truthy-predicate)
/// right row is still emitted once, NULL-extended over `right_cols`.
pub(super) fn hash_equi_join(
    left: Vec<Row>,
    right: Vec<Row>,
    left_key_expr: &Expr,
    right_key_expr: &Expr,
    full_predicate: &Expr,
    outer: bool,
    right_cols: &[String],
) -> Vec<Row> {
    let mut buckets: FxHashMap<JoinKey, Vec<usize>> = FxHashMap::default();
    for (i, r) in right.iter().enumerate() {
        if let Some(key) = value_join_key(&eval_expr(right_key_expr, r)) {
            buckets.entry(key).or_default().push(i);
        }
    }

    let mut out = Vec::new();
    for l in &left {
        let mut matched = false;
        if let Some(key) = value_join_key(&eval_expr(left_key_expr, l))
            && let Some(candidates) = buckets.get(&key)
        {
            for &i in candidates {
                let combined = combine_rows(l, &right[i]);
                if eval_expr(full_predicate, &combined).is_truthy() {
                    matched = true;
                    out.push(combined);
                }
            }
        }
        if outer && !matched {
            out.push(null_extend_row(l, right_cols));
        }
    }
    out
}
