//! Conversion of documents and blocks to rows, plus shared row/AST helpers.

use super::output::{QueryOutput, json_string};
use super::value::{Row, Value};
use crate::MqdbError;
use crate::block::{Block, Properties, PropertyValue};
use crate::document::Document;
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};
use sqlparser::ast::{ObjectName, ObjectNamePart, Value as SqlValue};

fn pv_to_json(pv: &PropertyValue) -> String {
    match pv {
        PropertyValue::String(s) => json_string(s),
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
        .map(|(k, v)| format!("{}:{}", json_string(k), pv_to_json(v)))
        .collect();
    format!("{{{}}}", pairs.join(","))
}

thread_local! {
    /// Whether the running query may read `blocks.content_hash`. Defaults to
    /// true so a query run outside `SqlEngine::execute` is never given a
    /// placeholder.
    pub(super) static QUERY_NEEDS_CONTENT_HASH: std::cell::Cell<bool> =
        const { std::cell::Cell::new(true) };
}

/// Cheap text scan, conservative: any `*` other than in `count(*)` may expand
/// to `content_hash`.
pub(super) fn sql_needs_content_hash(sql: &str) -> bool {
    let sql: String = sql
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    sql.contains("content_hash") || sql.replace("count(*)", "").contains('*')
}

/// SHA-256 over `block_type`, a NUL and `content`, as 64 hex digits.
fn content_hash(block: &Block) -> String {
    let mut hasher = Sha256::new();
    hasher.update(block.block_type.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(block.content.as_bytes());
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for b in hasher.finalize() {
        out.push(HEX[usize::from(b >> 4)] as char);
        out.push(HEX[usize::from(b & 0xf)] as char);
    }
    out
}

/// A location key for every block of `doc` that survives edits elsewhere.
///
/// A heading is its slug path (`guide/install`, repeated siblings get `-2`,
/// `-3`). Any other block is `<section path>::<type>:<n>`, the `n`th block of
/// that type directly under the section.
pub(super) fn block_anchors(doc: &Document) -> Vec<String> {
    #[derive(Default)]
    struct Scope {
        depth: u8,
        path: String,
        children: FxHashMap<String, usize>,
        types: FxHashMap<&'static str, usize>,
    }
    let mut stack = vec![Scope::default()];
    let mut anchors = Vec::with_capacity(doc.blocks.len());
    for block in &doc.blocks {
        if let Some(depth) = block.heading_depth() {
            while stack.len() > 1 && stack.last().is_some_and(|s| s.depth >= depth) {
                stack.pop();
            }
            let slug = match block.properties.get("slug") {
                Some(PropertyValue::String(s)) if !s.is_empty() => s.clone(),
                _ => block.content.to_lowercase(),
            };
            let parent = stack.last_mut().expect("root scope is never popped");
            let n = parent.children.entry(slug.clone()).or_insert(0);
            *n += 1;
            let segment = if *n == 1 { slug } else { format!("{slug}-{n}") };
            let path = if parent.path.is_empty() {
                segment
            } else {
                format!("{}/{segment}", parent.path)
            };
            anchors.push(path.clone());
            stack.push(Scope {
                depth,
                path,
                ..Scope::default()
            });
        } else {
            let scope = stack.last_mut().expect("root scope is never popped");
            let ty = block.block_type.as_str();
            let n = scope.types.entry(ty).or_insert(0);
            *n += 1;
            anchors.push(format!("{}::{ty}:{n}", scope.path));
        }
    }
    anchors
}

pub(super) fn block_to_row(doc_id: u32, block: &Block, block_idx: u32, anchor: &str) -> Row {
    let hash = if QUERY_NEEDS_CONTENT_HASH.with(std::cell::Cell::get) {
        Value::Str(content_hash(block))
    } else {
        Value::Null
    };
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
            "content_hash".into(),
            "anchor".into(),
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
            hash,
            Value::Str(anchor.to_string()),
        ],
    }
}

/// One row per link or image recorded on `block` at index time.
pub(super) fn link_rows(doc_id: u32, block: &Block, block_idx: u32) -> Vec<Row> {
    let Some(PropertyValue::Array(links)) = block.properties.get("links") else {
        return Vec::new();
    };
    links
        .iter()
        .filter_map(|link| {
            let PropertyValue::Array(fields) = link else {
                return None;
            };
            let field = |i: usize| match fields.get(i) {
                Some(PropertyValue::String(s)) => Value::Str(s.clone()),
                _ => Value::Str(String::new()),
            };
            Some(Row {
                columns: vec![
                    "document_id".into(),
                    "block_id".into(),
                    "kind".into(),
                    "url".into(),
                    "text".into(),
                    "title".into(),
                ],
                values: vec![
                    Value::Int(doc_id as i64),
                    Value::Int(block_idx as i64),
                    field(0),
                    field(1),
                    field(2),
                    field(3),
                ],
            })
        })
        .collect()
}

fn json_string_array<'a>(items: impl Iterator<Item = &'a String>) -> String {
    let items: Vec<String> = items.map(|s| json_string(s)).collect();
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
