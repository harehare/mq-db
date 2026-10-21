//! Expression rewriting: outer-reference substitution and constant folding.

use super::analysis::{select_own_aliases, value_to_expr};
use super::expr::eval_expr;
use super::value::Row;
use sqlparser::ast::{
    CaseWhen, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Query, SelectItem,
    SetExpr,
};

/// Replaces any `<alias>.<column>` reference in `expr` with its literal
/// value from `outer_row`, unless `<alias>` is one of `own_aliases` (i.e. it
/// resolves locally, not to the enclosing correlated query). Only qualified
/// references are substituted; an unqualified identifier is always left to
/// resolve against the subquery's own rows, which is the common and
/// unambiguous style for a correlated subquery (`WHERE b.document_id =
/// d.id`, not a bare `id`).
fn substitute_outer_refs(expr: &Expr, outer_row: &Row, own_aliases: &[String]) -> Expr {
    match expr {
        Expr::CompoundIdentifier(parts) => {
            let alias = parts
                .first()
                .map(|i| i.value.to_lowercase())
                .unwrap_or_default();
            if own_aliases.contains(&alias) {
                return expr.clone();
            }
            let full = parts
                .iter()
                .map(|i| i.value.as_str())
                .collect::<Vec<_>>()
                .join(".");
            match outer_row.get(&full) {
                Some(v) => value_to_expr(v),
                None => expr.clone(),
            }
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(substitute_outer_refs(left, outer_row, own_aliases)),
            op: op.clone(),
            right: Box::new(substitute_outer_refs(right, outer_row, own_aliases)),
        },
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op: *op,
            expr: Box::new(substitute_outer_refs(expr, outer_row, own_aliases)),
        },
        Expr::Nested(inner) => Expr::Nested(Box::new(substitute_outer_refs(
            inner,
            outer_row,
            own_aliases,
        ))),
        Expr::IsNull(inner) => Expr::IsNull(Box::new(substitute_outer_refs(
            inner,
            outer_row,
            own_aliases,
        ))),
        Expr::IsNotNull(inner) => Expr::IsNotNull(Box::new(substitute_outer_refs(
            inner,
            outer_row,
            own_aliases,
        ))),
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(substitute_outer_refs(expr, outer_row, own_aliases)),
            list: list
                .iter()
                .map(|e| substitute_outer_refs(e, outer_row, own_aliases))
                .collect(),
            negated: *negated,
        },
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => Expr::Between {
            expr: Box::new(substitute_outer_refs(expr, outer_row, own_aliases)),
            negated: *negated,
            low: Box::new(substitute_outer_refs(low, outer_row, own_aliases)),
            high: Box::new(substitute_outer_refs(high, outer_row, own_aliases)),
        },
        Expr::Like {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => Expr::Like {
            negated: *negated,
            any: *any,
            expr: Box::new(substitute_outer_refs(expr, outer_row, own_aliases)),
            pattern: Box::new(substitute_outer_refs(pattern, outer_row, own_aliases)),
            escape_char: escape_char.clone(),
        },
        Expr::RLike {
            negated,
            expr,
            pattern,
            regexp,
        } => Expr::RLike {
            negated: *negated,
            expr: Box::new(substitute_outer_refs(expr, outer_row, own_aliases)),
            pattern: Box::new(substitute_outer_refs(pattern, outer_row, own_aliases)),
            regexp: *regexp,
        },
        Expr::Cast {
            kind,
            expr,
            data_type,
            format,
        } => Expr::Cast {
            kind: kind.clone(),
            expr: Box::new(substitute_outer_refs(expr, outer_row, own_aliases)),
            data_type: data_type.clone(),
            format: format.clone(),
        },
        Expr::Case {
            case_token,
            end_token,
            operand,
            conditions,
            else_result,
        } => Expr::Case {
            case_token: case_token.clone(),
            end_token: end_token.clone(),
            operand: operand
                .as_ref()
                .map(|e| Box::new(substitute_outer_refs(e, outer_row, own_aliases))),
            conditions: conditions
                .iter()
                .map(|w| CaseWhen {
                    condition: substitute_outer_refs(&w.condition, outer_row, own_aliases),
                    result: substitute_outer_refs(&w.result, outer_row, own_aliases),
                })
                .collect(),
            else_result: else_result
                .as_ref()
                .map(|e| Box::new(substitute_outer_refs(e, outer_row, own_aliases))),
        },
        Expr::Function(f) => {
            let new_args = match &f.args {
                FunctionArguments::List(al) => {
                    let args =
                        al.args
                            .iter()
                            .map(|a| match a {
                                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(
                                        substitute_outer_refs(e, outer_row, own_aliases),
                                    ))
                                }
                                other => other.clone(),
                            })
                            .collect();
                    FunctionArguments::List(sqlparser::ast::FunctionArgumentList {
                        args,
                        ..al.clone()
                    })
                }
                other => other.clone(),
            };
            Expr::Function(Function {
                args: new_args,
                ..f.clone()
            })
        }
        other => other.clone(),
    }
}

/// Binds a (possibly correlated) subquery's free `<outer alias>.<column>`
/// references to `outer_row`'s values before it is executed. Only rewrites
/// a plain `SELECT` body; a nested set operation or further `WITH` is left
/// as-is, so correlation there simply won't resolve (no worse than before).
pub(super) fn substitute_outer_refs_query(query: &Query, outer_row: &Row) -> Query {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return query.clone();
    };
    let own_aliases = select_own_aliases(select);
    let mut new_select = select.clone();
    if let Some(sel) = &select.selection {
        new_select.selection = Some(substitute_outer_refs(sel, outer_row, &own_aliases));
    }
    if let Some(h) = &select.having {
        new_select.having = Some(substitute_outer_refs(h, outer_row, &own_aliases));
    }
    new_select.projection = select
        .projection
        .iter()
        .map(|item| match item {
            SelectItem::UnnamedExpr(e) => {
                SelectItem::UnnamedExpr(substitute_outer_refs(e, outer_row, &own_aliases))
            }
            SelectItem::ExprWithAlias { expr, alias } => SelectItem::ExprWithAlias {
                expr: substitute_outer_refs(expr, outer_row, &own_aliases),
                alias: alias.clone(),
            },
            other => other.clone(),
        })
        .collect();
    let mut new_query = query.clone();
    new_query.body = Box::new(SetExpr::Select(new_select));
    new_query
}

fn is_constant_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Value(_) => true,
        Expr::UnaryOp { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::Nested(expr)
        | Expr::Cast { expr, .. } => is_constant_expr(expr),
        Expr::BinaryOp { left, right, .. } => is_constant_expr(left) && is_constant_expr(right),
        Expr::Between {
            expr, low, high, ..
        } => is_constant_expr(expr) && is_constant_expr(low) && is_constant_expr(high),
        Expr::InList { expr, list, .. } => {
            is_constant_expr(expr) && list.iter().all(is_constant_expr)
        }
        _ => false,
    }
}

pub(super) fn fold_constants(expr: &Expr) -> Expr {
    let folded = match expr {
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(fold_constants(left)),
            op: op.clone(),
            right: Box::new(fold_constants(right)),
        },
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op: *op,
            expr: Box::new(fold_constants(expr)),
        },
        Expr::Nested(inner) => Expr::Nested(Box::new(fold_constants(inner))),
        Expr::IsNull(inner) => Expr::IsNull(Box::new(fold_constants(inner))),
        Expr::IsNotNull(inner) => Expr::IsNotNull(Box::new(fold_constants(inner))),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => Expr::Between {
            expr: Box::new(fold_constants(expr)),
            negated: *negated,
            low: Box::new(fold_constants(low)),
            high: Box::new(fold_constants(high)),
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(fold_constants(expr)),
            list: list.iter().map(fold_constants).collect(),
            negated: *negated,
        },
        other => other.clone(),
    };
    if matches!(folded, Expr::Value(_)) || !is_constant_expr(&folded) {
        return folded;
    }
    let dummy = Row {
        columns: vec![],
        values: vec![],
    };
    value_to_expr(&eval_expr(&folded, &dummy))
}
