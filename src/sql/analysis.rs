//! Static analysis of expressions (column validation, subquery detection).

use super::rows::ident_value;
use super::value::Value;
use crate::MqdbError;
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, FunctionArguments, InTarget, Query, Select, SelectItem,
    SetExpr, Statement, TableFactor, Value as SqlValue,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

pub(super) fn value_to_expr(v: &Value) -> Expr {
    match v {
        Value::Int(n) => Expr::Value(SqlValue::Number(n.to_string(), false).with_empty_span()),
        Value::Float(n) => Expr::Value(SqlValue::Number(format!("{n:e}"), false).with_empty_span()),
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
        Expr::In {
            expr,
            target: InTarget::List(list),
            ..
        } => {
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
        Expr::Subquery(_)
        | Expr::In {
            target: InTarget::Subquery(_),
            ..
        }
        | Expr::Exists { .. } => true,
        Expr::BinaryOp { left, right, .. } => expr_has_subquery(left) || expr_has_subquery(right),
        Expr::UnaryOp { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::Nested(expr)
        | Expr::Cast { expr, .. } => expr_has_subquery(expr),
        Expr::In {
            expr,
            target: InTarget::List(list),
            ..
        } => expr_has_subquery(expr) || list.iter().any(expr_has_subquery),
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

/// Pushes every subquery (scalar, `IN (SELECT ...)`, `EXISTS`) found in
/// `expr` onto `out`. Mirrors [`expr_has_subquery`]'s traversal.
pub(super) fn collect_subqueries<'e>(expr: &'e Expr, out: &mut Vec<&'e Query>) {
    match expr {
        Expr::Subquery(q) => out.push(q),
        Expr::In {
            expr,
            target: InTarget::Subquery(subquery),
            ..
        } => {
            collect_subqueries(expr, out);
            out.push(subquery);
        }
        Expr::Exists { subquery, .. } => out.push(subquery),
        Expr::BinaryOp { left, right, .. } => {
            collect_subqueries(left, out);
            collect_subqueries(right, out);
        }
        Expr::UnaryOp { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::Nested(expr)
        | Expr::Cast { expr, .. } => collect_subqueries(expr, out),
        Expr::In {
            expr,
            target: InTarget::List(list),
            ..
        } => {
            collect_subqueries(expr, out);
            list.iter().for_each(|e| collect_subqueries(e, out));
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_subqueries(expr, out);
            collect_subqueries(low, out);
            collect_subqueries(high, out);
        }
        Expr::Function(f) => {
            if let FunctionArguments::List(al) = &f.args {
                for a in &al.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = a {
                        collect_subqueries(e, out);
                    }
                }
            }
        }
        _ => {}
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

/// Cheap text check shared with the engine: does the query call `bm25(`?
pub(super) fn sql_uses_bm25(sql: &str) -> bool {
    sql.to_ascii_lowercase().contains("bm25(")
}

/// Whether running `sql` needs block data loaded. `false` only for a plain
/// `SELECT` that reads `documents` (or no table) with no CTE, join or subquery;
/// anything unrecognised or unparsable gives `true`, the safe default.
pub fn statement_needs_blocks(sql: &str) -> bool {
    let Ok(stmts) = Parser::parse_sql(&GenericDialect {}, sql) else {
        return true;
    };
    let [Statement::Query(q)] = stmts.as_slice() else {
        return true;
    };
    let SetExpr::Select(select) = q.body.as_ref() else {
        return true;
    };
    if q.with.is_some() || select.from.len() > 1 || sql_uses_bm25(sql) {
        return true;
    }
    if let Some(twj) = select.from.first() {
        let is_documents = matches!(
            &twj.relation,
            TableFactor::Table { name, args: None, .. }
                if name.to_string().eq_ignore_ascii_case("documents")
        );
        if !twj.joins.is_empty() || !is_documents {
            return true;
        }
    }
    // A second `select` keyword means a subquery, which may read another table.
    q.to_string()
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| *w == "select")
        .count()
        > 1
}
