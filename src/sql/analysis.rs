//! Static analysis of expressions (column validation, subquery detection).

use super::rows::ident_value;
use super::value::Value;
use crate::MqdbError;
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Select, SelectItem, TableFactor,
    Value as SqlValue,
};

pub(super) fn value_to_expr(v: &Value) -> Expr {
    match v {
        Value::Int(n) => Expr::Value(SqlValue::Number(n.to_string(), false).with_empty_span()),
        Value::Float(n) => Expr::Value(SqlValue::Number(n.to_string(), false).with_empty_span()),
        Value::Bool(b) => Expr::Value(SqlValue::Boolean(*b).with_empty_span()),
        Value::Str(s) => Expr::Value(SqlValue::SingleQuotedString(s.clone()).with_empty_span()),
        Value::Null => Expr::Value(SqlValue::Null.with_empty_span()),
    }
}

/// Is `name` one of `columns`, matched the same way [`Row::get`] matches:
/// exact (qualified) match first, then a bare/short-name match against
/// either side.
fn column_known(columns: &[String], name: &str) -> bool {
    if columns.iter().any(|c| c.eq_ignore_ascii_case(name)) {
        return true;
    }
    let short = name.rsplit('.').next().unwrap_or(name);
    columns.iter().any(|c| {
        c.rsplit('.')
            .next()
            .unwrap_or(c)
            .eq_ignore_ascii_case(short)
    })
}

/// Collects every column reference (`Identifier`/`CompoundIdentifier`) in
/// `expr` into `out`. Mirrors the `Expr` variants [`eval_expr`] and
/// [`fold_constants`] already walk; an unhandled variant is simply not
/// descended into, which only means a reference buried in it goes
/// unchecked: never a false positive.
pub(super) fn collect_column_names(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Identifier(i) => out.push(i.value.clone()),
        Expr::CompoundIdentifier(parts) => out.push(
            parts
                .iter()
                .map(|i| i.value.as_str())
                .collect::<Vec<_>>()
                .join("."),
        ),
        Expr::BinaryOp { left, right, .. } => {
            collect_column_names(left, out);
            collect_column_names(right, out);
        }
        Expr::UnaryOp { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::Nested(expr)
        | Expr::Cast { expr, .. }
        | Expr::Ceil { expr, .. }
        | Expr::Floor { expr, .. }
        | Expr::Extract { expr, .. } => collect_column_names(expr, out),
        Expr::InList { expr, list, .. } => {
            collect_column_names(expr, out);
            for e in list {
                collect_column_names(e, out);
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_column_names(expr, out);
            collect_column_names(low, out);
            collect_column_names(high, out);
        }
        Expr::Like { expr, pattern, .. } | Expr::RLike { expr, pattern, .. } => {
            collect_column_names(expr, out);
            collect_column_names(pattern, out);
        }
        Expr::Position { expr, r#in } => {
            collect_column_names(expr, out);
            collect_column_names(r#in, out);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(o) = operand {
                collect_column_names(o, out);
            }
            for w in conditions {
                collect_column_names(&w.condition, out);
                collect_column_names(&w.result, out);
            }
            if let Some(e) = else_result {
                collect_column_names(e, out);
            }
        }
        Expr::Trim {
            expr,
            trim_what,
            trim_characters,
            ..
        } => {
            collect_column_names(expr, out);
            if let Some(w) = trim_what {
                collect_column_names(w, out);
            }
            if let Some(cs) = trim_characters {
                for c in cs {
                    collect_column_names(c, out);
                }
            }
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            collect_column_names(expr, out);
            if let Some(f) = substring_from {
                collect_column_names(f, out);
            }
            if let Some(f) = substring_for {
                collect_column_names(f, out);
            }
        }
        Expr::Function(f) => {
            if let FunctionArguments::List(al) = &f.args {
                for a in &al.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = a {
                        collect_column_names(e, out);
                    }
                }
            }
        }
        // Subqueries carry their own, independently-checked scope.
        _ => {}
    }
}

pub(super) fn validate_expr_columns(expr: &Expr, known: &[String]) -> Result<(), MqdbError> {
    let mut names = Vec::new();
    collect_column_names(expr, &mut names);
    for name in names {
        if !column_known(known, &name) {
            return Err(MqdbError::SqlExec(format!("unknown column: {name}")));
        }
    }
    Ok(())
}

pub(super) fn validate_projection_columns(
    projection: &[SelectItem],
    known: &[String],
) -> Result<(), MqdbError> {
    for item in projection {
        match item {
            SelectItem::UnnamedExpr(e)
            | SelectItem::ExprWithAlias { expr: e, .. }
            | SelectItem::ExprWithAliases { expr: e, .. } => validate_expr_columns(e, known)?,
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {}
        }
    }
    Ok(())
}

/// Does `expr` contain a scalar subquery, `IN (SELECT ...)`, or `EXISTS
/// (SELECT ...)` anywhere in its tree? Gates the (potentially correlated,
/// re-resolved-per-row) slow path in [`SqlEngine::exec_select`]'s WHERE
/// handling so ordinary predicates keep the fast once-only resolution.
pub(super) fn expr_has_subquery(expr: &Expr) -> bool {
    match expr {
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => true,
        Expr::BinaryOp { left, right, .. } => expr_has_subquery(left) || expr_has_subquery(right),
        Expr::UnaryOp { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::Nested(expr)
        | Expr::Cast { expr, .. } => expr_has_subquery(expr),
        Expr::InList { expr, list, .. } => {
            expr_has_subquery(expr) || list.iter().any(expr_has_subquery)
        }
        Expr::Between {
            expr, low, high, ..
        } => expr_has_subquery(expr) || expr_has_subquery(low) || expr_has_subquery(high),
        Expr::Function(f) => matches!(&f.args, FunctionArguments::List(al) if al
            .args
            .iter()
            .any(|a| matches!(a, FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) if expr_has_subquery(e)))),
        _ => false,
    }
}

/// The table names/aliases a `Select` binds itself, i.e. the names an
/// identifier inside it should resolve to *locally* rather than being
/// treated as a reference to an enclosing (correlated) query.
pub(super) fn select_own_aliases(select: &Select) -> Vec<String> {
    fn factor_alias(factor: &TableFactor, out: &mut Vec<String>) {
        if let TableFactor::Table { name, alias, .. } = factor {
            out.push(match alias {
                Some(a) => a.name.value.to_lowercase(),
                None => name.0.last().map(ident_value).unwrap_or("").to_lowercase(),
            });
        }
    }
    let mut names = Vec::new();
    for twj in &select.from {
        factor_alias(&twj.relation, &mut names);
        for j in &twj.joins {
            factor_alias(&j.relation, &mut names);
        }
    }
    names
}
