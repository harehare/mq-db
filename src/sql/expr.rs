//! Scalar expression evaluation.

use super::aggregate::is_aggregate_name;
use super::analysis::collect_column_names;
use super::datetime::eval_extract;
use super::fts::{Bm25CorpusScope, QUERY_TABLE_BM25, QUERY_TABLE_TOKENIZERS, TokenizerScope};
use super::pattern::{compile_regex, like_match_str};
use super::rows::{eval_sql_value, ident_value};
use super::scalar::eval_scalar_function;
use super::value::{Row, Value};
use sqlparser::ast::{
    BinaryOperator, CaseWhen, CeilFloorKind, DateTimeField, Expr, Function, FunctionArg,
    FunctionArgExpr, FunctionArguments, TrimWhereField, UnaryOperator, Value as SqlValue,
};

pub(super) fn eval_expr(expr: &Expr, row: &Row) -> Value {
    match expr {
        Expr::Value(v) => eval_sql_value(&v.value),
        Expr::Identifier(i) => row.get(&i.value).cloned().unwrap_or(Value::Null),
        Expr::CompoundIdentifier(parts) => {
            // CompoundIdentifier holds Vec<Ident> (not Vec<ObjectNamePart>)
            let full = parts
                .iter()
                .map(|i| i.value.as_str())
                .collect::<Vec<_>>()
                .join(".");
            let short = parts.last().map(|i| i.value.as_str()).unwrap_or("");
            row.get(&full)
                .or_else(|| row.get(short))
                .cloned()
                .unwrap_or(Value::Null)
        }
        Expr::BinaryOp { left, op, right } => eval_binary(left, op, right, row),
        Expr::UnaryOp { op, expr } => match op {
            UnaryOperator::Not => Value::Bool(!eval_expr(expr, row).is_truthy()),
            UnaryOperator::Minus => match eval_expr(expr, row) {
                Value::Int(n) => Value::Int(-n),
                Value::Float(f) => Value::Float(-f),
                _ => Value::Null,
            },
            _ => Value::Null,
        },
        Expr::IsNull(inner) => Value::Bool(matches!(eval_expr(inner, row), Value::Null)),
        Expr::IsNotNull(inner) => Value::Bool(!matches!(eval_expr(inner, row), Value::Null)),
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let val = eval_expr(expr, row);
            let found = list.iter().any(|e| eval_expr(e, row) == val);
            Value::Bool(if *negated { !found } else { found })
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let val = eval_expr(expr, row);
            let lo = eval_expr(low, row);
            let hi = eval_expr(high, row);
            let in_range = lo.cmp_val(&val).map(|o| o.is_le()).unwrap_or(false)
                && val.cmp_val(&hi).map(|o| o.is_le()).unwrap_or(false);
            Value::Bool(if *negated { !in_range } else { in_range })
        }
        Expr::Like {
            expr,
            negated,
            pattern,
            ..
        } => {
            let val = eval_expr(expr, row);
            let pat = eval_expr(pattern, row);
            if let (Value::Str(s), Value::Str(p)) = (val, pat) {
                let matched = like_match_str(&s, &p);
                Value::Bool(if *negated { !matched } else { matched })
            } else {
                Value::Bool(false)
            }
        }
        Expr::RLike {
            expr,
            negated,
            pattern,
            ..
        } => {
            let val = eval_expr(expr, row);
            let pat = eval_expr(pattern, row);
            if let (Value::Str(s), Value::Str(p)) = (val, pat) {
                let matched = compile_regex(&p).is_some_and(|re| re.is_match(&s));
                Value::Bool(if *negated { !matched } else { matched })
            } else {
                Value::Bool(false)
            }
        }
        Expr::Function(f) => eval_function_call(f, row),
        Expr::Nested(inner) => eval_expr(inner, row),
        Expr::Cast { expr, .. } => eval_expr(expr, row),
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => eval_case(operand.as_deref(), conditions, else_result.as_deref(), row),
        Expr::Trim {
            expr,
            trim_where,
            trim_what,
            trim_characters,
        } => eval_trim(expr, trim_where, trim_what, trim_characters, row),
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => eval_substring(expr, substring_from, substring_for, row),
        Expr::Position { expr, r#in } => eval_position(expr, r#in, row),
        Expr::Ceil { expr, field } => eval_ceil_floor(expr, field, row, true),
        Expr::Floor { expr, field } => eval_ceil_floor(expr, field, row, false),
        Expr::Extract { field, expr, .. } => eval_extract(field, expr, row),
        // Subqueries are pre-resolved by resolve_subqueries before eval
        _ => Value::Null,
    }
}

fn eval_case(
    operand: Option<&Expr>,
    conditions: &[CaseWhen],
    else_result: Option<&Expr>,
    row: &Row,
) -> Value {
    let operand_val = operand.map(|o| eval_expr(o, row));
    for when in conditions {
        let matched = match &operand_val {
            Some(ov) => *ov == eval_expr(&when.condition, row),
            None => eval_expr(&when.condition, row).is_truthy(),
        };
        if matched {
            return eval_expr(&when.result, row);
        }
    }
    else_result
        .map(|e| eval_expr(e, row))
        .unwrap_or(Value::Null)
}

fn eval_trim(
    expr: &Expr,
    trim_where: &Option<TrimWhereField>,
    trim_what: &Option<Box<Expr>>,
    trim_characters: &Option<Vec<Expr>>,
    row: &Row,
) -> Value {
    let s = match eval_expr(expr, row).as_str() {
        Some(s) => s.to_string(),
        None => return Value::Null,
    };
    let chars: Vec<char> = if let Some(w) = trim_what {
        eval_expr(w, row)
            .as_str()
            .map(|s| s.chars().collect())
            .unwrap_or_default()
    } else if let Some(cs) = trim_characters {
        cs.iter()
            .filter_map(|e| eval_expr(e, row).as_str().map(|s| s.to_string()))
            .collect::<String>()
            .chars()
            .collect()
    } else {
        vec![' ', '\t', '\n', '\r']
    };
    let is_trim_char = |c: char| chars.contains(&c);
    let trimmed = match trim_where {
        Some(TrimWhereField::Leading) => s.trim_start_matches(is_trim_char).to_string(),
        Some(TrimWhereField::Trailing) => s.trim_end_matches(is_trim_char).to_string(),
        _ => s.trim_matches(is_trim_char).to_string(),
    };
    Value::Str(trimmed)
}

fn eval_substring(
    expr: &Expr,
    substring_from: &Option<Box<Expr>>,
    substring_for: &Option<Box<Expr>>,
    row: &Row,
) -> Value {
    let s = match eval_expr(expr, row).as_str() {
        Some(s) => s.to_string(),
        None => return Value::Null,
    };
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len() as i64;
    let start_1based = substring_from
        .as_ref()
        .map(|e| eval_expr(e, row).as_i64().unwrap_or(1))
        .unwrap_or(1);
    let take = substring_for
        .as_ref()
        .map(|e| eval_expr(e, row).as_i64().unwrap_or(len));
    // SQL substring is 1-based; positions before 1 are clamped, consuming from
    // the requested length as if the string started earlier.
    let start_0based = (start_1based - 1).max(0) as usize;
    let end_0based = match take {
        Some(n) => {
            let end = start_1based - 1 + n.max(0);
            end.clamp(0, len) as usize
        }
        None => len as usize,
    };
    if start_0based >= chars.len() || end_0based <= start_0based {
        return Value::Str(String::new());
    }
    Value::Str(chars[start_0based..end_0based].iter().collect())
}

fn eval_position(expr: &Expr, r#in: &Expr, row: &Row) -> Value {
    let needle = eval_expr(expr, row);
    let haystack = eval_expr(r#in, row);
    match (needle.as_str(), haystack.as_str()) {
        (Some(needle), Some(haystack)) => {
            let hay_chars: Vec<char> = haystack.chars().collect();
            let needle_chars: Vec<char> = needle.chars().collect();
            if needle_chars.is_empty() {
                return Value::Int(0);
            }
            for i in 0..=hay_chars.len().saturating_sub(needle_chars.len()) {
                if hay_chars[i..i + needle_chars.len()] == needle_chars[..] {
                    return Value::Int(i as i64 + 1);
                }
            }
            Value::Int(0)
        }
        _ => Value::Null,
    }
}

fn eval_ceil_floor(expr: &Expr, field: &CeilFloorKind, row: &Row, is_ceil: bool) -> Value {
    let n = match eval_expr(expr, row).as_f64() {
        Some(n) => n,
        None => return Value::Null,
    };
    let scale = match field {
        CeilFloorKind::Scale(v) => match &v.value {
            SqlValue::Number(s, _) => s.parse::<i32>().unwrap_or(0),
            _ => 0,
        },
        CeilFloorKind::DateTimeField(DateTimeField::NoDateTime) => 0,
        // Date-truncation forms (`CEIL(x TO DAY)`) need calendar data we don't track.
        _ => return Value::Null,
    };
    let factor = 10f64.powi(scale);
    let scaled = n * factor;
    let rounded = if is_ceil {
        scaled.ceil()
    } else {
        scaled.floor()
    };
    let result = rounded / factor;
    if scale <= 0 && result.fract() == 0.0 {
        Value::Int(result as i64)
    } else {
        Value::Float(result)
    }
}

fn eval_binary(left: &Expr, op: &BinaryOperator, right: &Expr, row: &Row) -> Value {
    match op {
        BinaryOperator::And => {
            if !eval_expr(left, row).is_truthy() {
                return Value::Bool(false);
            }
            Value::Bool(eval_expr(right, row).is_truthy())
        }
        BinaryOperator::Or => {
            if eval_expr(left, row).is_truthy() {
                return Value::Bool(true);
            }
            Value::Bool(eval_expr(right, row).is_truthy())
        }
        BinaryOperator::Eq => Value::Bool(eval_expr(left, row).sql_eq(&eval_expr(right, row))),
        BinaryOperator::NotEq => Value::Bool(!eval_expr(left, row).sql_eq(&eval_expr(right, row))),
        BinaryOperator::Lt => cmp_op(left, right, row, |o| o.is_lt()),
        BinaryOperator::LtEq => cmp_op(left, right, row, |o| o.is_le()),
        BinaryOperator::Gt => cmp_op(left, right, row, |o| o.is_gt()),
        BinaryOperator::GtEq => cmp_op(left, right, row, |o| o.is_ge()),
        BinaryOperator::Plus => arith_op(left, right, row, |a, b| a + b, |a, b| a + b),
        BinaryOperator::Minus => arith_op(left, right, row, |a, b| a - b, |a, b| a - b),
        BinaryOperator::Multiply => arith_op(left, right, row, |a, b| a * b, |a, b| a * b),
        BinaryOperator::Divide => {
            let (l, r) = (
                numeric(eval_expr(left, row)),
                numeric(eval_expr(right, row)),
            );
            match (&l, &r) {
                (Value::Int(a), Value::Int(b)) if *b != 0 => Value::Int(a / b),
                _ => match (l.as_f64(), r.as_f64()) {
                    (Some(a), Some(b)) if b != 0.0 => Value::Float(a / b),
                    _ => Value::Null,
                },
            }
        }
        BinaryOperator::StringConcat => {
            let l = eval_expr(left, row);
            let r = eval_expr(right, row);
            Value::Str(format!("{}{}", l.display(), r.display()))
        }
        _ => Value::Null,
    }
}

fn cmp_op(l: &Expr, r: &Expr, row: &Row, f: impl Fn(std::cmp::Ordering) -> bool) -> Value {
    Value::Bool(
        eval_expr(l, row)
            .cmp_val(&eval_expr(r, row))
            .map(f)
            .unwrap_or(false),
    )
}

/// A string holding a number, such as a custom table cell, as that number.
/// Anything else is returned unchanged.
fn numeric(v: Value) -> Value {
    match &v {
        Value::Str(s) => {
            if let Ok(n) = s.trim().parse::<i64>() {
                Value::Int(n)
            } else if let Some(f) = s.trim().parse::<f64>().ok().filter(|f| f.is_finite()) {
                Value::Float(f)
            } else {
                v
            }
        }
        _ => v,
    }
}

fn arith_op(
    l: &Expr,
    r: &Expr,
    row: &Row,
    int_f: impl Fn(i64, i64) -> i64,
    flt_f: impl Fn(f64, f64) -> f64,
) -> Value {
    let (lv, rv) = (numeric(eval_expr(l, row)), numeric(eval_expr(r, row)));
    match (&lv, &rv) {
        (Value::Int(a), Value::Int(b)) => Value::Int(int_f(*a, *b)),
        _ => match (lv.as_f64(), rv.as_f64()) {
            (Some(a), Some(b)) => Value::Float(flt_f(a, b)),
            _ => Value::Null,
        },
    }
}

/// The alias/table qualifier of a function's first argument, e.g. `o` for
/// `match(o.content, …)` or `match(lower(o.content), …)`; `None` if
/// unqualified or if columns from more than one alias are mixed in.
fn first_arg_alias(f: &Function) -> Option<String> {
    let FunctionArguments::List(al) = &f.args else {
        return None;
    };
    let expr = match al.args.first() {
        Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(e))) => e,
        _ => return None,
    };
    let mut names = Vec::new();
    collect_column_names(expr, &mut names);
    let mut aliases = names
        .iter()
        .map(|n| n.split_once('.').map(|(alias, _)| alias.to_lowercase()));
    let first = aliases.next()??;
    aliases
        .all(|alias| alias.as_deref() == Some(first.as_str()))
        .then_some(first)
}

fn eval_function_call(f: &Function, row: &Row) -> Value {
    let name = f.name.0.last().map(ident_value).unwrap_or("");
    let name_lower = name.to_lowercase();
    // Aggregates return placeholder; resolved later
    if is_aggregate_name(&name_lower) {
        return Value::Int(1);
    }
    let args: Vec<Value> = match &f.args {
        FunctionArguments::List(al) => al
            .args
            .iter()
            .filter_map(|a| match a {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(eval_expr(e, row)),
                _ => None,
            })
            .collect(),
        _ => vec![],
    };
    match name_lower.as_str() {
        "match" | "score" => {
            let _scope = first_arg_alias(f)
                .and_then(|alias| QUERY_TABLE_TOKENIZERS.with(|m| m.borrow().get(&alias).copied()))
                .map(TokenizerScope::set);
            eval_scalar_function(name, &args)
        }
        "bm25" => {
            let _scope = first_arg_alias(f)
                .and_then(|alias| QUERY_TABLE_BM25.with(|m| m.borrow().get(&alias).cloned()))
                .map(|corpus| Bm25CorpusScope::set(Some(corpus)));
            eval_scalar_function(name, &args)
        }
        _ => eval_scalar_function(name, &args),
    }
}
