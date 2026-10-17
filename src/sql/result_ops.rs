//! `ORDER BY`, `LIMIT`/`OFFSET` and set operations on result rows.

use super::expr::eval_expr;
use super::output::QueryOutput;
use super::value::{Row, Value};
use crate::MqdbError;
use rustc_hash::FxHashMap;
use sqlparser::ast::{
    Expr, LimitClause, OrderByExpr, OrderBySort, Query, SetOperator, SetQuantifier,
};

pub(super) fn expr_structurally_eq(a: &Expr, b: &Expr) -> bool {
    a == b
}

pub(super) fn apply_order_by_exprs(rows: &mut [(Row, Vec<String>)], exprs: &[OrderByExpr]) {
    rows.sort_by(|(ra, _), (rb, _)| {
        for ob in exprs {
            let va = eval_expr(&ob.expr, ra);
            let vb = eval_expr(&ob.expr, rb);
            let ord = va.cmp_val(&vb).unwrap_or(std::cmp::Ordering::Equal);
            // sort=None or Asc → ascending; Desc → descending
            let ord = if ob.options.sort == Some(OrderBySort::Desc) {
                ord.reverse()
            } else {
                ord
            };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
}

fn multiset_counts(rows: &[Vec<String>]) -> FxHashMap<Vec<String>, usize> {
    let mut counts = FxHashMap::default();
    for r in rows {
        *counts.entry(r.clone()).or_insert(0) += 1;
    }
    counts
}

fn dedup_rows(mut rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
    let mut seen = std::collections::HashSet::new();
    rows.retain(|r| seen.insert(r.clone()));
    rows
}

pub(super) fn combine_set_operation(
    left: QueryOutput,
    right: QueryOutput,
    op: &SetOperator,
    quantifier: &SetQuantifier,
) -> Result<QueryOutput, MqdbError> {
    let n_left = left.rows.first().map(|r| r.len());
    let n_right = right.rows.first().map(|r| r.len());
    if let (Some(a), Some(b)) = (n_left, n_right)
        && a != b
    {
        return Err(MqdbError::SqlExec(format!(
            "set operation: left side has {a} column(s), right side has {b}"
        )));
    }
    let columns = if !left.columns.is_empty() {
        left.columns
    } else {
        right.columns
    };
    let all = matches!(quantifier, SetQuantifier::All);
    let rows = match (op, all) {
        (SetOperator::Union, true) => {
            let mut combined = left.rows;
            combined.extend(right.rows);
            combined
        }
        (SetOperator::Union, false) => {
            let mut combined = left.rows;
            combined.extend(right.rows);
            dedup_rows(combined)
        }
        (SetOperator::Intersect, true) => {
            let mut right_counts = multiset_counts(&right.rows);
            left.rows
                .into_iter()
                .filter(|r| match right_counts.get_mut(r) {
                    Some(c) if *c > 0 => {
                        *c -= 1;
                        true
                    }
                    _ => false,
                })
                .collect()
        }
        (SetOperator::Intersect, false) => {
            let right_set: std::collections::HashSet<_> = right.rows.into_iter().collect();
            dedup_rows(
                left.rows
                    .into_iter()
                    .filter(|r| right_set.contains(r))
                    .collect(),
            )
        }
        (SetOperator::Except | SetOperator::Minus, true) => {
            let mut right_counts = multiset_counts(&right.rows);
            left.rows
                .into_iter()
                .filter(|r| match right_counts.get_mut(r) {
                    Some(c) if *c > 0 => {
                        *c -= 1;
                        false
                    }
                    _ => true,
                })
                .collect()
        }
        (SetOperator::Except | SetOperator::Minus, false) => {
            let right_set: std::collections::HashSet<_> = right.rows.into_iter().collect();
            dedup_rows(
                left.rows
                    .into_iter()
                    .filter(|r| !right_set.contains(r))
                    .collect(),
            )
        }
    };
    Ok(QueryOutput { columns, rows })
}

pub(super) fn apply_limit_offset(
    mut rows: Vec<Vec<String>>,
    limit: Option<&Expr>,
    offset: Option<&Expr>,
) -> Vec<Vec<String>> {
    let dummy = Row {
        columns: vec![],
        values: vec![],
    };
    if let Some(off) = offset
        && let Value::Int(n) = eval_expr(off, &dummy)
    {
        rows.drain(..(n as usize).min(rows.len()));
    }
    if let Some(lim) = limit
        && let Value::Int(n) = eval_expr(lim, &dummy)
    {
        rows.truncate(n as usize);
    }
    rows
}

/// Strips redundant `(...)` wrappers so callers can pattern-match the inner
/// expression directly.
pub(super) fn unwrap_nested(expr: &Expr) -> &Expr {
    match expr {
        Expr::Nested(inner) => unwrap_nested(inner),
        other => other,
    }
}

pub(super) fn limit_expr_of(query: &Query) -> Option<Expr> {
    query.limit_clause.as_ref().and_then(|lc| match lc {
        LimitClause::LimitOffset { limit, .. } => limit.clone(),
        LimitClause::OffsetCommaLimit { limit, .. } => Some(limit.clone()),
    })
}

pub(super) fn offset_expr_of(query: &Query) -> Option<Expr> {
    query.limit_clause.as_ref().and_then(|lc| match lc {
        LimitClause::LimitOffset { offset, .. } => offset.as_ref().map(|o| o.value.clone()),
        LimitClause::OffsetCommaLimit { offset, .. } => Some(offset.clone()),
    })
}
