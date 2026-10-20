//! Projection, `GROUP BY`, aggregates and `HAVING`.

use super::analysis::{validate_expr_columns, value_to_expr};
use super::engine::SqlEngine;
use super::expr::eval_expr;
use super::output::QueryOutput;
use super::result_ops::{apply_limit_offset, apply_order_by_exprs, expr_structurally_eq};
use super::rows::{eval_sql_value, ident_value};
use super::value::{Row, Value};
use crate::MqdbError;
use rustc_hash::FxHashMap;
use sqlparser::ast::{
    DuplicateTreatment, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments,
    GroupByExpr, OrderByExpr, OrderByKind, OrderBySort, Select, SelectItem,
};

fn projection_columns(projection: &[SelectItem], first_row: Option<&Row>) -> Vec<String> {
    if projection.len() == 1 && matches!(projection[0], SelectItem::Wildcard(_)) {
        return first_row
            .map(|r| {
                r.columns
                    .iter()
                    .map(|c| c.split('.').next_back().unwrap_or(c).to_string())
                    .collect()
            })
            .unwrap_or_default();
    }
    projection
        .iter()
        .enumerate()
        .map(|(i, item)| projection_col_name(item, i))
        .collect()
}

fn projection_col_name(item: &SelectItem, idx: usize) -> String {
    match item {
        SelectItem::UnnamedExpr(Expr::Identifier(i)) => i.value.clone(),
        SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) => parts
            .last()
            .map(|i| i.value.as_str())
            .unwrap_or("")
            .to_string(),
        SelectItem::UnnamedExpr(Expr::Function(f)) => {
            f.name.0.last().map(ident_value).unwrap_or("").to_string()
        }
        SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
        SelectItem::Wildcard(_) => "*".to_string(),
        _ => format!("col{}", idx),
    }
}

fn project_row(projection: &[SelectItem], row: &Row) -> Vec<String> {
    if projection.len() == 1 && matches!(projection[0], SelectItem::Wildcard(_)) {
        return row.values.iter().map(|v| v.display()).collect();
    }
    projection
        .iter()
        .map(|item| match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                eval_expr(e, row).display()
            }
            SelectItem::ExprWithAliases { expr: e, .. } => eval_expr(e, row).display(),
            SelectItem::Wildcard(_) => row
                .values
                .iter()
                .map(|v| v.display())
                .collect::<Vec<_>>()
                .join(","),
            SelectItem::QualifiedWildcard(kind, _) => {
                let prefix = match kind {
                    sqlparser::ast::SelectItemQualifiedWildcardKind::ObjectName(name) => {
                        name.0.last().map(ident_value).unwrap_or("").to_string()
                    }
                    _ => String::new(),
                };
                row.columns
                    .iter()
                    .zip(row.values.iter())
                    .filter(|(c, _)| c.starts_with(&format!("{}.", prefix)))
                    .map(|(_, v)| v.display())
                    .collect::<Vec<_>>()
                    .join(",")
            }
        })
        .collect()
}

fn has_aggregate(projection: &[SelectItem]) -> bool {
    projection.iter().any(|item| match item {
        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => is_agg_expr(e),
        _ => false,
    })
}

fn is_agg_expr(expr: &Expr) -> bool {
    matches!(expr, Expr::Function(f) if {
        let name = f.name.0.last().map(ident_value).unwrap_or("").to_lowercase();
        is_aggregate_name(&name)
    })
}

pub(super) fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name,
        "count" | "sum" | "min" | "max" | "avg" | "group_concat" | "string_agg"
    )
}

/// Rejects non-aggregate columns not covered by GROUP BY (PostgreSQL-style), instead of silently picking a row.
fn validate_agg_projection(
    projection: &[SelectItem],
    group_by_exprs: &[Expr],
) -> Result<(), MqdbError> {
    for item in projection {
        let expr = match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
            _ => continue,
        };
        if is_agg_expr(expr) || matches!(expr, Expr::Value(_)) {
            continue;
        }
        if group_by_exprs.iter().any(|g| expr_structurally_eq(g, expr)) {
            continue;
        }
        return Err(MqdbError::SqlExec(format!(
            "column \"{expr}\" must appear in the GROUP BY clause or be used in an aggregate function"
        )));
    }
    Ok(())
}

fn eval_agg_row(
    projection: &[SelectItem],
    group_by_exprs: &[Expr],
    key_vals: &[Value],
    group_rows: &[&Row],
) -> Vec<String> {
    projection
        .iter()
        .map(|item| {
            let expr = match item {
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                _ => return String::new(),
            };
            match expr {
                Expr::Function(f) => eval_aggregate(f, group_rows).display(),
                other => {
                    if let Some(ki) = group_by_exprs
                        .iter()
                        .position(|e| expr_structurally_eq(e, other))
                    {
                        key_vals.get(ki).map(|v| v.display()).unwrap_or_default()
                    } else {
                        group_rows
                            .first()
                            .map(|r| eval_expr(other, r).display())
                            .unwrap_or_default()
                    }
                }
            }
        })
        .collect()
}

fn agg_arg(f: &Function, row: &Row) -> Value {
    match &f.args {
        FunctionArguments::List(al) => al.args.iter().find_map(|a| match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(eval_expr(e, row)),
            FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => Some(Value::Int(1)),
            _ => None,
        }),
        _ => None,
    }
    .unwrap_or(Value::Null)
}

fn eval_aggregate(f: &Function, group_rows: &[&Row]) -> Value {
    let name = f
        .name
        .0
        .last()
        .map(ident_value)
        .unwrap_or("")
        .to_lowercase();
    match name.as_str() {
        "count" if is_distinct(f) => {
            let mut seen: Vec<Value> = Vec::new();
            for r in group_rows {
                let v = agg_arg(f, r);
                if !matches!(v, Value::Null) && !seen.contains(&v) {
                    seen.push(v);
                }
            }
            Value::Int(seen.len() as i64)
        }
        "count" => Value::Int(group_rows.len() as i64),
        "group_concat" | "string_agg" => {
            let sep = agg_separator(f);
            Value::Str(
                group_rows
                    .iter()
                    .map(|r| agg_arg(f, r))
                    .filter(|v| !matches!(v, Value::Null))
                    .map(|v| v.display())
                    .collect::<Vec<_>>()
                    .join(&sep),
            )
        }
        "sum" => Value::Float(
            group_rows
                .iter()
                .filter_map(|r| agg_arg(f, r).as_f64())
                .sum(),
        ),
        "min" => group_rows
            .iter()
            .map(|r| agg_arg(f, r))
            .min_by(|a, b| a.cmp_val(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(Value::Null),
        "max" => group_rows
            .iter()
            .map(|r| agg_arg(f, r))
            .max_by(|a, b| a.cmp_val(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(Value::Null),
        "avg" => {
            let vals: Vec<f64> = group_rows
                .iter()
                .filter_map(|r| agg_arg(f, r).as_f64())
                .collect();
            if vals.is_empty() {
                Value::Null
            } else {
                Value::Float(vals.iter().sum::<f64>() / vals.len() as f64)
            }
        }
        _ => Value::Null,
    }
}

fn substitute_having_expr(
    expr: &Expr,
    group_by_exprs: &[Expr],
    key_vals: &[Value],
    group_rows: &[&Row],
) -> Expr {
    match expr {
        Expr::Function(f) if is_aggregate_name(&func_name(f)) => {
            value_to_expr(&eval_aggregate(f, group_rows))
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(substitute_having_expr(
                left,
                group_by_exprs,
                key_vals,
                group_rows,
            )),
            op: op.clone(),
            right: Box::new(substitute_having_expr(
                right,
                group_by_exprs,
                key_vals,
                group_rows,
            )),
        },
        Expr::Nested(inner) => Expr::Nested(Box::new(substitute_having_expr(
            inner,
            group_by_exprs,
            key_vals,
            group_rows,
        ))),
        Expr::UnaryOp { op, expr: inner } => Expr::UnaryOp {
            op: *op,
            expr: Box::new(substitute_having_expr(
                inner,
                group_by_exprs,
                key_vals,
                group_rows,
            )),
        },
        other => {
            if let Some(ki) = group_by_exprs
                .iter()
                .position(|g| expr_structurally_eq(g, other))
            {
                value_to_expr(key_vals.get(ki).unwrap_or(&Value::Null))
            } else {
                other.clone()
            }
        }
    }
}

fn eval_having(
    having: &Expr,
    _projection: &[SelectItem],
    group_by_exprs: &[Expr],
    key_vals: &[Value],
    group_rows: &[&Row],
) -> bool {
    eval_agg_expr_value(having, group_by_exprs, key_vals, group_rows).is_truthy()
}

/// Evaluates an arbitrary expression (an `ORDER BY` key, in practice) in the
/// same per-group substitution context as `HAVING`: aggregate calls resolve
/// against `group_rows`, and any sub-expression structurally equal to one of
/// `group_by_exprs` resolves to its key value.
fn eval_agg_expr_value(
    expr: &Expr,
    group_by_exprs: &[Expr],
    key_vals: &[Value],
    group_rows: &[&Row],
) -> Value {
    let substituted = substitute_having_expr(expr, group_by_exprs, key_vals, group_rows);
    let dummy = Row {
        columns: vec![],
        values: vec![],
    };
    eval_expr(&substituted, &dummy)
}

/// An `ORDER BY <name>` after `GROUP BY` may refer to a `SELECT ... AS
/// <name>` alias rather than an input column; resolve that one level so
/// `eval_agg_expr_value` sees the aliased expression itself.
fn resolve_order_expr_alias(expr: &Expr, projection: &[SelectItem]) -> Expr {
    if let Expr::Identifier(id) = expr {
        for item in projection {
            if let SelectItem::ExprWithAlias { expr: inner, alias } = item
                && alias.value.eq_ignore_ascii_case(&id.value)
            {
                return inner.clone();
            }
        }
    }
    expr.clone()
}

fn func_name(f: &Function) -> String {
    f.name
        .0
        .last()
        .map(ident_value)
        .unwrap_or("")
        .to_lowercase()
}

fn is_distinct(f: &Function) -> bool {
    matches!(
        &f.args,
        FunctionArguments::List(al) if al.duplicate_treatment == Some(DuplicateTreatment::Distinct)
    )
}

/// Separator for `group_concat(expr[, sep])` / `string_agg(expr, sep)`; the
/// second argument is expected to be a literal, so it's read straight off
/// the AST rather than through `eval_expr` (which needs a row).
fn agg_separator(f: &Function) -> String {
    if let FunctionArguments::List(al) = &f.args
        && let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(v)))) = al.args.get(1)
        && let Value::Str(s) = eval_sql_value(&v.value)
    {
        return s;
    }
    ",".to_string()
}

impl<'a> SqlEngine<'a> {
    pub(super) fn project_and_aggregate(
        &self,
        select: &Select,
        rows: Vec<Row>,
        order_by: &Option<sqlparser::ast::OrderBy>,
        limit: Option<&Expr>,
        offset: Option<&Expr>,
        known_columns: &[String],
    ) -> Result<QueryOutput, MqdbError> {
        let group_by_exprs: Vec<Expr> = match &select.group_by {
            GroupByExpr::Expressions(exprs, _) => exprs.clone(),
            _ => vec![],
        };
        let is_agg = has_aggregate(&select.projection);

        if is_agg || !group_by_exprs.is_empty() {
            if !known_columns.is_empty() {
                for e in &group_by_exprs {
                    validate_expr_columns(e, known_columns)?;
                }
            }
            return self.aggregate(select, rows, order_by, limit, offset, &group_by_exprs);
        }

        // Plain SELECT
        let columns = projection_columns(&select.projection, rows.first());
        let mut result: Vec<(Row, Vec<String>)> = rows
            .into_iter()
            .map(|row| {
                let cells = project_row(&select.projection, &row);
                (row, cells)
            })
            .collect();

        // ORDER BY: a plain identifier may name a `SELECT ... AS x` alias
        // (a computed value with no column behind it, e.g. `bm25(...) AS
        // score`) rather than an input column, so resolve that first.
        if let Some(ob) = order_by
            && let OrderByKind::Expressions(exprs) = &ob.kind
        {
            let resolved: Vec<OrderByExpr> = exprs
                .iter()
                .map(|oe| OrderByExpr {
                    expr: resolve_order_expr_alias(&oe.expr, &select.projection),
                    ..oe.clone()
                })
                .collect();
            if !known_columns.is_empty() {
                for oe in &resolved {
                    validate_expr_columns(&oe.expr, known_columns)?;
                }
            }
            apply_order_by_exprs(&mut result, &resolved);
        }

        // DISTINCT
        let result: Vec<Vec<String>> = if select.distinct.is_some() {
            let mut seen = std::collections::HashSet::new();
            result
                .into_iter()
                .filter_map(|(_, cells)| {
                    if seen.insert(cells.clone()) {
                        Some(cells)
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            result.into_iter().map(|(_, cells)| cells).collect()
        };

        Ok(QueryOutput {
            columns,
            rows: apply_limit_offset(result, limit, offset),
        })
    }

    fn aggregate(
        &self,
        select: &Select,
        rows: Vec<Row>,
        order_by: &Option<sqlparser::ast::OrderBy>,
        limit: Option<&Expr>,
        offset: Option<&Expr>,
        group_by_exprs: &[Expr],
    ) -> Result<QueryOutput, MqdbError> {
        validate_agg_projection(&select.projection, group_by_exprs)?;

        let columns: Vec<String> = select
            .projection
            .iter()
            .enumerate()
            .map(|(i, item)| projection_col_name(item, i))
            .collect();

        // Group
        let mut groups: Vec<(Vec<Value>, Vec<&Row>)> = Vec::new();
        let mut key_index: FxHashMap<Vec<String>, usize> = FxHashMap::default();

        // We need owned rows to reference; collect first
        let owned: Vec<Row> = rows;

        if group_by_exprs.is_empty() {
            // Single group
            let all: Vec<&Row> = owned.iter().collect();
            let out_row = eval_agg_row(&select.projection, group_by_exprs, &[], &all);
            let out_rows = if select
                .having
                .as_ref()
                .is_some_and(|h| !eval_having(h, &select.projection, group_by_exprs, &[], &all))
            {
                vec![]
            } else {
                vec![out_row]
            };
            return Ok(QueryOutput {
                columns,
                rows: apply_limit_offset(out_rows, limit, offset),
            });
        }

        for row in &owned {
            let key: Vec<Value> = group_by_exprs.iter().map(|e| eval_expr(e, row)).collect();
            let key_str: Vec<String> = key.iter().map(|v| v.display()).collect();
            let idx = key_index.entry(key_str.clone()).or_insert_with(|| {
                groups.push((key, Vec::new()));
                groups.len() - 1
            });
            groups[*idx].1.push(row);
        }

        let mut filtered: Vec<&(Vec<Value>, Vec<&Row>)> = groups
            .iter()
            .filter(|(key_vals, group_rows)| {
                select.having.as_ref().is_none_or(|h| {
                    eval_having(h, &select.projection, group_by_exprs, key_vals, group_rows)
                })
            })
            .collect();

        if let Some(ob) = order_by
            && let OrderByKind::Expressions(exprs) = &ob.kind
        {
            let sort_keys: Vec<(Expr, bool)> = exprs
                .iter()
                .map(|oe| {
                    (
                        resolve_order_expr_alias(&oe.expr, &select.projection),
                        oe.options.sort == Some(OrderBySort::Desc),
                    )
                })
                .collect();
            filtered.sort_by(|a, b| {
                for (expr, desc) in &sort_keys {
                    let va = eval_agg_expr_value(expr, group_by_exprs, &a.0, &a.1);
                    let vb = eval_agg_expr_value(expr, group_by_exprs, &b.0, &b.1);
                    let mut ord = va.sort_cmp(&vb);
                    if *desc {
                        ord = ord.reverse();
                    }
                    if ord != std::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                std::cmp::Ordering::Equal
            });
        }

        let out_rows: Vec<Vec<String>> = filtered
            .iter()
            .map(|(key_vals, group_rows)| {
                eval_agg_row(&select.projection, group_by_exprs, key_vals, group_rows)
            })
            .collect();

        Ok(QueryOutput {
            columns,
            rows: apply_limit_offset(out_rows, limit, offset),
        })
    }
}
