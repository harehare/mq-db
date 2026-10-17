//! `read_csv()` / `read_json()` table functions.

use super::join::parse_display_value;
use super::plan::expr_str_val;
use super::rows::{ident_value, qualify_row};
use super::value::{Row, Value};
use crate::MqdbError;
use sqlparser::ast::{FunctionArg, FunctionArgExpr, TableFactor, TableFunctionArgs};

pub(super) fn table_factor_ident(factor: &TableFactor) -> Option<String> {
    match factor {
        TableFactor::Table { name, .. } => {
            Some(name.0.last().map(ident_value).unwrap_or("").to_lowercase())
        }
        _ => None,
    }
}

pub(super) fn resolve_table_function(
    name: &str,
    args: &TableFunctionArgs,
    prefix: &str,
) -> Result<Vec<Row>, MqdbError> {
    let path = table_function_path_arg(name, args)?;
    match name {
        "read_csv" => read_csv_rows(&path, prefix),
        "read_json" => read_json_rows(&path, prefix),
        _ => Err(MqdbError::SqlExec(format!(
            "unknown table function: {name}"
        ))),
    }
}

fn table_function_path_arg(name: &str, args: &TableFunctionArgs) -> Result<String, MqdbError> {
    let [FunctionArg::Unnamed(FunctionArgExpr::Expr(e))] = args.args.as_slice() else {
        return Err(MqdbError::SqlExec(format!(
            "{name}(path) expects exactly one string-literal argument"
        )));
    };
    expr_str_val(e)
        .ok_or_else(|| MqdbError::SqlExec(format!("{name}(path): path must be a string literal")))
}

fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' => in_quotes = true,
            ',' => record.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
            }
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    records
}

fn read_csv_rows(path: &str, prefix: &str) -> Result<Vec<Row>, MqdbError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| MqdbError::SqlExec(format!("read_csv('{path}'): {e}")))?;
    let mut records = parse_csv(&text).into_iter();
    let header = records.next().unwrap_or_default();
    let rows = records
        .map(|record| {
            let values = header
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    record
                        .get(i)
                        .map(|v| parse_display_value(v))
                        .unwrap_or(Value::Null)
                })
                .collect();
            qualify_row(
                Row {
                    columns: header.clone(),
                    values,
                },
                prefix,
            )
        })
        .collect();
    Ok(rows)
}

fn json_to_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::Null
            }
        }
        serde_json::Value::String(s) => Value::Str(s.clone()),
        other => Value::Str(other.to_string()),
    }
}

fn read_json_rows(path: &str, prefix: &str) -> Result<Vec<Row>, MqdbError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| MqdbError::SqlExec(format!("read_json('{path}'): {e}")))?;

    let mut columns: Vec<String> = Vec::new();
    let mut objects: Vec<serde_json::Map<String, serde_json::Value>> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| MqdbError::SqlExec(format!("read_json('{path}'): line {}: {e}", i + 1)))?;
        let serde_json::Value::Object(obj) = value else {
            return Err(MqdbError::SqlExec(format!(
                "read_json('{path}'): line {}: expected a JSON object per line",
                i + 1
            )));
        };
        for key in obj.keys() {
            if !columns.iter().any(|c| c == key) {
                columns.push(key.clone());
            }
        }
        objects.push(obj);
    }

    let rows = objects
        .into_iter()
        .map(|obj| {
            let values = columns
                .iter()
                .map(|c| obj.get(c).map(json_to_value).unwrap_or(Value::Null))
                .collect();
            qualify_row(
                Row {
                    columns: columns.clone(),
                    values,
                },
                prefix,
            )
        })
        .collect();
    Ok(rows)
}
