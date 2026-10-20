//! Conversion of documents and blocks to rows, plus shared row/AST helpers.

use super::output::QueryOutput;
use super::value::{Row, Value};
use crate::MqdbError;
use crate::block::{Block, Properties, PropertyValue};
use crate::document::Document;
use sqlparser::ast::{ObjectName, ObjectNamePart, Value as SqlValue};

fn pv_to_json(pv: &PropertyValue) -> String {
    match pv {
        PropertyValue::String(s) => {
            format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
        }
        PropertyValue::Int(n) => n.to_string(),
        PropertyValue::Float(f) => f.to_string(),
        PropertyValue::Bool(b) => b.to_string(),
        PropertyValue::Array(arr) => {
            format!(
                "[{}]",
                arr.iter().map(pv_to_json).collect::<Vec<_>>().join(",")
            )
        }
        PropertyValue::Null => "null".to_string(),
    }
}

fn properties_to_json(props: &Properties) -> String {
    pairs_to_json(props.iter())
}

fn pairs_to_json<'a>(pairs: impl Iterator<Item = (&'a String, &'a PropertyValue)>) -> String {
    let pairs: Vec<String> = pairs
        .map(|(k, v)| {
            format!(
                "\"{}\":{}",
                k.replace('\\', "\\\\").replace('"', "\\\""),
                pv_to_json(v)
            )
        })
        .collect();
    format!("{{{}}}", pairs.join(","))
}

pub(super) fn block_to_row(doc_id: u32, block: &Block, block_idx: u32) -> Row {
    Row {
        columns: vec![
            "id".into(),
            "document_id".into(),
            "block_type".into(),
            "content".into(),
            "pre".into(),
            "post".into(),
            "depth".into(),
            "lang".into(),
            "properties".into(),
        ],
        values: vec![
            Value::Int(block_idx as i64),
            Value::Int(doc_id as i64),
            Value::Str(block.block_type.as_str().to_string()),
            Value::Str(block.content.clone()),
            Value::Int(block.pre as i64),
            Value::Int(block.post as i64),
            Value::Int(block.heading_depth().unwrap_or(0) as i64),
            Value::Str(block.code_lang().unwrap_or("").to_string()),
            Value::Str(properties_to_json(&block.properties)),
        ],
    }
}

fn json_string_array<'a>(items: impl Iterator<Item = &'a String>) -> String {
    let items: Vec<String> = items
        .map(|s| format!("\"{}\"", s.replace('"', "\\\"")))
        .collect();
    format!("[{}]", items.join(","))
}

fn json_string_array_sorted<'a>(items: impl Iterator<Item = &'a String>) -> String {
    let mut sorted: Vec<&String> = items.collect();
    sorted.sort();
    json_string_array(sorted.into_iter())
}

pub(super) fn doc_to_row(doc: &Document) -> Row {
    Row {
        columns: vec![
            "id".into(),
            "path".into(),
            "title".into(),
            "tags".into(),
            "block_count".into(),
            "max_heading_depth".into(),
            "code_languages".into(),
            "frontmatter_keys".into(),
            "properties".into(),
        ],
        values: vec![
            Value::Int(doc.id as i64),
            Value::Str(
                doc.path
                    .as_ref()
                    .and_then(|p| p.to_str())
                    .unwrap_or("")
                    .to_string(),
            ),
            Value::Str(doc.zone_maps.title.clone().unwrap_or_default()),
            Value::Str(json_string_array(doc.zone_maps.tags.iter())),
            Value::Int(doc.block_count as i64),
            Value::Int(doc.zone_maps.max_heading_depth as i64),
            Value::Str(json_string_array_sorted(
                doc.zone_maps.code_languages.iter(),
            )),
            Value::Str(json_string_array_sorted(
                doc.zone_maps.frontmatter_keys.iter(),
            )),
            Value::Str(pairs_to_json(
                doc.zone_maps.frontmatter.iter().map(|(k, v)| (k, v)),
            )),
        ],
    }
}

pub(super) fn qualify_row(row: Row, prefix: &str) -> Row {
    Row {
        columns: row
            .columns
            .iter()
            .map(|c| format!("{}.{}", prefix, c))
            .collect(),
        values: row.values,
    }
}

pub(super) fn eval_sql_value(v: &SqlValue) -> Value {
    match v {
        SqlValue::Number(n, _) => {
            if let Ok(i) = n.parse::<i64>() {
                Value::Int(i)
            } else if let Ok(f) = n.parse::<f64>() {
                Value::Float(f)
            } else {
                Value::Null
            }
        }
        SqlValue::SingleQuotedString(s) | SqlValue::DoubleQuotedString(s) => Value::Str(s.clone()),
        SqlValue::Boolean(b) => Value::Bool(*b),
        SqlValue::Null => Value::Null,
        _ => Value::Null,
    }
}

pub(super) fn ident_value(part: &ObjectNamePart) -> &str {
    match part {
        ObjectNamePart::Identifier(i) => &i.value,
        ObjectNamePart::Function(_) => "",
    }
}

/// Table name for DDL/DML targets, which must be unqualified; writes
/// through an ATTACHed database's `<alias>.<table>` are not supported.
pub(super) fn require_unqualified(name: &ObjectName) -> Result<String, MqdbError> {
    if name.0.len() > 1 {
        return Err(MqdbError::SqlExec(format!(
            "'{}': writes to an attached database are not supported — only SELECT/JOIN may use <alias>.<table>",
            name.0.iter().map(ident_value).collect::<Vec<_>>().join(".")
        )));
    }
    Ok(name.0.last().map(ident_value).unwrap_or("").to_lowercase())
}

/// Single-row `("ok")` result for statements with no natural row output
/// (`ATTACH`/`DETACH`).
pub(super) fn ok_result() -> QueryOutput {
    QueryOutput {
        columns: vec!["result".to_string()],
        rows: vec![vec!["ok".to_string()]],
    }
}
