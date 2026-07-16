//! Custom SQL execution engine for mq-db.
//!
//! Executes SQL queries directly against the in-memory [`DocumentStore`]
//! without copying data into an external database. Uses [`sqlparser`] to parse
//! SQL and evaluates predicates natively against [`Block`] data — including the
//! O(1) `under(pre, post, anc_pre, anc_post)` interval-index function.
//!
//! # Virtual Schema
//!
//! ```sql
//! -- documents table
//! SELECT id, path, title, tags FROM documents;
//!
//! -- blocks table
//! SELECT id, document_id, block_type, content, pre, post, depth, lang,
//!        properties FROM blocks;
//! ```
//!
//! # Built-in Functions
//!
//! | Function | Description |
//! |---|---|
//! | `under(pre, post, anc_pre, anc_post)` | O(1) interval ancestor check |
//! | `json_extract(json, path)` | Extract value from JSON string |
//! | `mq(program, content)` | Run an mq program against Markdown content |
//! | `count`/`min`/`max`/`sum`/`avg`/`group_concat`/`string_agg` | Aggregates (`count` and `group_concat`/`string_agg` support `DISTINCT`) |
//! | `lower`/`upper`/`length`/`trim`/`ltrim`/`rtrim`/`concat`/`concat_ws`/`replace`/`left`/`right`/`lpad`/`rpad`/`reverse`/`repeat`/`initcap`/`ascii`/`chr`/`instr`/`split_part`/`substring`/`substr`/`position` | String functions |
//! | `abs`/`round`/`ceil`/`floor`/`trunc`/`mod`/`power`/`sqrt`/`sign`/`exp`/`ln`/`log`/`log10`/`log2`/`pi`/`greatest`/`least` | Numeric functions |
//! | `coalesce`/`ifnull`/`nullif` | Null handling |
//! | `typeof`/`now`/`current_timestamp`/`current_date`/`current_time`/`CASE WHEN` | Misc |
//!
//! # Example
//!
//! ```rust,no_run
//! use mq_db::{DocumentStore, SqlEngine};
//!
//! let mut store = DocumentStore::new();
//! store.add_str("# Hello\n\n## Architecture\n\nDetails\n\n```rust\ncode\n```\n").unwrap();
//!
//! let engine = SqlEngine::new(&store).unwrap();
//! let out = engine.execute(
//!     "SELECT block_type, content FROM blocks WHERE block_type = 'heading'"
//! ).unwrap();
//! assert!(!out.rows.is_empty());
//! ```

use rustc_hash::FxHashMap;
use sqlparser::{
    ast::{
        AssignmentTarget, BinaryOperator, CaseWhen, CeilFloorKind, CreateTable, DateTimeField,
        DuplicateTreatment, Expr, FromTable, Function, FunctionArg, FunctionArgExpr,
        FunctionArguments, GroupByExpr, Insert, JoinConstraint, JoinOperator, LimitClause,
        ObjectName, ObjectNamePart, ObjectType, OrderByExpr, OrderByKind, Query, Select,
        SelectItem, SetExpr, Statement, TableFactor, TableObject, TableWithJoins, TrimWhereField,
        UnaryOperator, Value as SqlValue, Values,
    },
    dialect::GenericDialect,
    parser::Parser,
};

use mq_lang::{DefaultEngine, parse_markdown_input};

use crate::{
    DocumentStore, MqdbError,
    block::{Block, BlockType, Properties, PropertyValue},
    document::{Document, ZoneMaps},
    indexes::{DocumentIndex, IndexHint, tokenize},
    store::CustomTableState,
};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

impl Value {
    fn as_str(&self) -> Option<&str> {
        if let Value::Str(s) = self {
            Some(s)
        } else {
            None
        }
    }
    fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            Value::Float(f) => Some(*f as i64),
            _ => None,
        }
    }
    fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            Value::Int(n) => Some(*n as f64),
            _ => None,
        }
    }
    fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Int(n) => *n != 0,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Null => false,
        }
    }
    fn display(&self) -> String {
        match self {
            Value::Str(s) => s.clone(),
            Value::Int(n) => n.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Null => "NULL".to_string(),
        }
    }
    fn cmp_val(&self, other: &Value) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
            (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
            (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
            (Value::Null, Value::Null) => Some(std::cmp::Ordering::Equal),
            _ => None,
        }
    }
}

/// Hashable projection of [`Value`], mirroring its derived `PartialEq` (no
/// cross-variant coercion, `NULL` equals `NULL`, `NaN` matches nothing).
#[derive(PartialEq, Eq, Hash)]
enum JoinKey {
    Str(String),
    Int(i64),
    Bool(bool),
    FloatBits(u64),
    Null,
}

fn value_join_key(v: &Value) -> Option<JoinKey> {
    match v {
        Value::Str(s) => Some(JoinKey::Str(s.clone())),
        Value::Int(i) => Some(JoinKey::Int(*i)),
        Value::Bool(b) => Some(JoinKey::Bool(*b)),
        Value::Null => Some(JoinKey::Null),
        Value::Float(f) if f.is_nan() => None, // NaN matches nothing
        Value::Float(f) => {
            let normalized = if *f == 0.0 { 0.0 } else { *f };
            Some(JoinKey::FloatBits(normalized.to_bits()))
        }
    }
}

#[derive(Debug, Clone)]
struct Row {
    columns: Vec<String>,
    values: Vec<Value>,
}

impl Row {
    fn get(&self, col: &str) -> Option<&Value> {
        let col_lower = col.to_lowercase();
        if let Some(i) = self
            .columns
            .iter()
            .position(|c| c.to_lowercase() == col_lower)
        {
            return self.values.get(i);
        }
        // Try short name (strip "table." prefix from query)
        let short = col_lower.split('.').next_back().unwrap_or(&col_lower);
        // Match "alias.col" columns
        self.columns
            .iter()
            .position(|c| {
                let cl = c.to_lowercase();
                cl == col_lower || cl.split('.').next_back().unwrap_or(&cl) == short
            })
            .and_then(|i| self.values.get(i))
    }
}

fn json_value_str(s: &str) -> String {
    if let Ok(n) = s.parse::<i64>() {
        return n.to_string();
    }
    if let Ok(f) = s.parse::<f64>() {
        return f.to_string();
    }
    if s == "true" || s == "false" || s == "null" || s == "NULL" {
        return s.to_lowercase();
    }
    // Treat as JSON string — escape quotes and backslashes
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn csv_cell(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn csv_row(fields: &[String]) -> String {
    let mut row = fields
        .iter()
        .map(|f| csv_cell(f))
        .collect::<Vec<_>>()
        .join(",");
    row.push('\n');
    row
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The tabular output of a SQL query.
#[derive(Debug)]
pub struct QueryOutput {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl QueryOutput {
    /// Render as a JSON array of objects, one object per row.
    pub fn to_json(&self) -> String {
        if self.rows.is_empty() {
            return "[]\n".to_string();
        }
        let objects: Vec<String> = self
            .rows
            .iter()
            .map(|row| {
                let pairs: Vec<String> = self
                    .columns
                    .iter()
                    .zip(row.iter())
                    .map(|(col, val)| {
                        format!(
                            "\"{}\":{}",
                            col.replace('\\', "\\\\").replace('"', "\\\""),
                            json_value_str(val)
                        )
                    })
                    .collect();
                format!("{{{}}}", pairs.join(","))
            })
            .collect();
        format!("[{}]\n", objects.join(","))
    }

    /// Render as RFC 4180 CSV with a header row.
    pub fn to_csv(&self) -> String {
        let mut out = String::new();
        if !self.columns.is_empty() {
            out.push_str(&csv_row(&self.columns));
        }
        for row in &self.rows {
            out.push_str(&csv_row(row));
        }
        out
    }

    /// Render as tab-separated values with a header row.
    pub fn to_tsv(&self) -> String {
        let mut out = String::new();
        if !self.columns.is_empty() {
            out.push_str(&self.columns.join("\t"));
            out.push('\n');
        }
        for row in &self.rows {
            out.push_str(&row.join("\t"));
            out.push('\n');
        }
        out
    }

    /// Render as a GFM Markdown table.
    pub fn to_markdown_table(&self) -> String {
        if self.columns.is_empty() {
            return String::new();
        }
        let mut widths: Vec<usize> = self.columns.iter().map(|h| h.len().max(3)).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < widths.len() {
                    widths[i] = widths[i].max(cell.len());
                }
            }
        }

        let mut out = String::new();
        out.push('|');
        for (i, h) in self.columns.iter().enumerate() {
            out.push_str(&format!(" {:<w$} |", h, w = widths[i]));
        }
        out.push('\n');

        out.push('|');
        for &w in &widths {
            out.push_str(&format!(" {} |", "-".repeat(w)));
        }
        out.push('\n');

        for row in &self.rows {
            out.push('|');
            for (i, &w) in widths.iter().enumerate() {
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                let escaped = cell
                    .replace('|', "\\|")
                    .replace('\n', " ")
                    .replace('\r', "");
                out.push_str(&format!(" {:<w$} |", escaped, w = w));
            }
            out.push('\n');
        }
        out
    }

    /// Render as an HTML `<table>`.
    pub fn to_html_table(&self) -> String {
        let mut out = String::from("<table>\n");
        if !self.columns.is_empty() {
            out.push_str("<thead><tr>");
            for h in &self.columns {
                out.push_str(&format!("<th>{}</th>", html_escape(h)));
            }
            out.push_str("</tr></thead>\n");
        }
        out.push_str("<tbody>\n");
        for row in &self.rows {
            out.push_str("<tr>");
            for (i, _) in self.columns.iter().enumerate() {
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                out.push_str(&format!("<td>{}</td>", html_escape(cell)));
            }
            out.push_str("</tr>\n");
        }
        out.push_str("</tbody>\n</table>\n");
        out
    }

    /// Render as a Unicode box-drawing table. Cells > 60 chars are truncated.
    pub fn to_table(&self) -> String {
        const MAX_CELL: usize = 60;

        if self.columns.is_empty() {
            return "(no columns)\n".to_string();
        }
        if self.rows.is_empty() {
            return "(0 rows)\n".to_string();
        }

        let mut widths: Vec<usize> = self.columns.iter().map(|h| h.len()).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < widths.len() {
                    let display_len = cell.replace('\r', "").replace('\n', " ").chars().count();
                    widths[i] = widths[i].max(display_len.min(MAX_CELL));
                }
            }
        }

        let col_count = self.columns.len();
        let mut out = String::new();

        out.push('┌');
        for (i, &w) in widths.iter().enumerate() {
            out.push_str(&"─".repeat(w + 2));
            out.push(if i + 1 < col_count { '┬' } else { '┐' });
        }
        out.push('\n');

        out.push('│');
        for (i, h) in self.columns.iter().enumerate() {
            out.push_str(&format!(" {:<width$} │", h, width = widths[i]));
        }
        out.push('\n');

        out.push('├');
        for (i, &w) in widths.iter().enumerate() {
            out.push_str(&"─".repeat(w + 2));
            out.push(if i + 1 < col_count { '┼' } else { '┤' });
        }
        out.push('\n');

        for row in &self.rows {
            out.push('│');
            for (i, &w) in widths.iter().enumerate() {
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                let cell = cell.replace('\r', "").replace('\n', " ");
                let truncated: String = if cell.chars().count() > MAX_CELL {
                    let mut s: String = cell.chars().take(MAX_CELL - 1).collect();
                    s.push('…');
                    s
                } else {
                    cell
                };
                out.push_str(&format!(" {:<width$} │", truncated, width = w));
            }
            out.push('\n');
        }

        out.push('└');
        for (i, &w) in widths.iter().enumerate() {
            out.push_str(&"─".repeat(w + 2));
            out.push(if i + 1 < col_count { '┴' } else { '┘' });
        }
        out.push('\n');
        out.push_str(&format!(
            "({} row{})\n",
            self.rows.len(),
            if self.rows.len() == 1 { "" } else { "s" }
        ));
        out
    }
}

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
    let pairs: Vec<String> = props
        .iter()
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

fn block_to_row(doc_id: u32, block: &Block, block_idx: u32) -> Row {
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

fn doc_to_row(doc: &Document) -> Row {
    let tags_json = {
        let items: Vec<String> = doc
            .zone_maps
            .tags
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\\\"")))
            .collect();
        format!("[{}]", items.join(","))
    };
    Row {
        columns: vec!["id".into(), "path".into(), "title".into(), "tags".into()],
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
            Value::Str(tags_json),
        ],
    }
}

fn qualify_row(row: Row, prefix: &str) -> Row {
    Row {
        columns: row
            .columns
            .iter()
            .map(|c| format!("{}.{}", prefix, c))
            .collect(),
        values: row.values,
    }
}

fn cross_join(left: Vec<Row>, right: Vec<Row>) -> Vec<Row> {
    let mut out = Vec::with_capacity(left.len() * right.len());
    for l in &left {
        for r in &right {
            let mut cols = l.columns.clone();
            cols.extend(r.columns.iter().cloned());
            let mut vals = l.values.clone();
            vals.extend(r.values.iter().cloned());
            out.push(Row {
                columns: cols,
                values: vals,
            });
        }
    }
    out
}

/// Equi-join fast path: hashes `right` by `right_key_expr` and probes it with
/// `left_key_expr` per left row instead of the full `left * right` cross
/// product. `full_predicate` is still checked per candidate pair, so results
/// match `cross_join` + `.retain(full_predicate)` exactly.
fn hash_equi_join(
    left: Vec<Row>,
    right: Vec<Row>,
    left_key_expr: &Expr,
    right_key_expr: &Expr,
    full_predicate: &Expr,
) -> Vec<Row> {
    let mut buckets: FxHashMap<JoinKey, Vec<usize>> = FxHashMap::default();
    for (i, r) in right.iter().enumerate() {
        if let Some(key) = value_join_key(&eval_expr(right_key_expr, r)) {
            buckets.entry(key).or_default().push(i);
        }
    }

    let mut out = Vec::new();
    for l in &left {
        let Some(key) = value_join_key(&eval_expr(left_key_expr, l)) else {
            continue;
        };
        let Some(candidates) = buckets.get(&key) else {
            continue;
        };
        for &i in candidates {
            let r = &right[i];
            let mut cols = l.columns.clone();
            cols.extend(r.columns.iter().cloned());
            let mut vals = l.values.clone();
            vals.extend(r.values.iter().cloned());
            let combined = Row {
                columns: cols,
                values: vals,
            };
            if eval_expr(full_predicate, &combined).is_truthy() {
                out.push(combined);
            }
        }
    }
    out
}

fn eval_sql_value(v: &SqlValue) -> Value {
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

fn ident_value(part: &ObjectNamePart) -> &str {
    match part {
        ObjectNamePart::Identifier(i) => &i.value,
        ObjectNamePart::Function(_) => "",
    }
}

fn eval_expr(expr: &Expr, row: &Row) -> Value {
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
        BinaryOperator::Eq => Value::Bool(eval_expr(left, row) == eval_expr(right, row)),
        BinaryOperator::NotEq => Value::Bool(eval_expr(left, row) != eval_expr(right, row)),
        BinaryOperator::Lt => cmp_op(left, right, row, |o| o.is_lt()),
        BinaryOperator::LtEq => cmp_op(left, right, row, |o| o.is_le()),
        BinaryOperator::Gt => cmp_op(left, right, row, |o| o.is_gt()),
        BinaryOperator::GtEq => cmp_op(left, right, row, |o| o.is_ge()),
        BinaryOperator::Plus => arith_op(left, right, row, |a, b| a + b, |a, b| a + b),
        BinaryOperator::Minus => arith_op(left, right, row, |a, b| a - b, |a, b| a - b),
        BinaryOperator::Multiply => arith_op(left, right, row, |a, b| a * b, |a, b| a * b),
        BinaryOperator::Divide => {
            let (l, r) = (eval_expr(left, row), eval_expr(right, row));
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

fn arith_op(
    l: &Expr,
    r: &Expr,
    row: &Row,
    int_f: impl Fn(i64, i64) -> i64,
    flt_f: impl Fn(f64, f64) -> f64,
) -> Value {
    let (lv, rv) = (eval_expr(l, row), eval_expr(r, row));
    match (&lv, &rv) {
        (Value::Int(a), Value::Int(b)) => Value::Int(int_f(*a, *b)),
        _ => match (lv.as_f64(), rv.as_f64()) {
            (Some(a), Some(b)) => Value::Float(flt_f(a, b)),
            _ => Value::Null,
        },
    }
}

fn eval_function_call(f: &Function, row: &Row) -> Value {
    let name = f.name.0.last().map(ident_value).unwrap_or("");
    // Aggregates return placeholder; resolved later
    if is_aggregate_name(&name.to_lowercase()) {
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
    eval_scalar_function(name, &args)
}

fn eval_scalar_function(name: &str, args: &[Value]) -> Value {
    match name.to_lowercase().as_str() {
        "under" => {
            if args.len() < 4 {
                return Value::Bool(false);
            }
            let (pre, post) = (args[0].as_i64().unwrap_or(0), args[1].as_i64().unwrap_or(0));
            let (ap, aq) = (args[2].as_i64().unwrap_or(0), args[3].as_i64().unwrap_or(0));
            Value::Bool(pre > ap && post < aq)
        }
        "json_extract" => {
            if args.len() < 2 {
                return Value::Null;
            }
            let json = args[0].as_str().unwrap_or("");
            let path = args[1].as_str().unwrap_or("");
            let key = path.trim_start_matches("$.").trim_matches('"');
            extract_json_key(json, key)
        }
        "mq" => {
            if args.len() < 2 {
                return Value::Null;
            }
            let program = match args[0].as_str() {
                Some(s) => s.to_string(),
                None => return Value::Null,
            };
            let content = match args[1].as_str() {
                Some(s) => s.to_string(),
                None => return Value::Null,
            };
            eval_mq_scalar(&program, &content)
        }
        "match" => {
            let (Some(content), Some(query)) = (
                args.first().and_then(Value::as_str),
                args.get(1).and_then(Value::as_str),
            ) else {
                return Value::Bool(false);
            };
            let content_terms: std::collections::HashSet<String> =
                tokenize(content).into_iter().collect();
            let query_terms = tokenize(query);
            Value::Bool(
                !query_terms.is_empty() && query_terms.iter().all(|t| content_terms.contains(t)),
            )
        }
        "score" => {
            let (Some(content), Some(query)) = (
                args.first().and_then(Value::as_str),
                args.get(1).and_then(Value::as_str),
            ) else {
                return Value::Float(0.0);
            };
            let content_terms = tokenize(content);
            let query_terms = tokenize(query);
            if content_terms.is_empty() || query_terms.is_empty() {
                return Value::Float(0.0);
            }
            // Simple term-frequency score, normalised by content length —
            // deliberately not BM25 (no IDF/corpus-wide stats): `eval_expr`
            // only ever sees one `Row` at a time with no back-reference to
            // the corpus, so a real IDF term would need a much larger
            // signature change (see `TermIndex`'s doc comment for the same
            // constraint on the index side). Good enough to rank matches
            // within a single query; a document that repeats a common word
            // many times can outrank one with a rarer, more specific match.
            let mut freq: FxHashMap<&str, u32> = FxHashMap::default();
            for t in &content_terms {
                *freq.entry(t.as_str()).or_default() += 1;
            }
            let hits: f64 = query_terms
                .iter()
                .map(|q| *freq.get(q.as_str()).unwrap_or(&0) as f64)
                .sum();
            Value::Float(hits / content_terms.len() as f64)
        }

        "lower" => str_fn(args, |s| s.to_lowercase()),
        "upper" => str_fn(args, |s| s.to_uppercase()),
        "length" | "len" | "char_length" | "character_length" => args
            .first()
            .and_then(|v| v.as_str())
            .map(|s| Value::Int(s.chars().count() as i64))
            .unwrap_or(Value::Null),
        "trim" => str_fn(args, |s| s.trim().to_string()),
        "ltrim" => {
            let chars = trim_char_set(args, 1);
            str_fn(args, |s| {
                s.trim_start_matches(|c| chars.contains(&c)).to_string()
            })
        }
        "rtrim" => {
            let chars = trim_char_set(args, 1);
            str_fn(args, |s| {
                s.trim_end_matches(|c| chars.contains(&c)).to_string()
            })
        }
        "concat" => Value::Str(
            args.iter()
                .map(|v| v.display())
                .collect::<Vec<_>>()
                .join(""),
        ),
        "concat_ws" => {
            let sep = match args.first().and_then(|v| v.as_str()) {
                Some(s) => s,
                None => return Value::Null,
            };
            Value::Str(
                args[1..]
                    .iter()
                    .filter(|v| !matches!(v, Value::Null))
                    .map(|v| v.display())
                    .collect::<Vec<_>>()
                    .join(sep),
            )
        }
        "replace" => {
            if args.len() < 3 {
                return Value::Null;
            }
            match (args[0].as_str(), args[1].as_str(), args[2].as_str()) {
                (Some(s), Some(from), Some(to)) => Value::Str(s.replace(from, to)),
                _ => Value::Null,
            }
        }
        "left" => str_int_fn(args, |chars, n| {
            chars[..(n.max(0) as usize).min(chars.len())]
                .iter()
                .collect()
        }),
        "right" => str_int_fn(args, |chars, n| {
            let n = (n.max(0) as usize).min(chars.len());
            chars[chars.len() - n..].iter().collect()
        }),
        "lpad" => pad_fn(args, true),
        "rpad" => pad_fn(args, false),
        "reverse" => str_fn(args, |s| s.chars().rev().collect()),
        "repeat" => {
            if args.len() < 2 {
                return Value::Null;
            }
            match (args[0].as_str(), args[1].as_i64()) {
                (Some(s), Some(n)) => Value::Str(s.repeat(n.max(0) as usize)),
                _ => Value::Null,
            }
        }
        "initcap" => str_fn(args, |s| {
            s.split(' ')
                .map(|word| {
                    let mut c = word.chars();
                    match c.next() {
                        Some(first) => {
                            first.to_uppercase().collect::<String>() + &c.as_str().to_lowercase()
                        }
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        }),
        "ascii" => args
            .first()
            .and_then(|v| v.as_str())
            .and_then(|s| s.chars().next())
            .map(|c| Value::Int(c as i64))
            .unwrap_or(Value::Null),
        "chr" => args
            .first()
            .and_then(|v| v.as_i64())
            .and_then(|n| u32::try_from(n).ok())
            .and_then(char::from_u32)
            .map(|c| Value::Str(c.to_string()))
            .unwrap_or(Value::Null),
        "instr" => {
            if args.len() < 2 {
                return Value::Null;
            }
            match (args[0].as_str(), args[1].as_str()) {
                (Some(haystack), Some(needle)) => {
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
        "split_part" => {
            if args.len() < 3 {
                return Value::Null;
            }
            match (args[0].as_str(), args[1].as_str(), args[2].as_i64()) {
                (Some(s), Some(delim), Some(n)) if n > 0 => s
                    .split(delim)
                    .nth((n - 1) as usize)
                    .map(|p| Value::Str(p.to_string()))
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            }
        }

        "abs" => num_fn(args, |n| n.abs(), |n| n.abs()),
        "round" => {
            let n = match args.first().and_then(|v| v.as_f64()) {
                Some(n) => n,
                None => return Value::Null,
            };
            let scale = args.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
            let factor = 10f64.powi(scale as i32);
            let result = (n * factor).round() / factor;
            if scale <= 0 {
                Value::Int(result as i64)
            } else {
                Value::Float(result)
            }
        }
        "ceil" | "ceiling" => float_fn(args, |n| n.ceil()),
        "floor" => float_fn(args, |n| n.floor()),
        "trunc" | "truncate" => {
            let n = match args.first().and_then(|v| v.as_f64()) {
                Some(n) => n,
                None => return Value::Null,
            };
            let scale = args.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
            let factor = 10f64.powi(scale as i32);
            let result = (n * factor).trunc() / factor;
            if scale <= 0 {
                Value::Int(result as i64)
            } else {
                Value::Float(result)
            }
        }
        "mod" => {
            if args.len() < 2 {
                return Value::Null;
            }
            match (&args[0], &args[1]) {
                (Value::Int(a), Value::Int(b)) if *b != 0 => Value::Int(a % b),
                _ => match (args[0].as_f64(), args[1].as_f64()) {
                    (Some(a), Some(b)) if b != 0.0 => Value::Float(a % b),
                    _ => Value::Null,
                },
            }
        }
        "power" | "pow" => {
            if args.len() < 2 {
                return Value::Null;
            }
            match (args[0].as_f64(), args[1].as_f64()) {
                (Some(a), Some(b)) => Value::Float(a.powf(b)),
                _ => Value::Null,
            }
        }
        "sqrt" => float_fn(args, |n| n.sqrt()),
        "sign" => float_fn(args, |n| {
            if n > 0.0 {
                1.0
            } else if n < 0.0 {
                -1.0
            } else {
                0.0
            }
        }),
        "exp" => float_fn(args, |n| n.exp()),
        "ln" => float_fn(args, |n| n.ln()),
        "log10" => float_fn(args, |n| n.log10()),
        "log2" => float_fn(args, |n| n.log2()),
        "log" => {
            let n = match args.first().and_then(|v| v.as_f64()) {
                Some(n) => n,
                None => return Value::Null,
            };
            match args.get(1).and_then(|v| v.as_f64()) {
                Some(base) => Value::Float(n.log(base)),
                None => Value::Float(n.log10()),
            }
        }
        "pi" => Value::Float(std::f64::consts::PI),
        "greatest" => args
            .iter()
            .filter(|v| !matches!(v, Value::Null))
            .cloned()
            .max_by(|a, b| a.cmp_val(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(Value::Null),
        "least" => args
            .iter()
            .filter(|v| !matches!(v, Value::Null))
            .cloned()
            .min_by(|a, b| a.cmp_val(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(Value::Null),

        "coalesce" | "ifnull" => args
            .iter()
            .find(|v| !matches!(v, Value::Null))
            .cloned()
            .unwrap_or(Value::Null),
        "nullif" => {
            if args.len() < 2 {
                return Value::Null;
            }
            if args[0] == args[1] {
                Value::Null
            } else {
                args[0].clone()
            }
        }

        "typeof" => Value::Str(
            match args.first() {
                Some(Value::Str(_)) => "text",
                Some(Value::Int(_)) => "integer",
                Some(Value::Float(_)) => "float",
                Some(Value::Bool(_)) => "boolean",
                Some(Value::Null) | None => "null",
            }
            .to_string(),
        ),
        "now" | "current_timestamp" => Value::Str(current_datetime_utc(true, true)),
        "current_date" => Value::Str(current_datetime_utc(true, false)),
        "current_time" => Value::Str(current_datetime_utc(false, true)),
        _ => Value::Null,
    }
}

fn str_fn(args: &[Value], f: impl Fn(&str) -> String) -> Value {
    args.first()
        .and_then(|v| v.as_str())
        .map(|s| Value::Str(f(s)))
        .unwrap_or(Value::Null)
}

fn str_int_fn(args: &[Value], f: impl Fn(&[char], i64) -> String) -> Value {
    if args.len() < 2 {
        return Value::Null;
    }
    match (args[0].as_str(), args[1].as_i64()) {
        (Some(s), Some(n)) => {
            let chars: Vec<char> = s.chars().collect();
            Value::Str(f(&chars, n))
        }
        _ => Value::Null,
    }
}

fn num_fn(args: &[Value], int_f: impl Fn(i64) -> i64, flt_f: impl Fn(f64) -> f64) -> Value {
    match args.first() {
        Some(Value::Int(n)) => Value::Int(int_f(*n)),
        Some(v) => v
            .as_f64()
            .map(|n| Value::Float(flt_f(n)))
            .unwrap_or(Value::Null),
        None => Value::Null,
    }
}

fn float_fn(args: &[Value], f: impl Fn(f64) -> f64) -> Value {
    args.first()
        .and_then(|v| v.as_f64())
        .map(|n| Value::Float(f(n)))
        .unwrap_or(Value::Null)
}

/// Builds the set of characters TRIM/LTRIM/RTRIM should strip, defaulting to
/// whitespace when no explicit character argument is given.
fn trim_char_set(args: &[Value], chars_idx: usize) -> Vec<char> {
    args.get(chars_idx)
        .and_then(|v| v.as_str())
        .map(|s| s.chars().collect())
        .unwrap_or_else(|| vec![' ', '\t', '\n', '\r'])
}

fn pad_fn(args: &[Value], left: bool) -> Value {
    if args.len() < 2 {
        return Value::Null;
    }
    let s = match args[0].as_str() {
        Some(s) => s,
        None => return Value::Null,
    };
    let target_len = match args[1].as_i64() {
        Some(n) => n.max(0) as usize,
        None => return Value::Null,
    };
    let pad_str = args.get(2).and_then(|v| v.as_str()).unwrap_or(" ");
    let mut chars: Vec<char> = s.chars().collect();
    if chars.len() >= target_len {
        chars.truncate(target_len);
        return Value::Str(chars.into_iter().collect());
    }
    if pad_str.is_empty() {
        return Value::Str(s.to_string());
    }
    let pad_chars: Vec<char> = pad_str.chars().collect();
    let needed = target_len - chars.len();
    let padding: Vec<char> = pad_chars.iter().cycle().take(needed).copied().collect();
    if left {
        Value::Str(padding.into_iter().chain(chars).collect())
    } else {
        chars.extend(padding);
        Value::Str(chars.into_iter().collect())
    }
}

/// Returns the current UTC time formatted for `now()`/`current_timestamp`/
/// `current_date`/`current_time`. No date columns exist in the schema, so
/// this only needs to support clock-style scalar lookups, not arithmetic.
fn current_datetime_utc(with_date: bool, with_time: bool) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86400) as i64;
    let time_of_day = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (
        time_of_day / 3600,
        (time_of_day / 60) % 60,
        time_of_day % 60,
    );
    match (with_date, with_time) {
        (true, true) => format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}"),
        (true, false) => format!("{y:04}-{m:02}-{d:02}"),
        _ => format!("{h:02}:{mi:02}:{s:02}"),
    }
}

/// Howard Hinnant's `civil_from_days` algorithm: converts a day count
/// since the Unix epoch (1970-01-01) into a proleptic-Gregorian (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn eval_mq_scalar(program: &str, content: &str) -> Value {
    let mut engine = DefaultEngine::default();
    engine.load_builtin_module();
    let input = match parse_markdown_input(content) {
        Ok(i) => i,
        Err(_) => return Value::Null,
    };
    match engine.eval(program, input.into_iter()) {
        Ok(output) => {
            let parts: Vec<String> = output
                .compact()
                .into_iter()
                .map(|v| v.to_string())
                .collect();
            if parts.is_empty() {
                Value::Null
            } else {
                Value::Str(parts.join("\n"))
            }
        }
        Err(_) => Value::Null,
    }
}

fn extract_json_key(json: &str, key: &str) -> Value {
    let s = json.trim();
    if !s.starts_with('{') {
        return Value::Null;
    }
    let target = format!("\"{}\":", key);
    if let Some(pos) = s.find(&target) {
        let after = s[pos + target.len()..].trim_start();
        if let Some(inner) = after.strip_prefix('"') {
            if let Some(end) = inner.find('"') {
                return Value::Str(inner[..end].to_string());
            }
        } else if let Some(end) = after.find([',', '}']) {
            let raw = after[..end].trim();
            if let Ok(n) = raw.parse::<i64>() {
                return Value::Int(n);
            }
            if let Ok(f) = raw.parse::<f64>() {
                return Value::Float(f);
            }
            if raw == "true" {
                return Value::Bool(true);
            }
            if raw == "false" {
                return Value::Bool(false);
            }
            if raw == "null" {
                return Value::Null;
            }
        }
    }
    Value::Null
}

// LIKE pattern matching (% = .*, _ = any char)
fn like_match_str(s: &str, pattern: &str) -> bool {
    let s: Vec<char> = s.to_lowercase().chars().collect();
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    like_dp(&s, &p, 0, 0)
}

fn like_dp(s: &[char], p: &[char], si: usize, pi: usize) -> bool {
    if pi == p.len() {
        return si == s.len();
    }
    if p[pi] == '%' {
        // skip consecutive %
        let mut npi = pi + 1;
        while npi < p.len() && p[npi] == '%' {
            npi += 1;
        }
        for k in si..=s.len() {
            if like_dp(s, p, k, npi) {
                return true;
            }
        }
        return false;
    }
    if si >= s.len() {
        return false;
    }
    let matches = p[pi] == '_' || p[pi] == s[si];
    matches && like_dp(s, p, si + 1, pi + 1)
}

/// Custom SQL execution engine backed by a [`DocumentStore`] reference.
///
/// Secondary indexes are built once on construction (O(n) in total block count)
/// and reused for every query. Commands that do not create a `SqlEngine`
/// (mq, list, show, stats …) pay no index-construction cost.
pub struct SqlEngine<'a> {
    store: &'a DocumentStore,
    /// One `DocumentIndex` per document, in the same order as `store.documents()`.
    indexes: Vec<DocumentIndex>,
    /// Stack of CTE scopes from `WITH` clauses, one frame per nested
    /// `exec_query` call. Looked up innermost-first so a nested subquery's
    /// own `WITH` shadows an outer CTE of the same name.
    cte_scopes: std::cell::RefCell<Vec<FxHashMap<String, std::rc::Rc<QueryOutput>>>>,
}

impl<'a> SqlEngine<'a> {
    /// Build the engine and its secondary indexes.
    ///
    /// Uses cached indexes from [`DocumentStore::load_all_indexes`] when
    /// available (O(1) per document); otherwise rebuilds from blocks (O(n)).
    pub fn new(store: &'a DocumentStore) -> Result<Self, MqdbError> {
        let indexes = store
            .documents()
            .iter()
            .enumerate()
            .map(|(i, doc)| {
                if let Some(idx) = store.get_doc_index(i) {
                    idx.clone()
                } else {
                    DocumentIndex::build(&doc.blocks)
                }
            })
            .collect();
        Ok(Self {
            store,
            indexes,
            cte_scopes: std::cell::RefCell::new(Vec::new()),
        })
    }

    fn documents_with_indexes(&self) -> impl Iterator<Item = (&Document, &DocumentIndex)> {
        self.store.documents().iter().zip(self.indexes.iter())
    }

    /// Execute a SQL statement against the store.
    ///
    /// Supports `SELECT`, `CREATE TABLE`, `INSERT INTO`, `DROP TABLE`,
    /// `DESC`/`DESCRIBE`, and `SHOW TABLES`.
    pub fn execute(&self, sql: &str) -> Result<QueryOutput, MqdbError> {
        // Pre-process non-standard commands (DESC / SHOW TABLES).
        let trimmed = sql.trim().trim_end_matches(';');
        let upper = trimmed.to_ascii_uppercase();
        if upper.starts_with("DESC ") || upper.starts_with("DESCRIBE ") {
            let name = trimmed
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_lowercase();
            return self.exec_desc(&name);
        }
        if upper == "SHOW TABLES" {
            return self.exec_show_tables();
        }

        let stmts = Parser::parse_sql(&GenericDialect {}, sql)
            .map_err(|e| MqdbError::SqlParse(e.to_string()))?;
        let stmt = stmts
            .into_iter()
            .next()
            .ok_or_else(|| MqdbError::SqlParse("empty query".into()))?;
        match stmt {
            Statement::Query(q) => self.exec_query(&q),
            Statement::CreateTable(ct) => self.exec_create_table(&ct),
            Statement::Insert(ins) => self.exec_insert(&ins),
            Statement::Drop {
                object_type: ObjectType::Table,
                names,
                if_exists,
                ..
            } => self.exec_drop_tables(&names, if_exists),
            _ => Err(MqdbError::SqlExec(
                "unsupported statement; supported: SELECT, CREATE TABLE, INSERT INTO, DROP TABLE, DESC, SHOW TABLES".into(),
            )),
        }
    }

    fn exec_desc(&self, table_name: &str) -> Result<QueryOutput, MqdbError> {
        let schema: Option<Vec<(&str, &str)>> = match table_name {
            "blocks" => Some(vec![
                ("id", "integer"),
                ("document_id", "integer"),
                ("block_type", "text"),
                ("content", "text"),
                ("pre", "integer"),
                ("post", "integer"),
                ("depth", "integer"),
                ("lang", "text"),
                ("properties", "text"),
            ]),
            "documents" => Some(vec![
                ("id", "integer"),
                ("path", "text"),
                ("title", "text"),
                ("tags", "text"),
            ]),
            _ => None,
        };
        if let Some(rows) = schema {
            return Ok(QueryOutput {
                columns: vec!["column".to_string(), "type".to_string()],
                rows: rows
                    .iter()
                    .map(|(c, t)| vec![c.to_string(), t.to_string()])
                    .collect(),
            });
        }
        let guard = self.store.custom_tables.read().unwrap();
        if let Some(state) = guard.get(table_name) {
            let rows = state
                .columns
                .iter()
                .map(|c| vec![c.clone(), "text".to_string()])
                .collect();
            return Ok(QueryOutput {
                columns: vec!["column".to_string(), "type".to_string()],
                rows,
            });
        }
        Err(MqdbError::SqlExec(format!("unknown table: {table_name}")))
    }

    fn exec_show_tables(&self) -> Result<QueryOutput, MqdbError> {
        let mut rows = vec![
            vec!["blocks".to_string(), "built-in".to_string()],
            vec!["documents".to_string(), "built-in".to_string()],
        ];
        let guard = self.store.custom_tables.read().unwrap();
        let mut custom: Vec<String> = guard.keys().cloned().collect();
        drop(guard);
        custom.sort();
        rows.extend(custom.into_iter().map(|n| vec![n, "custom".to_string()]));
        Ok(QueryOutput {
            columns: vec!["table".to_string(), "kind".to_string()],
            rows,
        })
    }

    fn exec_create_table(&self, ct: &CreateTable) -> Result<QueryOutput, MqdbError> {
        let table_name = ct
            .name
            .0
            .last()
            .map(ident_value)
            .unwrap_or("")
            .to_lowercase();
        if matches!(table_name.as_str(), "blocks" | "documents") {
            return Err(MqdbError::SqlExec(format!(
                "cannot override built-in table '{table_name}'"
            )));
        }

        if let Some(query) = &ct.query {
            // CREATE TABLE name AS SELECT ...
            let result = self.exec_query(query)?;
            let n = result.rows.len();
            self.store.custom_tables.write().unwrap().insert(
                table_name,
                CustomTableState {
                    columns: result.columns,
                    rows: result.rows,
                    first_row_page: 0,
                    last_row_page: 0,
                },
            );
            self.store.try_flush_catalog_to_storage();
            return Ok(QueryOutput {
                columns: vec!["rows".to_string()],
                rows: vec![vec![n.to_string()]],
            });
        }

        // CREATE TABLE name (col1 TYPE, ...)
        let columns: Vec<String> = ct.columns.iter().map(|c| c.name.value.clone()).collect();
        if columns.is_empty() {
            return Err(MqdbError::SqlExec(
                "CREATE TABLE requires at least one column or AS SELECT".into(),
            ));
        }
        let already_exists = self
            .store
            .custom_tables
            .read()
            .unwrap()
            .contains_key(&table_name);
        if already_exists {
            if ct.if_not_exists {
                return Ok(QueryOutput {
                    columns: vec!["result".to_string()],
                    rows: vec![vec!["already exists".to_string()]],
                });
            }
            return Err(MqdbError::SqlExec(format!(
                "table '{table_name}' already exists"
            )));
        }
        self.store.custom_tables.write().unwrap().insert(
            table_name,
            CustomTableState {
                columns,
                rows: vec![],
                first_row_page: 0,
                last_row_page: 0,
            },
        );
        self.store.try_flush_catalog_to_storage();
        Ok(QueryOutput {
            columns: vec!["result".to_string()],
            rows: vec![vec!["ok".to_string()]],
        })
    }

    fn exec_insert(&self, ins: &Insert) -> Result<QueryOutput, MqdbError> {
        let table_name = match &ins.table {
            TableObject::TableName(name) => {
                name.0.last().map(ident_value).unwrap_or("").to_lowercase()
            }
            _ => return Err(MqdbError::SqlExec("unsupported INSERT target".into())),
        };

        let source = ins
            .source
            .as_ref()
            .ok_or_else(|| MqdbError::SqlExec("INSERT requires VALUES or SELECT".into()))?;
        let values_out = self.exec_query(source)?;

        // Determine column mapping
        let col_indices: Option<Vec<usize>> = if ins.columns.is_empty() {
            None // positional
        } else {
            let guard = self.store.custom_tables.read().unwrap();
            let table_cols = guard
                .get(&table_name)
                .map(|state| state.columns.clone())
                .ok_or_else(|| MqdbError::SqlExec(format!("unknown table: {table_name}")))?;
            drop(guard);
            let indices: Result<Vec<usize>, _> = ins
                .columns
                .iter()
                .map(|col_name| {
                    let name = col_name.0.last().map(ident_value).unwrap_or("");
                    table_cols
                        .iter()
                        .position(|c| c.eq_ignore_ascii_case(name))
                        .ok_or_else(|| MqdbError::SqlExec(format!("unknown column '{name}'")))
                })
                .collect();
            Some(indices?)
        };

        let new_rows = {
            let mut guard = self.store.custom_tables.write().unwrap();
            let state = guard
                .get_mut(&table_name)
                .ok_or_else(|| MqdbError::SqlExec(format!("unknown table: {table_name}")))?;
            let ncols = state.columns.len();

            let mut new_rows = Vec::with_capacity(values_out.rows.len());
            for src_row in &values_out.rows {
                let mut row = vec![String::new(); ncols];
                match &col_indices {
                    None => {
                        if src_row.len() != ncols {
                            return Err(MqdbError::SqlExec(format!(
                                "expected {ncols} columns, got {}",
                                src_row.len()
                            )));
                        }
                        row = src_row.clone();
                    }
                    Some(idx_map) => {
                        for (dst_idx, &src_idx) in idx_map.iter().enumerate() {
                            if let Some(v) = src_row.get(dst_idx) {
                                row[src_idx] = v.clone();
                            }
                        }
                    }
                }
                state.rows.push(row.clone());
                new_rows.push(row);
            }
            new_rows
        }; // write lock released before flush
        let inserted = new_rows.len();
        // Append only the new rows to the on-disk chain instead of rewriting
        // the whole table, so INSERT cost stays proportional to the rows
        // being added rather than the table's total size.
        self.store
            .try_append_table_rows_to_storage(&table_name, &new_rows);
        Ok(QueryOutput {
            columns: vec!["rows_affected".to_string()],
            rows: vec![vec![inserted.to_string()]],
        })
    }

    fn exec_drop_tables(
        &self,
        names: &[ObjectName],
        if_exists: bool,
    ) -> Result<QueryOutput, MqdbError> {
        let dropped = {
            let mut guard = self.store.custom_tables.write().unwrap();
            let mut dropped = 0usize;
            for name in names {
                let table_name = name.0.last().map(ident_value).unwrap_or("").to_lowercase();
                if matches!(table_name.as_str(), "blocks" | "documents") {
                    return Err(MqdbError::SqlExec(format!(
                        "cannot drop built-in table '{table_name}'"
                    )));
                }
                if guard.remove(&table_name).is_some() {
                    dropped += 1;
                } else if !if_exists {
                    return Err(MqdbError::SqlExec(format!(
                        "table '{table_name}' does not exist"
                    )));
                }
            }
            dropped
        }; // write lock released before flush
        self.store.try_flush_catalog_to_storage();
        Ok(QueryOutput {
            columns: vec!["result".to_string()],
            rows: vec![vec![format!("{dropped} table(s) dropped")]],
        })
    }

    /// Materialises any `WITH` clause's CTEs into a new scope frame, then
    /// delegates to [`Self::exec_query_body`].
    fn exec_query(&self, query: &Query) -> Result<QueryOutput, MqdbError> {
        let Some(with) = &query.with else {
            return self.exec_query_body(query);
        };
        if with.recursive {
            return Err(MqdbError::SqlExec("WITH RECURSIVE is not supported".into()));
        }

        self.cte_scopes.borrow_mut().push(FxHashMap::default());
        let result = (|| {
            for cte in &with.cte_tables {
                if !cte.alias.columns.is_empty() {
                    return Err(MqdbError::SqlExec(
                        "CTE column aliases (WITH x(a, b) AS ...) are not supported".into(),
                    ));
                }
                let name = cte.alias.name.value.to_lowercase();
                // `name` isn't in scope yet, so no self-reference (WITH RECURSIVE only).
                let out = self.exec_query(&cte.query)?;
                self.cte_scopes
                    .borrow_mut()
                    .last_mut()
                    .expect("scope frame just pushed above")
                    .insert(name, std::rc::Rc::new(out));
            }
            self.exec_query_body(query)
        })();
        self.cte_scopes.borrow_mut().pop();
        result
    }

    fn exec_query_body(&self, query: &Query) -> Result<QueryOutput, MqdbError> {
        let select = match query.body.as_ref() {
            SetExpr::Select(s) => s,
            SetExpr::Values(Values { rows, .. }) => {
                let empty = Row {
                    columns: vec![],
                    values: vec![],
                };
                let out: Vec<Vec<String>> = rows
                    .iter()
                    .map(|row| row.iter().map(|e| eval_expr(e, &empty).display()).collect())
                    .collect();
                return Ok(QueryOutput {
                    columns: vec![],
                    rows: out,
                });
            }
            _ => return Err(MqdbError::SqlExec("unsupported query type".into())),
        };

        // 1. Materialise FROM — with index-based predicate pushdown
        let where_expr = select.selection.as_ref();
        let hint = where_expr
            .map(analyze_where_for_index)
            .unwrap_or(IndexHint::FullScan);
        // Unlike `hint`, a skip has no later row-by-row recheck, so only
        // allow it for a single un-joined FROM table (no alias ambiguity).
        let zone_filter =
            where_expr.filter(|_| select.from.len() == 1 && select.from[0].joins.is_empty());
        let mut rows = self.materialise_from_with_hint(&select.from, &hint, zone_filter)?;

        // 2. WHERE (full predicate evaluation; index only pre-filtered)
        if let Some(where_expr) = &select.selection {
            let resolved = self.resolve_subqueries(where_expr)?;
            rows.retain(|row| eval_expr(&resolved, row).is_truthy());
        }

        // 3. PROJECT / GROUP / ORDER / LIMIT
        let limit_expr = query.limit_clause.as_ref().and_then(|lc| match lc {
            LimitClause::LimitOffset { limit, .. } => limit.clone(),
            LimitClause::OffsetCommaLimit { limit, .. } => Some(limit.clone()),
        });

        self.project_and_aggregate(select, rows, &query.order_by, limit_expr.as_ref())
    }

    fn resolve_subqueries(&self, expr: &Expr) -> Result<Expr, MqdbError> {
        match expr {
            Expr::BinaryOp { left, op, right } => Ok(Expr::BinaryOp {
                left: Box::new(self.resolve_subqueries(left)?),
                op: op.clone(),
                right: Box::new(self.resolve_subqueries(right)?),
            }),
            Expr::Subquery(q) => {
                let out = self.exec_query(q)?;
                let val = out
                    .rows
                    .first()
                    .and_then(|r| r.first())
                    .map(|s| {
                        if let Ok(n) = s.parse::<i64>() {
                            Expr::Value(SqlValue::Number(n.to_string(), false).with_empty_span())
                        } else {
                            Expr::Value(SqlValue::SingleQuotedString(s.clone()).with_empty_span())
                        }
                    })
                    .unwrap_or(Expr::Value(SqlValue::Null.with_empty_span()));
                Ok(val)
            }
            Expr::Nested(inner) => Ok(Expr::Nested(Box::new(self.resolve_subqueries(inner)?))),
            Expr::Function(f) => {
                let new_args = match &f.args {
                    FunctionArguments::List(al) => {
                        let resolved: Result<Vec<_>, _> = al
                            .args
                            .iter()
                            .map(|a| match a {
                                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                                    Ok::<FunctionArg, MqdbError>(FunctionArg::Unnamed(
                                        FunctionArgExpr::Expr(self.resolve_subqueries(e)?),
                                    ))
                                }
                                _ => Ok(a.clone()),
                            })
                            .collect();
                        FunctionArguments::List(sqlparser::ast::FunctionArgumentList {
                            args: resolved?,
                            ..al.clone()
                        })
                    }
                    other => other.clone(),
                };
                Ok(Expr::Function(Function {
                    args: new_args,
                    ..f.clone()
                }))
            }
            other => Ok(other.clone()),
        }
    }

    fn materialise_from_with_hint(
        &self,
        from: &[sqlparser::ast::TableWithJoins],
        hint: &IndexHint,
        zone_filter: Option<&Expr>,
    ) -> Result<Vec<Row>, MqdbError> {
        if from.is_empty() {
            return Ok(vec![Row {
                columns: vec![],
                values: vec![],
            }]);
        }
        let mut rows = self.table_rows_with_hint(&from[0].relation, hint, zone_filter)?;
        for join in &from[0].joins {
            // Joined tables always full-scan (join partner)
            let right = self.table_rows_with_hint(&join.relation, &IndexHint::FullScan, None)?;
            match &join.join_operator {
                JoinOperator::Inner(JoinConstraint::On(on))
                | JoinOperator::Join(JoinConstraint::On(on))
                | JoinOperator::Left(JoinConstraint::On(on))
                | JoinOperator::LeftOuter(JoinConstraint::On(on)) => {
                    let resolved = self.resolve_subqueries(on)?;
                    let left_cols = rows.first().map(|r| r.columns.clone()).unwrap_or_default();
                    let right_cols = right.first().map(|r| r.columns.clone()).unwrap_or_default();
                    rows = match find_equi_join_exprs(&resolved, &left_cols, &right_cols) {
                        Some((left_key, right_key)) => {
                            hash_equi_join(rows, right, left_key, right_key, &resolved)
                        }
                        None => {
                            let mut combined = cross_join(rows, right);
                            combined.retain(|row| eval_expr(&resolved, row).is_truthy());
                            combined
                        }
                    };
                }
                _ => {
                    rows = cross_join(rows, right);
                }
            }
        }
        for twj in from.iter().skip(1) {
            let right = self.table_rows_with_hint(&twj.relation, &IndexHint::FullScan, None)?;
            rows = cross_join(rows, right);
            for join in &twj.joins {
                let right2 =
                    self.table_rows_with_hint(&join.relation, &IndexHint::FullScan, None)?;
                rows = cross_join(rows, right2);
            }
        }
        Ok(rows)
    }

    fn table_rows_with_hint(
        &self,
        factor: &TableFactor,
        hint: &IndexHint,
        zone_filter: Option<&Expr>,
    ) -> Result<Vec<Row>, MqdbError> {
        let (table_name, alias) = match factor {
            TableFactor::Table { name, alias, .. } => {
                let n = name.0.last().map(ident_value).unwrap_or("").to_lowercase();
                let a = alias.as_ref().map(|a| a.name.value.clone());
                (n, a)
            }
            _ => return Err(MqdbError::SqlExec("unsupported FROM clause".into())),
        };

        // A `WITH x AS (...)` shadows a real table named `x`; search
        // innermost-to-outermost so nested `WITH`s shadow outer ones.
        for scope in self.cte_scopes.borrow().iter().rev() {
            if let Some(out) = scope.get(&table_name) {
                let prefix = alias.as_deref().unwrap_or(&table_name);
                return Ok(out
                    .rows
                    .iter()
                    .map(|r| {
                        qualify_row(
                            Row {
                                columns: out.columns.clone(),
                                values: r.iter().map(|v| Value::Str(v.clone())).collect(),
                            },
                            prefix,
                        )
                    })
                    .collect());
            }
        }

        match table_name.as_str() {
            "blocks" => {
                let prefix = alias.as_deref().unwrap_or("blocks");
                let mut rows = Vec::new();
                let mut global_idx: u32 = 0;

                for (doc, doc_idx) in self.documents_with_indexes() {
                    // Zone-map document skip: prove no block in this document
                    // can match before reading any of them.
                    if let Some(we) = zone_filter
                        && zone_map_skip(&doc.zone_maps, we)
                    {
                        global_idx += doc.blocks.len() as u32;
                        continue;
                    }
                    // Try index-based access first
                    if let Some(local_indices) = hint.resolve(doc_idx) {
                        // Only materialise the pre-filtered blocks
                        for local_i in local_indices {
                            if let Some(block) = doc.blocks.get(local_i as usize) {
                                let block_global_idx = global_idx + local_i;
                                rows.push(qualify_row(
                                    block_to_row(doc.id, block, block_global_idx),
                                    prefix,
                                ));
                            }
                        }
                    } else {
                        // FullScan
                        for (i, block) in doc.blocks.iter().enumerate() {
                            rows.push(qualify_row(
                                block_to_row(doc.id, block, global_idx + i as u32),
                                prefix,
                            ));
                        }
                    }
                    global_idx += doc.blocks.len() as u32;
                }
                Ok(rows)
            }
            "documents" => {
                let prefix = alias.as_deref().unwrap_or("documents");
                Ok(self
                    .store
                    .documents()
                    .iter()
                    .map(|doc| qualify_row(doc_to_row(doc), prefix))
                    .collect())
            }
            other => {
                let guard = self.store.custom_tables.read().unwrap();
                if let Some(state) = guard.get(other) {
                    let prefix = alias.as_deref().unwrap_or(other);
                    let rows = state
                        .rows
                        .iter()
                        .map(|row_vals| {
                            qualify_row(
                                Row {
                                    columns: state.columns.clone(),
                                    values: row_vals
                                        .iter()
                                        .map(|v| Value::Str(v.clone()))
                                        .collect(),
                                },
                                prefix,
                            )
                        })
                        .collect();
                    return Ok(rows);
                }
                drop(guard);
                Err(MqdbError::SqlExec(format!("unknown table: {other}")))
            }
        }
    }

    fn project_and_aggregate(
        &self,
        select: &Select,
        rows: Vec<Row>,
        order_by: &Option<sqlparser::ast::OrderBy>,
        limit: Option<&Expr>,
    ) -> Result<QueryOutput, MqdbError> {
        let group_by_exprs: Vec<Expr> = match &select.group_by {
            GroupByExpr::Expressions(exprs, _) => exprs.clone(),
            _ => vec![],
        };
        let is_agg = has_aggregate(&select.projection);

        if is_agg || !group_by_exprs.is_empty() {
            return self.aggregate(select, rows, limit, &group_by_exprs);
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

        // ORDER BY
        if let Some(ob) = order_by {
            apply_order_by(&mut result, &ob.kind);
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
            rows: apply_limit(result, limit),
        })
    }

    fn aggregate(
        &self,
        select: &Select,
        rows: Vec<Row>,
        limit: Option<&Expr>,
        group_by_exprs: &[Expr],
    ) -> Result<QueryOutput, MqdbError> {
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
            return Ok(QueryOutput {
                columns,
                rows: apply_limit(vec![out_row], limit),
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

        let out_rows: Vec<Vec<String>> = groups
            .iter()
            .map(|(key_vals, group_rows)| {
                eval_agg_row(&select.projection, group_by_exprs, key_vals, group_rows)
            })
            .collect();

        Ok(QueryOutput {
            columns,
            rows: apply_limit(out_rows, limit),
        })
    }
}

/// A single matched `blocks` row targeted by `UPDATE`/`DELETE`, identified
/// by `(document_id, pre)` — `pre` is a unique per-document DFS number, so
/// this is stable even though the SQL-visible `id` column is a store-wide
/// running index that doesn't correspond to any field on [`Block`].
struct MatchedBlockEdit {
    document_id: u32,
    pre: u32,
    /// `Some(rendered content)` for `UPDATE`, `None` for `DELETE`.
    new_content: Option<String>,
}

fn single_table_name(twj: &TableWithJoins) -> Result<String, MqdbError> {
    if !twj.joins.is_empty() {
        return Err(MqdbError::SqlExec(
            "UPDATE/DELETE with write-back do not support joins".into(),
        ));
    }
    match &twj.relation {
        TableFactor::Table { name, .. } => {
            Ok(name.0.last().map(ident_value).unwrap_or("").to_lowercase())
        }
        _ => Err(MqdbError::SqlExec(
            "unsupported UPDATE/DELETE target".into(),
        )),
    }
}

/// Materialises the rows matched by `target`/`selection`, optionally
/// evaluating `set_value` (the `UPDATE ... SET content = <expr>` value,
/// per matched row) into `MatchedBlockEdit`s. `set_value` is `None` for
/// `DELETE`.
fn collect_matched_edits(
    store: &DocumentStore,
    target: &TableWithJoins,
    selection: Option<&Expr>,
    set_value: Option<&Expr>,
) -> Result<Vec<MatchedBlockEdit>, MqdbError> {
    let table_name = single_table_name(target)?;
    if table_name != "blocks" {
        return Err(MqdbError::SqlExec(format!(
            "UPDATE/DELETE with write-back is only supported on 'blocks' (got '{table_name}')"
        )));
    }

    let engine = SqlEngine::new(store)?;
    let mut rows = engine.materialise_from_with_hint(
        std::slice::from_ref(target),
        &IndexHint::FullScan,
        None,
    )?;
    if let Some(sel) = selection {
        let resolved = engine.resolve_subqueries(sel)?;
        rows.retain(|row| eval_expr(&resolved, row).is_truthy());
    }

    rows.iter()
        .map(|row| {
            let document_id = row
                .get("document_id")
                .and_then(Value::as_i64)
                .ok_or_else(|| MqdbError::SqlExec("matched row missing document_id".into()))?
                as u32;
            let pre = row
                .get("pre")
                .and_then(Value::as_i64)
                .ok_or_else(|| MqdbError::SqlExec("matched row missing pre".into()))?
                as u32;
            let new_content = set_value.map(|expr| eval_expr(expr, row).display());
            Ok(MatchedBlockEdit {
                document_id,
                pre,
                new_content,
            })
        })
        .collect()
}

/// Renders Markdown source text for a `Heading`/`Paragraph` block. Shared by
/// `UPDATE`/`INSERT INTO blocks` write-back. Other block types (tables,
/// code, lists, ...) aren't supported.
fn render_markdown_for(
    block_type: &BlockType,
    depth: Option<u8>,
    content: &str,
) -> Result<String, MqdbError> {
    match block_type {
        BlockType::Heading => Ok(format!(
            "{} {}",
            "#".repeat(depth.unwrap_or(1).max(1) as usize),
            content
        )),
        BlockType::Paragraph => Ok(content.to_string()),
        other => Err(MqdbError::SqlExec(format!(
            "write-back is only supported for heading/paragraph blocks (found {})",
            other.as_str()
        ))),
    }
}

/// Renders `edit`'s replacement text for an existing matched block.
fn render_replacement(block: &Block, new_content: &str) -> Result<String, MqdbError> {
    render_markdown_for(&block.block_type, block.heading_depth(), new_content)
}

/// Applies `edits` (grouped by document) as a source-text patch + reparse:
/// for each affected document, reads the *current* file off disk, splices
/// in the rendered replacement (or removes the lines entirely for a
/// `DELETE`) at each matched block's `Span`, writes the patched text back to
/// the file, then calls [`DocumentStore::replace_document`] to re-parse it
/// in place (same `DocumentId`, fresh blocks/index/catalog entry).
///
/// Returns the number of blocks affected.
fn apply_matched_edits(
    store: &mut DocumentStore,
    edits: Vec<MatchedBlockEdit>,
) -> Result<usize, MqdbError> {
    let mut by_doc: FxHashMap<u32, Vec<MatchedBlockEdit>> = FxHashMap::default();
    for edit in edits {
        by_doc.entry(edit.document_id).or_default().push(edit);
    }

    let mut affected = 0usize;
    for (doc_id, doc_edits) in by_doc {
        struct LineEdit {
            start_line: usize,
            end_line: usize,
            replacement: Option<String>,
        }

        let (path, mut line_edits) = {
            let doc = store
                .get_document(doc_id)
                .ok_or_else(|| MqdbError::SqlExec(format!("no such document: {doc_id}")))?;
            let path = doc.path.clone().ok_or_else(|| {
                MqdbError::SqlExec(
                    "cannot write back: document has no source file (added via add_str)".into(),
                )
            })?;

            let mut line_edits = Vec::with_capacity(doc_edits.len());
            for edit in &doc_edits {
                let block = doc
                    .blocks
                    .iter()
                    .find(|b| b.pre == edit.pre)
                    .ok_or_else(|| MqdbError::SqlExec("matched block no longer exists".into()))?;
                let span = block.span.as_ref().ok_or_else(|| {
                    MqdbError::SqlExec(
                        "write-back requires source spans; reindex without --no-spans".into(),
                    )
                })?;
                let replacement = edit
                    .new_content
                    .as_deref()
                    .map(|c| render_replacement(block, c))
                    .transpose()?;
                line_edits.push(LineEdit {
                    start_line: span.start_line,
                    end_line: span.end_line,
                    replacement,
                });
            }
            (path, line_edits)
        };

        let original = std::fs::read_to_string(&path)?;
        let had_trailing_newline = original.ends_with('\n');
        let mut lines: Vec<String> = original.lines().map(str::to_string).collect();

        // Apply from the bottom up so earlier edits don't shift later
        // (already-resolved) line numbers.
        line_edits.sort_by_key(|edit| std::cmp::Reverse(edit.start_line));
        for edit in &line_edits {
            let start = edit.start_line.saturating_sub(1);
            let end = edit.end_line.min(lines.len());
            if start >= end || start >= lines.len() {
                continue;
            }
            match &edit.replacement {
                Some(text) => {
                    lines.splice(start..end, std::iter::once(text.clone()));
                }
                None => {
                    let mut remove_start = start;
                    let mut remove_end = end;
                    if remove_end < lines.len() && lines[remove_end].trim().is_empty() {
                        // Blank line after (the common case: an interior or
                        // first block) — swallow it.
                        remove_end += 1;
                    } else if remove_start > 0 && lines[remove_start - 1].trim().is_empty() {
                        // No blank line after (block was the last one in the
                        // file) — swallow the blank line before it instead.
                        remove_start -= 1;
                    }
                    lines.splice(remove_start..remove_end, std::iter::empty());
                }
            }
        }

        let mut patched = lines.join("\n");
        if had_trailing_newline {
            patched.push('\n');
        }

        std::fs::write(&path, &patched)?;
        affected += doc_edits.len();
        store.replace_document(doc_id, &patched, Some(path))?;
    }

    Ok(affected)
}

/// A new block to insert via `INSERT INTO blocks (...) VALUES (...)`.
/// Mirrors [`MatchedBlockEdit`] but for insertion.
struct NewBlockSpec {
    document_id: u32,
    block_type: BlockType,
    content: String,
    /// Required (1-6) iff `block_type` is `Heading`.
    depth: Option<u8>,
    /// `pre` of the block to insert after; `None` appends at document end.
    after_pre: Option<u32>,
    /// Position within `VALUES`, to preserve order among same-anchor rows.
    row_index: usize,
}

const INSERT_BLOCKS_COLUMNS: [&str; 5] =
    ["document_id", "block_type", "content", "depth", "after_pre"];

/// Parses an `INSERT INTO blocks (...) VALUES (...)` statement into
/// [`NewBlockSpec`]s. Only an explicit column list and a literal `VALUES`
/// source are supported (no `INSERT ... SELECT`).
fn collect_new_blocks(ins: &Insert) -> Result<Vec<NewBlockSpec>, MqdbError> {
    if ins.columns.is_empty() {
        return Err(MqdbError::SqlExec(
            "write-back INSERT INTO blocks requires an explicit column list".into(),
        ));
    }
    let col_names: Vec<String> = ins
        .columns
        .iter()
        .map(|c| c.0.last().map(ident_value).unwrap_or("").to_lowercase())
        .collect();
    for name in &col_names {
        if !INSERT_BLOCKS_COLUMNS.contains(&name.as_str()) {
            return Err(MqdbError::SqlExec(format!(
                "write-back INSERT INTO blocks does not support column '{name}'"
            )));
        }
    }

    let source = ins
        .source
        .as_ref()
        .ok_or_else(|| MqdbError::SqlExec("INSERT requires VALUES".into()))?;
    let SetExpr::Values(Values { rows, .. }) = source.body.as_ref() else {
        return Err(MqdbError::SqlExec(
            "write-back INSERT INTO blocks only supports VALUES, not INSERT ... SELECT".into(),
        ));
    };

    let empty = Row {
        columns: vec![],
        values: vec![],
    };
    rows.iter()
        .enumerate()
        .map(|(row_index, row)| {
            if row.len() != col_names.len() {
                return Err(MqdbError::SqlExec(format!(
                    "expected {} values, got {}",
                    col_names.len(),
                    row.len()
                )));
            }

            let mut document_id: Option<i64> = None;
            let mut block_type: Option<BlockType> = None;
            let mut content: Option<String> = None;
            let mut depth: Option<u8> = None;
            let mut after_pre: Option<u32> = None;

            for (name, expr) in col_names.iter().zip(row.iter()) {
                let value = eval_expr(expr, &empty);
                match name.as_str() {
                    "document_id" => {
                        document_id = Some(value.as_i64().ok_or_else(|| {
                            MqdbError::SqlExec("document_id must be an integer".into())
                        })?);
                    }
                    "block_type" => {
                        let s = value.as_str().ok_or_else(|| {
                            MqdbError::SqlExec("block_type must be a string".into())
                        })?;
                        let bt = BlockType::from_str(&s.to_lowercase())
                            .filter(|bt| matches!(bt, BlockType::Heading | BlockType::Paragraph))
                            .ok_or_else(|| {
                                MqdbError::SqlExec(format!(
                                    "write-back is only supported for heading/paragraph blocks (found {s})"
                                ))
                            })?;
                        block_type = Some(bt);
                    }
                    "content" => {
                        content = match value {
                            Value::Null => None,
                            other => Some(other.display()),
                        };
                    }
                    "depth" => {
                        depth = match value {
                            Value::Null => None,
                            other => Some(other.as_i64().ok_or_else(|| {
                                MqdbError::SqlExec("depth must be an integer".into())
                            })? as u8),
                        };
                    }
                    "after_pre" => {
                        after_pre = match value {
                            Value::Null => None,
                            other => Some(other.as_i64().ok_or_else(|| {
                                MqdbError::SqlExec("after_pre must be an integer".into())
                            })? as u32),
                        };
                    }
                    _ => unreachable!("column names validated above"),
                }
            }

            let document_id = document_id
                .ok_or_else(|| MqdbError::SqlExec("INSERT INTO blocks requires document_id".into()))?
                as u32;
            let block_type = block_type
                .ok_or_else(|| MqdbError::SqlExec("INSERT INTO blocks requires block_type".into()))?;
            let content = content.ok_or_else(|| {
                MqdbError::SqlExec("INSERT INTO blocks requires non-NULL content".into())
            })?;

            match block_type {
                BlockType::Heading => match depth {
                    None => {
                        return Err(MqdbError::SqlExec(
                            "INSERT INTO blocks requires depth (1-6) for block_type 'heading'"
                                .into(),
                        ));
                    }
                    Some(d) if !(1..=6).contains(&d) => {
                        return Err(MqdbError::SqlExec(
                            "depth must be between 1 and 6 for block_type 'heading'".into(),
                        ));
                    }
                    Some(_) => {}
                },
                BlockType::Paragraph if depth.is_some() => {
                    return Err(MqdbError::SqlExec(
                        "depth is only valid for block_type 'heading'".into(),
                    ));
                }
                _ => {}
            }

            Ok(NewBlockSpec {
                document_id,
                block_type,
                content,
                depth,
                after_pre,
                row_index,
            })
        })
        .collect()
}

/// Applies `specs` (grouped by document) by splicing rendered Markdown text
/// into the source file at each spec's anchor position, then reparsing via
/// [`DocumentStore::replace_document`], same as [`apply_matched_edits`].
///
/// Returns the number of blocks inserted.
fn apply_new_blocks(
    store: &mut DocumentStore,
    specs: Vec<NewBlockSpec>,
) -> Result<usize, MqdbError> {
    let mut by_doc: FxHashMap<u32, Vec<NewBlockSpec>> = FxHashMap::default();
    for spec in specs {
        by_doc.entry(spec.document_id).or_default().push(spec);
    }

    let mut inserted = 0usize;
    for (doc_id, doc_specs) in by_doc {
        struct Insertion {
            /// 0-indexed line to insert before. `usize::MAX` means "end of
            /// file", resolved once the line count is known, below.
            at: usize,
            row_index: usize,
            rendered: String,
        }

        let (path, mut insertions) = {
            let doc = store
                .get_document(doc_id)
                .ok_or_else(|| MqdbError::SqlExec(format!("no such document: {doc_id}")))?;
            let path = doc.path.clone().ok_or_else(|| {
                MqdbError::SqlExec(
                    "cannot write back: document has no source file (added via add_str)".into(),
                )
            })?;

            let mut insertions = Vec::with_capacity(doc_specs.len());
            for spec in &doc_specs {
                let at = match spec.after_pre {
                    Some(pre) => {
                        let block = doc.blocks.iter().find(|b| b.pre == pre).ok_or_else(|| {
                            MqdbError::SqlExec(format!(
                                "after_pre {pre} does not match any block in document {doc_id}"
                            ))
                        })?;
                        let span = block.span.as_ref().ok_or_else(|| {
                            MqdbError::SqlExec(
                                "write-back requires source spans; reindex without --no-spans"
                                    .into(),
                            )
                        })?;
                        span.end_line
                    }
                    None => usize::MAX,
                };
                let rendered = render_markdown_for(&spec.block_type, spec.depth, &spec.content)?;
                insertions.push(Insertion {
                    at,
                    row_index: spec.row_index,
                    rendered,
                });
            }
            (path, insertions)
        };

        let original = std::fs::read_to_string(&path)?;
        let had_trailing_newline = original.ends_with('\n');
        let mut lines: Vec<String> = original.lines().map(str::to_string).collect();

        for insertion in &mut insertions {
            if insertion.at == usize::MAX {
                insertion.at = lines.len();
            }
        }

        // Bottom-up so earlier insertions don't shift later line numbers;
        // ties broken by declared VALUES order.
        insertions.sort_by_key(|ins| (std::cmp::Reverse(ins.at), std::cmp::Reverse(ins.row_index)));

        for insertion in &insertions {
            let at = insertion.at.min(lines.len());
            let needs_leading_blank = at > 0 && !lines[at - 1].trim().is_empty();
            let needs_trailing_blank = at < lines.len() && !lines[at].trim().is_empty();

            let mut new_lines = vec![insertion.rendered.clone()];
            if needs_trailing_blank {
                new_lines.push(String::new());
            }
            if needs_leading_blank {
                new_lines.insert(0, String::new());
            }
            lines.splice(at..at, new_lines);
        }

        let mut patched = lines.join("\n");
        if had_trailing_newline {
            patched.push('\n');
        }

        std::fs::write(&path, &patched)?;
        inserted += doc_specs.len();
        store.replace_document(doc_id, &patched, Some(path))?;
    }

    Ok(inserted)
}

impl DocumentStore {
    /// Execute a SQL statement that may mutate the store.
    ///
    /// `UPDATE`/`DELETE` against the `blocks` table are handled directly —
    /// see the module-level write-back notes above — and are written back
    /// to the affected document's *source Markdown file* (re-parsed in
    /// place, same `DocumentId`). Everything else (`SELECT`, `CREATE
    /// TABLE`, `INSERT`, `DROP TABLE`, `DESC`, `SHOW TABLES`) delegates to
    /// the regular read-only [`SqlEngine::execute`].
    ///
    /// Callers that expose this over an interface an end user might not
    /// expect to mutate files (a CLI, an HTTP/MCP endpoint) should gate it
    /// behind an explicit opt-in before calling this — write-back mutates
    /// the user's Markdown source on disk.
    pub fn execute_sql_mut(&mut self, sql: &str) -> Result<QueryOutput, MqdbError> {
        let trimmed = sql.trim().trim_end_matches(';');
        let upper = trimmed.to_ascii_uppercase();
        if upper.starts_with("DESC ") || upper.starts_with("DESCRIBE ") || upper == "SHOW TABLES" {
            return SqlEngine::new(self)?.execute(sql);
        }

        let stmts = Parser::parse_sql(&GenericDialect {}, sql)
            .map_err(|e| MqdbError::SqlParse(e.to_string()))?;
        let stmt = stmts
            .into_iter()
            .next()
            .ok_or_else(|| MqdbError::SqlParse("empty query".into()))?;

        match stmt {
            Statement::Update(update) => {
                if update.from.is_some() {
                    return Err(MqdbError::SqlExec(
                        "UPDATE ... FROM is not supported for write-back".into(),
                    ));
                }
                if update.assignments.len() != 1 {
                    return Err(MqdbError::SqlExec(
                        "write-back UPDATE supports exactly one assignment: SET content = ..."
                            .into(),
                    ));
                }
                let assignment = &update.assignments[0];
                let column = match &assignment.target {
                    AssignmentTarget::ColumnName(name) => {
                        name.0.last().map(ident_value).unwrap_or("").to_lowercase()
                    }
                    AssignmentTarget::Tuple(_) => {
                        return Err(MqdbError::SqlExec(
                            "write-back UPDATE does not support tuple assignment targets".into(),
                        ));
                    }
                };
                if column != "content" {
                    return Err(MqdbError::SqlExec(format!(
                        "write-back UPDATE only supports the 'content' column (got '{column}')"
                    )));
                }

                let edits = collect_matched_edits(
                    self,
                    &update.table,
                    update.selection.as_ref(),
                    Some(&assignment.value),
                )?;
                let n = apply_matched_edits(self, edits)?;
                Ok(QueryOutput {
                    columns: vec!["updated".to_string()],
                    rows: vec![vec![n.to_string()]],
                })
            }
            Statement::Delete(delete) => {
                let tables = match &delete.from {
                    FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => {
                        tables
                    }
                };
                if tables.len() != 1 {
                    return Err(MqdbError::SqlExec(
                        "write-back DELETE supports exactly one target table".into(),
                    ));
                }
                let edits =
                    collect_matched_edits(self, &tables[0], delete.selection.as_ref(), None)?;
                let n = apply_matched_edits(self, edits)?;
                Ok(QueryOutput {
                    columns: vec!["deleted".to_string()],
                    rows: vec![vec![n.to_string()]],
                })
            }
            Statement::Insert(ins) => {
                let table_name = match &ins.table {
                    TableObject::TableName(name) => {
                        name.0.last().map(ident_value).unwrap_or("").to_lowercase()
                    }
                    _ => return Err(MqdbError::SqlExec("unsupported INSERT target".into())),
                };
                if table_name == "blocks" {
                    let specs = collect_new_blocks(&ins)?;
                    let n = apply_new_blocks(self, specs)?;
                    Ok(QueryOutput {
                        columns: vec!["inserted".to_string()],
                        rows: vec![vec![n.to_string()]],
                    })
                } else {
                    SqlEngine::new(self)?.execute(sql)
                }
            }
            _ => SqlEngine::new(self)?.execute(sql),
        }
    }
}

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

fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name,
        "count" | "sum" | "min" | "max" | "avg" | "group_concat" | "string_agg"
    )
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
                Expr::Function(f) => {
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
                            seen.len().to_string()
                        }
                        "count" => group_rows.len().to_string(),
                        "group_concat" | "string_agg" => {
                            let sep = agg_separator(f);
                            group_rows
                                .iter()
                                .map(|r| agg_arg(f, r))
                                .filter(|v| !matches!(v, Value::Null))
                                .map(|v| v.display())
                                .collect::<Vec<_>>()
                                .join(&sep)
                        }
                        "sum" => {
                            let sum: f64 = group_rows
                                .iter()
                                .filter_map(|r| agg_arg(f, r).as_f64())
                                .sum();
                            sum.to_string()
                        }
                        "min" => group_rows
                            .iter()
                            .map(|r| agg_arg(f, r))
                            .min_by(|a, b| a.cmp_val(b).unwrap_or(std::cmp::Ordering::Equal))
                            .map(|v| v.display())
                            .unwrap_or_else(|| "NULL".into()),
                        "max" => group_rows
                            .iter()
                            .map(|r| agg_arg(f, r))
                            .max_by(|a, b| a.cmp_val(b).unwrap_or(std::cmp::Ordering::Equal))
                            .map(|v| v.display())
                            .unwrap_or_else(|| "NULL".into()),
                        "avg" => {
                            let vals: Vec<f64> = group_rows
                                .iter()
                                .filter_map(|r| agg_arg(f, r).as_f64())
                                .collect();
                            if vals.is_empty() {
                                "NULL".into()
                            } else {
                                (vals.iter().sum::<f64>() / vals.len() as f64).to_string()
                            }
                        }
                        _ => String::new(),
                    }
                }
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

fn expr_structurally_eq(a: &Expr, b: &Expr) -> bool {
    format!("{:?}", a) == format!("{:?}", b)
}

fn apply_order_by(rows: &mut [(Row, Vec<String>)], kind: &OrderByKind) {
    let exprs: &[OrderByExpr] = match kind {
        OrderByKind::Expressions(exprs) => exprs,
        _ => return,
    };
    rows.sort_by(|(ra, _), (rb, _)| {
        for ob in exprs {
            let va = eval_expr(&ob.expr, ra);
            let vb = eval_expr(&ob.expr, rb);
            let ord = va.cmp_val(&vb).unwrap_or(std::cmp::Ordering::Equal);
            // asc=None or asc=Some(true) → ascending; asc=Some(false) → descending
            let ord = if ob.options.asc == Some(false) {
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

fn apply_limit(mut rows: Vec<Vec<String>>, limit: Option<&Expr>) -> Vec<Vec<String>> {
    if let Some(lim) = limit {
        let dummy = Row {
            columns: vec![],
            values: vec![],
        };
        if let Value::Int(n) = eval_expr(lim, &dummy) {
            rows.truncate(n as usize);
        }
    }
    rows
}

/// Flattens a top-level AND-chain into its conjuncts, unwrapping parens.
/// Anything else (including `OR`) is returned as a single, unrecognized leaf.
fn flatten_and_conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            let mut out = flatten_and_conjuncts(left);
            out.extend(flatten_and_conjuncts(right));
            out
        }
        Expr::Nested(inner) => flatten_and_conjuncts(inner),
        other => vec![other],
    }
}

/// Whether `schema` has a column matching `short` (an already-lowercased,
/// unqualified name from [`expr_col_name`]). Mirrors `Row::get`'s fallback.
fn schema_has_short_col(schema: &[String], short: &str) -> bool {
    schema.iter().any(|c| {
        let cl = c.to_lowercase();
        cl == short || cl.split('.').next_back().unwrap_or(&cl) == short
    })
}

/// First top-level `AND`-conjunct of `on` that is a plain `column = column`
/// equality across `left_cols`/`right_cols`, as `(left_key_expr,
/// right_key_expr)`. `None` if there's no such conjunct (e.g. only a
/// computed key like `nxt.pre = h.pre + 1`) — caller falls back to cross-join.
fn find_equi_join_exprs<'a>(
    on: &'a Expr,
    left_cols: &[String],
    right_cols: &[String],
) -> Option<(&'a Expr, &'a Expr)> {
    for conjunct in flatten_and_conjuncts(on) {
        let Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } = conjunct
        else {
            continue;
        };
        let (Some(lname), Some(rname)) = (expr_col_name(left), expr_col_name(right)) else {
            continue;
        };
        if schema_has_short_col(left_cols, &lname) && schema_has_short_col(right_cols, &rname) {
            return Some((left, right));
        }
        if schema_has_short_col(right_cols, &lname) && schema_has_short_col(left_cols, &rname) {
            return Some((right, left));
        }
    }
    None
}

/// Decides whether a whole document can be skipped using [`ZoneMaps`],
/// without reading any of its blocks. Unlike [`IndexHint`], a wrong skip
/// here silently drops matching rows, so this only returns `true` when it
/// can prove no block in the document satisfies `where_expr`.
fn zone_map_skip(zone_maps: &ZoneMaps, where_expr: &Expr) -> bool {
    let mut eq_block_type: Option<BlockType> = None;
    let mut eq_content: Option<String> = None;
    let mut eq_lang: Option<String> = None;
    let mut eq_depth: Option<u8> = None;

    for conjunct in flatten_and_conjuncts(where_expr) {
        let Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } = conjunct
        else {
            continue;
        };
        let col = expr_col_name(left).or_else(|| expr_col_name(right));
        let val = expr_str_val(right).or_else(|| expr_str_val(left));
        let int_val = expr_int_val(right).or_else(|| expr_int_val(left));

        match col.as_deref() {
            Some("block_type") => {
                if let Some(s) = val.as_deref()
                    && let Some(bt) = BlockType::from_str(s)
                {
                    eq_block_type = Some(bt);
                }
            }
            Some("content") => eq_content = val,
            // lang = '' means "no lang" (matches non-code blocks), which
            // code_languages says nothing about.
            Some("lang") => {
                if let Some(s) = val
                    && !s.is_empty()
                {
                    eq_lang = Some(s);
                }
            }
            // depth = 0 means "no heading depth" (matches non-heading
            // blocks), which max_heading_depth says nothing about.
            Some("depth") => {
                if let Some(n) = int_val
                    && let Ok(n) = u8::try_from(n)
                    && n > 0
                {
                    eq_depth = Some(n);
                }
            }
            _ => {}
        }
    }

    if let Some(lang) = &eq_lang
        && !zone_maps.code_languages.contains(lang)
    {
        return true;
    }
    if let Some(depth) = eq_depth
        && depth > zone_maps.max_heading_depth
    {
        return true;
    }
    // Only safe when `block_type = 'heading'` is also required — `content`
    // alone could match a non-heading block.
    if let Some(content) = &eq_content
        && eq_block_type == Some(BlockType::Heading)
        && !zone_maps
            .heading_contents
            .iter()
            .any(|h| h.eq_ignore_ascii_case(content))
    {
        return true;
    }

    false
}

/// Inspect the WHERE expression and return the best [`IndexHint`].
///
/// Only analyses the *outermost* conjunct that can be served by an index.
/// The full WHERE predicate is still evaluated row-by-row after pre-filtering,
/// so false positives from index lookups are harmless (but there shouldn't be any).
///
/// Patterns recognised:
/// - `block_type = 'X'` → [`IndexHint::BlockType`]
/// - `block_type IN ('X','Y',...)` → [`IndexHint::BlockType`] (union)
/// - `pre = N` → [`IndexHint::PreExact`]
/// - `pre BETWEEN lo AND hi` → [`IndexHint::PreRange`]
/// - `content = 'X'` → [`IndexHint::ContentExact`]
/// - `lang = 'X'` → [`IndexHint::LangExact`]
/// - `depth = N` → [`IndexHint::DepthExact`]
/// - `A AND B` → picks the better hint from A or B
fn analyze_where_for_index(expr: &Expr) -> IndexHint {
    match expr {
        // A AND B — try both sides, prefer more selective
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            let lh = analyze_where_for_index(left);
            let rh = analyze_where_for_index(right);
            pick_better_hint(lh, rh)
        }
        // col = 'value'
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => {
            let col = expr_col_name(left).or_else(|| expr_col_name(right));
            let val = expr_str_val(right).or_else(|| expr_str_val(left));
            let int_val = expr_int_val(right).or_else(|| expr_int_val(left));

            match col.as_deref() {
                Some("block_type") => {
                    if let Some(s) = val
                        && let Some(bt) = BlockType::from_str(&s)
                    {
                        return IndexHint::BlockType(vec![bt]);
                    }
                    IndexHint::FullScan
                }
                Some("pre") => {
                    if let Some(n) = int_val {
                        return IndexHint::PreExact(n as u32);
                    }
                    IndexHint::FullScan
                }
                Some("content") => {
                    if let Some(s) = val {
                        return IndexHint::ContentExact(s);
                    }
                    IndexHint::FullScan
                }
                Some("lang") => {
                    if let Some(s) = val
                        && !s.is_empty()
                    {
                        return IndexHint::LangExact(s);
                    }
                    IndexHint::FullScan
                }
                Some("depth") => {
                    if let Some(n) = int_val {
                        // depth 0 means "no heading depth" — not in the index
                        if n > 0 {
                            return IndexHint::DepthExact(n as u8);
                        }
                    }
                    IndexHint::FullScan
                }
                _ => IndexHint::FullScan,
            }
        }
        // block_type IN ('heading', 'code')
        Expr::InList {
            expr,
            list,
            negated: false,
        } => {
            if expr_col_name(expr).as_deref() == Some("block_type") {
                let types: Vec<BlockType> = list
                    .iter()
                    .filter_map(expr_str_val)
                    .filter_map(|s| BlockType::from_str(&s))
                    .collect();
                if !types.is_empty() {
                    return IndexHint::BlockType(types);
                }
            }
            IndexHint::FullScan
        }
        // pre BETWEEN lo AND hi
        Expr::Between {
            expr,
            negated: false,
            low,
            high,
        } => {
            if expr_col_name(expr).as_deref() == Some("pre")
                && let (Some(lo), Some(hi)) = (expr_int_val(low), expr_int_val(high))
            {
                return IndexHint::PreRange(lo as u32, hi as u32);
            }
            IndexHint::FullScan
        }
        // match(content, 'query terms') used directly as a boolean predicate
        // (unlike the other arms above, this isn't wrapped in a BinaryOp).
        Expr::Function(f) => {
            let name = f
                .name
                .0
                .last()
                .map(ident_value)
                .unwrap_or("")
                .to_lowercase();
            if name != "match" {
                return IndexHint::FullScan;
            }
            let FunctionArguments::List(al) = &f.args else {
                return IndexHint::FullScan;
            };
            let [
                FunctionArg::Unnamed(FunctionArgExpr::Expr(col)),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(q)),
            ] = al.args.as_slice()
            else {
                return IndexHint::FullScan;
            };
            if expr_col_name(col).as_deref() != Some("content") {
                return IndexHint::FullScan;
            }
            let Some(query_str) = expr_str_val(q) else {
                return IndexHint::FullScan;
            };
            let terms = tokenize(&query_str);
            if terms.is_empty() {
                IndexHint::FullScan
            } else {
                IndexHint::TermMatch(terms)
            }
        }
        Expr::Nested(inner) => analyze_where_for_index(inner),
        _ => IndexHint::FullScan,
    }
}

/// Returns the column name if the expression is a bare identifier or `alias.col`.
fn expr_col_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(i) => Some(i.value.to_lowercase()),
        Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.to_lowercase()),
        _ => None,
    }
}

fn expr_str_val(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Value(v) => match &v.value {
            SqlValue::SingleQuotedString(s) | SqlValue::DoubleQuotedString(s) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn expr_int_val(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Value(v) => match &v.value {
            SqlValue::Number(n, _) => n.parse::<i64>().ok(),
            _ => None,
        },
        _ => None,
    }
}

/// Pick the more selective of two hints (prefer specific types over FullScan).
fn pick_better_hint(a: IndexHint, b: IndexHint) -> IndexHint {
    match (&a, &b) {
        (IndexHint::FullScan, _) => b,
        (_, IndexHint::FullScan) => a,
        // Both have hints — prefer the one that narrows more
        // BlockType with fewer types is more selective
        (IndexHint::BlockType(ta), IndexHint::BlockType(tb)) => {
            if ta.len() <= tb.len() {
                a
            } else {
                b
            }
        }
        // Exact lookups beat range
        (IndexHint::PreExact(_), _) => a,
        (_, IndexHint::PreExact(_)) => b,
        _ => a,
    }
}

impl BlockType {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "heading" => Some(BlockType::Heading),
            "paragraph" => Some(BlockType::Paragraph),
            "code" => Some(BlockType::Code),
            "list" => Some(BlockType::List),
            "table_cell" => Some(BlockType::TableCell),
            "table_row" => Some(BlockType::TableRow),
            "table_align" => Some(BlockType::TableAlign),
            "blockquote" => Some(BlockType::Blockquote),
            "horizontal_rule" => Some(BlockType::HorizontalRule),
            "html" => Some(BlockType::Html),
            "yaml" => Some(BlockType::Yaml),
            "toml" => Some(BlockType::Toml),
            "math" => Some(BlockType::Math),
            "definition" => Some(BlockType::Definition),
            "footnote" => Some(BlockType::Footnote),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DocumentStore;
    use rstest::rstest;

    fn make_store() -> DocumentStore {
        let mut s = DocumentStore::new();
        s.add_str(
            "# Doc\n\n## Architecture\n\nDetails\n\n```rust\nfn main(){}\n```\n\n## Other\n\nOther\n",
        )
        .unwrap();
        s
    }

    // Doc B (no code, depth 1) sits between two rust/depth-3 docs.
    fn make_multi_doc_store() -> DocumentStore {
        let mut s = DocumentStore::new();
        s.add_str("# A\n\n```rust\nfn a(){}\n```\n").unwrap();
        s.add_str("# B\n\nParagraph\n").unwrap();
        s.add_str("# C\n\n## C2\n\n### C3\n\n```rust\nfn c(){}\n```\n")
            .unwrap();
        s
    }

    #[test]
    fn test_sql_select_all_blocks() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT block_type, content FROM blocks ORDER BY pre")
            .unwrap();
        assert!(!out.rows.is_empty());
    }

    #[test]
    fn test_sql_heading_filter() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT content FROM blocks WHERE block_type = 'heading' ORDER BY pre")
            .unwrap();
        assert_eq!(out.rows.len(), 3);
    }

    #[test]
    fn test_sql_under_function() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "SELECT b.content FROM blocks b
             WHERE under(b.pre, b.post,
               (SELECT pre FROM blocks WHERE block_type='heading' AND content='Architecture'),
               (SELECT post FROM blocks WHERE block_type='heading' AND content='Architecture')
             )",
            )
            .unwrap();
        assert_eq!(out.rows.len(), 2);
    }

    #[test]
    fn test_query_output_table() {
        let out = QueryOutput {
            columns: vec!["id".to_string(), "type".to_string()],
            rows: vec![
                vec!["1".to_string(), "heading".to_string()],
                vec!["2".to_string(), "paragraph".to_string()],
            ],
        };
        let table = out.to_table();
        assert!(table.contains("heading"));
        assert!(table.contains("paragraph"));
        assert!(table.contains("2 rows"));
    }

    #[test]
    fn test_sql_count_aggregate() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT count(*) FROM blocks WHERE block_type = 'heading'")
            .unwrap();
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0][0], "3");
    }

    #[test]
    fn test_sql_limit() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT content FROM blocks LIMIT 2")
            .unwrap();
        assert_eq!(out.rows.len(), 2);
    }

    #[test]
    fn test_sql_like() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT content FROM blocks WHERE content LIKE '%chitect%'")
            .unwrap();
        assert!(!out.rows.is_empty());
    }

    #[test]
    fn test_sql_order_by_desc() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT content FROM blocks ORDER BY pre DESC LIMIT 1")
            .unwrap();
        assert_eq!(out.rows.len(), 1);
    }

    #[test]
    fn test_sql_engine_zero_copy() {
        let mut store = DocumentStore::new();
        for _ in 0..100 {
            store.add_str("# Heading\n\nParagraph text\n").unwrap();
        }
        let start = std::time::Instant::now();
        let _engine = SqlEngine::new(&store).unwrap();
        let elapsed = start.elapsed();
        // Bound loosened from 1ms to 5ms when `TermIndex` (a fourth
        // per-document index) was added — still catches anything
        // pathological (e.g. accidental file I/O or O(n^2) behaviour) while
        // tolerating cold-start allocator/thread warmup noise on the first
        // test invocation in a fresh process.
        assert!(
            elapsed.as_micros() < 5000,
            "SqlEngine::new took {}us — should be cheap",
            elapsed.as_micros()
        );
    }

    // make_store() produces:
    //   "# Doc\n\n## Architecture\n\nDetails\n\n```rust\nfn main(){}\n```\n\n## Other\n\nOther\n"
    // → heading×3, paragraph×2, code×1  (6 blocks total)

    #[rstest]
    #[case("SELECT content FROM blocks WHERE block_type = 'heading'", 3)]
    #[case("SELECT content FROM blocks WHERE block_type = 'paragraph'", 2)]
    #[case("SELECT content FROM blocks WHERE block_type = 'code'", 1)]
    #[case("SELECT content FROM blocks WHERE block_type = 'list'", 0)]
    fn test_sql_where_block_type_param(#[case] sql: &str, #[case] expected: usize) {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        assert_eq!(engine.execute(sql).unwrap().rows.len(), expected);
    }

    #[rstest]
    #[case("SELECT content FROM blocks WHERE content LIKE '%Doc%'", 1)]
    #[case("SELECT content FROM blocks WHERE content LIKE '%chitect%'", 1)]
    #[case("SELECT content FROM blocks WHERE content LIKE '%Other%'", 2)]
    #[case("SELECT content FROM blocks WHERE content LIKE '%Details%'", 1)]
    #[case("SELECT content FROM blocks WHERE content LIKE '%nonexistent%'", 0)]
    fn test_sql_like_pattern_param(#[case] sql: &str, #[case] expected: usize) {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        assert_eq!(engine.execute(sql).unwrap().rows.len(), expected);
    }

    #[rstest]
    #[case("SELECT content FROM blocks LIMIT 1", 1)]
    #[case("SELECT content FROM blocks LIMIT 3", 3)]
    #[case("SELECT content FROM blocks LIMIT 5", 5)]
    #[case("SELECT content FROM blocks LIMIT 1000", 6)]
    fn test_sql_limit_row_count_param(#[case] sql: &str, #[case] expected: usize) {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        assert_eq!(engine.execute(sql).unwrap().rows.len(), expected);
    }

    #[rstest]
    #[case("SELECT count(*) FROM blocks", "6")]
    #[case("SELECT count(*) FROM blocks WHERE block_type = 'heading'", "3")]
    #[case("SELECT count(*) FROM blocks WHERE block_type = 'code'", "1")]
    fn test_sql_count_aggregate_param(#[case] sql: &str, #[case] expected: &str) {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine.execute(sql).unwrap();
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0][0], expected);
    }

    // depth = 0 should return all non-heading blocks (paragraphs + code), not 0 rows
    #[test]
    fn test_sql_depth_zero_returns_non_headings() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT content FROM blocks WHERE depth = 0")
            .unwrap();
        // make_store has 2 paragraphs + 1 code block = 3 non-heading blocks
        assert_eq!(out.rows.len(), 3, "depth=0 must return non-heading blocks");
    }

    // lang = '' should return non-code blocks (paragraph, heading blocks have empty lang)
    #[test]
    fn test_sql_empty_lang_returns_non_code_blocks() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT block_type FROM blocks WHERE lang = ''")
            .unwrap();
        // make_store: 3 headings + 2 paragraphs = 5 blocks with no lang
        assert_eq!(out.rows.len(), 5, "lang='' must return non-code blocks");
    }

    // to_table() must not let newlines inside cells break the table row structure
    #[test]
    fn test_to_table_newline_in_cell() {
        let out = QueryOutput {
            columns: vec!["content".to_string()],
            rows: vec![
                vec!["line one\nline two".to_string()],
                vec!["plain".to_string()],
            ],
        };
        let table = out.to_table();
        // Lines that start with '│' = header + 2 data rows = 3 (no extra split)
        let bar_lines: Vec<&str> = table.lines().filter(|l| l.starts_with('│')).collect();
        assert_eq!(
            bar_lines.len(),
            3,
            "newline in cell must not produce extra table rows"
        );
        // The first data row (index 1, after the header) must contain the normalised content
        assert!(bar_lines[1].contains("line one line two"));
    }

    // register_table / custom table query
    #[test]
    fn test_custom_table_query() {
        let mut store = DocumentStore::new();
        store.register_table(
            "kv",
            vec!["key".to_string(), "value".to_string()],
            vec![
                vec!["foo".to_string(), "bar".to_string()],
                vec!["hello".to_string(), "world".to_string()],
            ],
        );
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT key, value FROM kv WHERE key = 'hello'")
            .unwrap();
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0][1], "world");
    }

    // CREATE TABLE (empty) then INSERT then SELECT
    #[test]
    fn test_ddl_create_insert_select() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();

        // create
        engine
            .execute("CREATE TABLE notes (id TEXT, body TEXT)")
            .unwrap();
        // insert two rows
        engine
            .execute("INSERT INTO notes VALUES ('1', 'hello')")
            .unwrap();
        engine
            .execute("INSERT INTO notes VALUES ('2', 'world')")
            .unwrap();
        // select with filter
        let out = engine
            .execute("SELECT body FROM notes WHERE id = '1'")
            .unwrap();
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0][0], "hello");
        // total rows
        let all = engine.execute("SELECT * FROM notes").unwrap();
        assert_eq!(all.rows.len(), 2);
    }

    // CREATE TABLE AS SELECT
    #[test]
    fn test_ddl_create_as_select() {
        let store = {
            let mut s = DocumentStore::new();
            s.add_str("# H1\n\n## H2\n\nParagraph\n").unwrap();
            s
        };
        let engine = SqlEngine::new(&store).unwrap();
        engine
            .execute(
                "CREATE TABLE headings AS \
                 SELECT block_type, content FROM blocks WHERE block_type = 'heading'",
            )
            .unwrap();
        let out = engine.execute("SELECT content FROM headings").unwrap();
        assert_eq!(out.rows.len(), 2);
    }

    // DROP TABLE
    #[test]
    fn test_ddl_drop_table() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        engine.execute("CREATE TABLE tmp (x TEXT)").unwrap();
        engine.execute("DROP TABLE tmp").unwrap();
        let err = engine.execute("SELECT * FROM tmp").unwrap_err();
        assert!(err.to_string().contains("unknown table"));
    }

    // DROP TABLE IF EXISTS (must not error on missing table)
    #[test]
    fn test_ddl_drop_if_exists() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        engine
            .execute("DROP TABLE IF EXISTS no_such_table")
            .unwrap();
    }

    // DESC blocks (built-in)
    #[test]
    fn test_desc_builtin() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine.execute("DESC blocks").unwrap();
        assert_eq!(out.columns, vec!["column", "type"]);
        assert!(out.rows.iter().any(|r| r[0] == "block_type"));
        assert!(out.rows.iter().any(|r| r[0] == "content"));
    }

    // DESC custom table
    #[test]
    fn test_desc_custom() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        engine
            .execute("CREATE TABLE meta (k TEXT, v TEXT)")
            .unwrap();
        let out = engine.execute("DESC meta").unwrap();
        assert_eq!(out.rows.len(), 2);
        assert_eq!(out.rows[0][0], "k");
        assert_eq!(out.rows[1][0], "v");
    }

    // SHOW TABLES
    #[test]
    fn test_show_tables() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        engine.execute("CREATE TABLE extra (a TEXT)").unwrap();
        let out = engine.execute("SHOW TABLES").unwrap();
        let names: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
        assert!(names.contains(&"blocks"));
        assert!(names.contains(&"documents"));
        assert!(names.contains(&"extra"));
    }

    // mq() scalar function applied to a literal markdown string
    #[test]
    fn test_mq_scalar_function() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "SELECT mq('.h1 | to_text', '# Hello\n\nWorld\n') AS title FROM blocks LIMIT 1",
            )
            .unwrap();
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0][0], "Hello");
    }

    // mq() returns NULL when program produces no output
    #[test]
    fn test_mq_scalar_null_on_no_match() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT mq('.h1', '## No h1 here\n') FROM blocks LIMIT 1")
            .unwrap();
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0][0], "NULL");
    }

    #[test]
    fn match_function_true_for_all_terms_present() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT match('The quick brown fox', 'quick fox')")
            .unwrap();
        assert_eq!(out.rows[0][0], "true");
    }

    #[test]
    fn match_function_false_if_any_term_missing() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT match('The quick brown fox', 'quick zebra')")
            .unwrap();
        assert_eq!(out.rows[0][0], "false");
    }

    #[test]
    fn match_function_case_insensitive() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT match('Rust Programming', 'rust')")
            .unwrap();
        assert_eq!(out.rows[0][0], "true");
    }

    #[test]
    fn score_function_ranks_denser_matches_higher() {
        let mut store = DocumentStore::new();
        store
            .add_str("# Doc\n\nrust rust rust other words here\n\nrust is fine\n")
            .unwrap();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "SELECT content FROM blocks WHERE block_type = 'paragraph'
                 ORDER BY score(content, 'rust') DESC",
            )
            .unwrap();
        assert_eq!(out.rows[0][0], "rust rust rust other words here");
    }

    #[test]
    fn where_match_uses_term_match_index_hint() {
        let stmts = Parser::parse_sql(
            &GenericDialect {},
            "SELECT * FROM blocks WHERE match(content, 'foo bar')",
        )
        .unwrap();
        let Statement::Query(q) = stmts.into_iter().next().unwrap() else {
            panic!("expected query")
        };
        let SetExpr::Select(select) = q.body.as_ref() else {
            panic!("expected select")
        };
        let hint = analyze_where_for_index(select.selection.as_ref().unwrap());
        assert_eq!(
            hint,
            IndexHint::TermMatch(vec!["foo".to_string(), "bar".to_string()])
        );
    }

    #[test]
    fn where_match_and_block_type_combines_hints() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "SELECT content FROM blocks
                 WHERE match(content, 'architecture') AND block_type = 'heading'",
            )
            .unwrap();
        assert_eq!(out.rows, vec![vec!["Architecture".to_string()]]);
    }

    #[test]
    fn where_match_full_scan_fallback_when_query_not_literal() {
        let stmts = Parser::parse_sql(
            &GenericDialect {},
            "SELECT * FROM blocks WHERE match(content, lang)",
        )
        .unwrap();
        let Statement::Query(q) = stmts.into_iter().next().unwrap() else {
            panic!("expected query")
        };
        let SetExpr::Select(select) = q.body.as_ref() else {
            panic!("expected select")
        };
        let hint = analyze_where_for_index(select.selection.as_ref().unwrap());
        assert_eq!(hint, IndexHint::FullScan);
    }

    fn eval_one(sql: &str) -> String {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        engine.execute(sql).unwrap().rows[0][0].clone()
    }

    #[rstest]
    // string functions
    #[case("SELECT lower('Hello')", "hello")]
    #[case("SELECT upper('Hello')", "HELLO")]
    #[case("SELECT length('héllo')", "5")]
    #[case("SELECT trim('  hi  ')", "hi")]
    #[case("SELECT ltrim('  hi  ')", "hi  ")]
    #[case("SELECT rtrim('  hi  ')", "  hi")]
    #[case("SELECT trim(LEADING 'x' FROM 'xxhixx')", "hixx")]
    #[case("SELECT trim(TRAILING 'x' FROM 'xxhixx')", "xxhi")]
    #[case("SELECT trim('x' FROM 'xxhixx')", "hi")]
    #[case("SELECT concat('a', 'b', 'c')", "abc")]
    #[case("SELECT concat_ws('-', 'a', 'b', NULL, 'c')", "a-b-c")]
    #[case("SELECT replace('foobar', 'o', '0')", "f00bar")]
    #[case("SELECT left('hello', 3)", "hel")]
    #[case("SELECT right('hello', 3)", "llo")]
    #[case("SELECT lpad('7', 3, '0')", "007")]
    #[case("SELECT rpad('7', 3, '0')", "700")]
    #[case("SELECT reverse('hello')", "olleh")]
    #[case("SELECT repeat('ab', 3)", "ababab")]
    #[case("SELECT initcap('hello world')", "Hello World")]
    #[case("SELECT ascii('A')", "65")]
    #[case("SELECT chr(65)", "A")]
    #[case("SELECT instr('hello world', 'world')", "7")]
    #[case("SELECT position('world' in 'hello world')", "7")]
    #[case("SELECT split_part('a,b,c', ',', 2)", "b")]
    #[case("SELECT substring('hello world', 1, 5)", "hello")]
    #[case("SELECT substring('hello world' from 7)", "world")]
    #[case("SELECT substr('hello world', 7, 5)", "world")]
    // numeric functions
    #[case("SELECT abs(-5)", "5")]
    #[case("SELECT abs(-5.5)", "5.5")]
    #[case("SELECT round(3.456, 2)", "3.46")]
    #[case("SELECT round(3.5)", "4")]
    #[case("SELECT ceil(3.1)", "4")]
    #[case("SELECT floor(3.9)", "3")]
    #[case("SELECT trunc(3.789, 1)", "3.7")]
    #[case("SELECT mod(10, 3)", "1")]
    #[case("SELECT power(2, 10)", "1024")]
    #[case("SELECT sqrt(16)", "4")]
    #[case("SELECT sign(-3)", "-1")]
    #[case("SELECT greatest(3, 7, 2)", "7")]
    #[case("SELECT least(3, 7, 2)", "2")]
    // null handling
    #[case("SELECT coalesce(NULL, NULL, 'x')", "x")]
    #[case("SELECT ifnull(NULL, 'y')", "y")]
    #[case("SELECT nullif('a', 'a')", "NULL")]
    #[case("SELECT nullif('a', 'b')", "a")]
    // misc
    #[case("SELECT typeof('x')", "text")]
    #[case("SELECT typeof(1)", "integer")]
    // CASE
    #[case(
        "SELECT CASE WHEN 1 = 2 THEN 'a' WHEN 1 = 1 THEN 'b' ELSE 'c' END",
        "b"
    )]
    #[case("SELECT CASE 2 WHEN 1 THEN 'a' WHEN 2 THEN 'b' ELSE 'c' END", "b")]
    #[case("SELECT CASE WHEN 1 = 2 THEN 'a' ELSE 'c' END", "c")]
    fn test_sql_scalar_functions(#[case] sql: &str, #[case] expected: &str) {
        assert_eq!(eval_one(sql), expected);
    }

    #[test]
    fn test_sql_group_concat() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT group_concat(content) FROM blocks WHERE block_type = 'heading'")
            .unwrap();
        assert_eq!(out.rows[0][0], "Doc,Architecture,Other");
    }

    #[test]
    fn test_sql_string_agg_custom_separator() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT string_agg(content, ' | ') FROM blocks WHERE block_type = 'heading'")
            .unwrap();
        assert_eq!(out.rows[0][0], "Doc | Architecture | Other");
    }

    #[test]
    fn test_sql_count_distinct() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT count(DISTINCT block_type) FROM blocks")
            .unwrap();
        assert_eq!(out.rows[0][0], "3");
    }

    // doc B has no code at all; A and C's rust blocks must still come through.
    #[test]
    fn test_sql_zone_map_skip_by_lang() {
        let store = make_multi_doc_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT content FROM blocks WHERE lang = 'rust' ORDER BY content")
            .unwrap();
        let contents: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(contents, vec!["fn a(){}", "fn c(){}"]);
    }

    // depth=3 only exists in doc C; A and B (max depth 1) must be skipped.
    #[test]
    fn test_sql_zone_map_skip_by_depth() {
        let store = make_multi_doc_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT content FROM blocks WHERE depth = 3")
            .unwrap();
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0][0], "C3");
    }

    // Only doc B has a heading named "B"; requires block_type='heading' too.
    #[test]
    fn test_sql_zone_map_skip_by_heading_content() {
        let store = make_multi_doc_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT content FROM blocks WHERE block_type = 'heading' AND content = 'B'")
            .unwrap();
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0][0], "B");
    }

    // `lang = ''` means "no lang"; must never trigger a code-language skip.
    #[test]
    fn test_sql_zone_map_no_skip_on_empty_lang() {
        let store = make_multi_doc_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT content FROM blocks WHERE lang = ''")
            .unwrap();
        let contents: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
        assert!(contents.contains(&"B"), "doc B must not be skipped");
        assert!(contents.contains(&"Paragraph"));
    }

    // `id` must stay stable regardless of which documents get skipped.
    #[test]
    fn test_sql_zone_map_skip_preserves_block_ids() {
        let store = make_multi_doc_store();
        let engine = SqlEngine::new(&store).unwrap();
        let full = engine.execute("SELECT id, content FROM blocks").unwrap();
        let filtered = engine
            .execute("SELECT id, content FROM blocks WHERE lang = 'rust'")
            .unwrap();
        assert_eq!(filtered.rows.len(), 2);
        for row in &filtered.rows {
            let same_id = full.rows.iter().find(|r| r[0] == row[0]).unwrap();
            assert_eq!(
                same_id[1], row[1],
                "id {} must reference the same block content in both queries",
                row[0]
            );
        }
    }

    // Zone-map skip is disabled whenever FROM has a join (see `exec_query`).
    // Just checks a join with a recognized conjunct still scans normally.
    #[test]
    fn test_sql_zone_map_skip_disabled_for_joins() {
        let store = make_multi_doc_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "SELECT h.content, c.content FROM blocks h
                 JOIN blocks c ON c.document_id = h.document_id AND c.block_type = 'code'
                 WHERE h.block_type = 'heading'",
            )
            .unwrap();
        let headings: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(headings, vec!["A", "C", "C2", "C3"]);
    }

    #[test]
    fn cte_basic_select_from_named_cte() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "WITH headings AS (SELECT content FROM blocks WHERE block_type = 'heading')
                 SELECT content FROM headings ORDER BY content",
            )
            .unwrap();
        let contents: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(contents, vec!["Architecture", "Doc", "Other"]);
    }

    #[test]
    fn cte_later_cte_references_earlier_cte_in_same_with() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "WITH h AS (SELECT content FROM blocks WHERE block_type = 'heading'),
                      h2 AS (SELECT content FROM h WHERE content != 'Doc')
                 SELECT content FROM h2 ORDER BY content",
            )
            .unwrap();
        let contents: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(contents, vec!["Architecture", "Other"]);
    }

    #[test]
    fn cte_forward_reference_to_later_cte_errors_unknown_table() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let err = engine
            .execute(
                "WITH a AS (SELECT content FROM b),
                      b AS (SELECT content FROM blocks WHERE block_type = 'heading')
                 SELECT content FROM a",
            )
            .unwrap_err();
        assert!(err.to_string().contains("unknown table"));
    }

    #[test]
    fn cte_used_in_join_both_sides() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "WITH h AS (SELECT content, document_id FROM blocks WHERE block_type = 'heading')
                 SELECT a.content, b.content FROM h a JOIN h b
                   ON a.document_id = b.document_id AND a.content = 'Doc' AND b.content = 'Other'",
            )
            .unwrap();
        assert_eq!(out.rows, vec![vec!["Doc".to_string(), "Other".to_string()]]);
    }

    #[test]
    fn cte_visible_inside_subquery_in_where_clause() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "WITH h AS (SELECT content FROM blocks WHERE block_type = 'heading')
                 SELECT content FROM blocks
                 WHERE content = (SELECT content FROM h WHERE content = 'Doc')",
            )
            .unwrap();
        assert_eq!(out.rows, vec![vec!["Doc".to_string()]]);
    }

    #[test]
    fn cte_recursive_rejected_with_clear_error() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let err = engine
            .execute("WITH RECURSIVE r AS (SELECT content FROM blocks) SELECT content FROM r")
            .unwrap_err();
        assert!(err.to_string().contains("RECURSIVE"));
    }

    #[test]
    fn cte_name_shadows_blocks_table() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("WITH blocks AS (SELECT 'shadowed' AS content) SELECT content FROM blocks")
            .unwrap();
        assert_eq!(out.rows, vec![vec!["shadowed".to_string()]]);
    }

    #[test]
    fn cte_name_collision_with_custom_table_prefers_cte() {
        let mut store = make_store();
        store
            .execute_sql_mut("CREATE TABLE notes (name TEXT)")
            .unwrap();
        store
            .execute_sql_mut("INSERT INTO notes (name) VALUES ('real')")
            .unwrap();

        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("WITH notes AS (SELECT 'cte' AS name) SELECT name FROM notes")
            .unwrap();
        assert_eq!(out.rows, vec![vec!["cte".to_string()]]);
    }

    #[test]
    fn cte_column_alias_list_rejected() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let err = engine
            .execute(
                "WITH h(a) AS (SELECT content FROM blocks WHERE block_type = 'heading')
                 SELECT a FROM h",
            )
            .unwrap_err();
        assert!(err.to_string().contains("column aliases"));
    }

    #[test]
    fn cte_shadowing_across_nested_subquery_with_same_name() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "WITH x AS (SELECT content FROM blocks WHERE content = 'Doc')
                 SELECT content FROM blocks
                 WHERE block_type = 'heading'
                   AND (content = (WITH x AS (SELECT content FROM blocks WHERE content = 'Other') SELECT content FROM x)
                        OR content = (SELECT content FROM x))
                 ORDER BY content",
            )
            .unwrap();
        let contents: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(contents, vec!["Doc", "Other"]);
    }

    // ── UPDATE/DELETE write-back ────────────────────────────────────────────

    fn write_md(dir: &tempfile::TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn write_back_update_rewrites_heading_and_keeps_rest_of_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Old Title\n\nBody text\n");

        let mut store = DocumentStore::new();
        let doc_id = store.add_file(&path).unwrap();

        let out = store
            .execute_sql_mut("UPDATE blocks SET content = 'New Title' WHERE block_type = 'heading'")
            .unwrap();
        assert_eq!(out.rows[0][0], "1");

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "# New Title\n\nBody text\n");

        assert_eq!(store.documents()[0].id, doc_id);
        assert!(
            store.documents()[0]
                .blocks
                .iter()
                .any(|b| b.content == "New Title")
        );
    }

    #[test]
    fn write_back_update_rewrites_paragraph_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nOld body\n\nAnother paragraph\n");

        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        store
            .execute_sql_mut("UPDATE blocks SET content = 'New body' WHERE content = 'Old body'")
            .unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "# Title\n\nNew body\n\nAnother paragraph\n");
    }

    #[test]
    fn write_back_delete_removes_matched_block_and_blank_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nKeep me\n\nRemove me\n");

        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        let out = store
            .execute_sql_mut("DELETE FROM blocks WHERE content = 'Remove me'")
            .unwrap();
        assert_eq!(out.rows[0][0], "1");

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "# Title\n\nKeep me\n");
        assert!(
            !store.documents()[0]
                .blocks
                .iter()
                .any(|b| b.content == "Remove me")
        );
    }

    #[test]
    fn write_back_update_rejects_non_heading_paragraph_block_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\n```rust\nfn main() {}\n```\n");

        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        let err = store
            .execute_sql_mut(
                "UPDATE blocks SET content = 'fn other() {}' WHERE block_type = 'code'",
            )
            .unwrap_err();
        assert!(err.to_string().contains("heading/paragraph"));
    }

    #[test]
    fn write_back_rejects_document_with_no_source_path() {
        let mut store = DocumentStore::new();
        store.add_str("# Title\n\nBody\n").unwrap();

        let err = store
            .execute_sql_mut("UPDATE blocks SET content = 'x' WHERE block_type = 'heading'")
            .unwrap_err();
        assert!(err.to_string().contains("no source file"));
    }

    #[test]
    fn write_back_rejects_column_other_than_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");

        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        let err = store
            .execute_sql_mut("UPDATE blocks SET pre = 5 WHERE block_type = 'heading'")
            .unwrap_err();
        assert!(err.to_string().contains("'content'"));
    }

    #[test]
    fn write_back_rejects_joins() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");

        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        let err = store
            .execute_sql_mut(
                "UPDATE blocks b JOIN blocks c ON c.document_id = b.document_id SET b.content = 'x'",
            )
            .unwrap_err();
        assert!(err.to_string().contains("joins"));
    }

    #[test]
    fn write_back_read_only_statements_still_work_via_execute_sql_mut() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");

        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        let out = store
            .execute_sql_mut("SELECT content FROM blocks WHERE block_type = 'heading'")
            .unwrap();
        assert_eq!(out.rows, vec![vec!["Title".to_string()]]);
    }

    fn title_pre(store: &DocumentStore) -> String {
        SqlEngine::new(store)
            .unwrap()
            .execute("SELECT pre FROM blocks WHERE content = 'Title'")
            .unwrap()
            .rows[0][0]
            .clone()
    }

    #[test]
    fn write_back_insert_heading_after_pre_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");

        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();
        let pre = title_pre(&store);

        let out = store
            .execute_sql_mut(&format!(
                "INSERT INTO blocks (document_id, block_type, content, depth, after_pre) VALUES (0, 'heading', 'Subsection', 2, {pre})"
            ))
            .unwrap();
        assert_eq!(out.rows[0][0], "1");

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "# Title\n\n## Subsection\n\nBody\n");
        assert!(
            store.documents()[0]
                .blocks
                .iter()
                .any(|b| b.content == "Subsection")
        );
    }

    #[test]
    fn write_back_insert_paragraph_append_at_end_no_after_pre() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");

        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        let out = store
            .execute_sql_mut(
                "INSERT INTO blocks (document_id, block_type, content) VALUES (0, 'paragraph', 'Appended')",
            )
            .unwrap();
        assert_eq!(out.rows[0][0], "1");

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "# Title\n\nBody\n\nAppended\n");
    }

    #[test]
    fn write_back_insert_append_preserves_missing_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.md");
        std::fs::write(&path, "# Title\n\nBody").unwrap();

        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        store
            .execute_sql_mut(
                "INSERT INTO blocks (document_id, block_type, content) VALUES (0, 'paragraph', 'Appended')",
            )
            .unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "# Title\n\nBody\n\nAppended");
    }

    #[test]
    fn write_back_insert_two_rows_same_after_pre_preserves_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");

        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();
        let pre = title_pre(&store);

        store
            .execute_sql_mut(&format!(
                "INSERT INTO blocks (document_id, block_type, content, after_pre) VALUES (0, 'paragraph', 'First', {pre}), (0, 'paragraph', 'Second', {pre})"
            ))
            .unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "# Title\n\nFirst\n\nSecond\n\nBody\n");
    }

    #[test]
    fn write_back_insert_mixed_anchors_same_document() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");

        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();
        let pre = title_pre(&store);

        store
            .execute_sql_mut(&format!(
                "INSERT INTO blocks (document_id, block_type, content, after_pre) VALUES \
                 (0, 'paragraph', 'AfterTitle', {pre}), \
                 (0, 'paragraph', 'AtEnd', NULL)"
            ))
            .unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "# Title\n\nAfterTitle\n\nBody\n\nAtEnd\n");
    }

    #[test]
    fn write_back_insert_multi_row_different_documents() {
        let dir = tempfile::tempdir().unwrap();
        let path_a = write_md(&dir, "a.md", "# A\n\nBodyA\n");
        let path_b = write_md(&dir, "b.md", "# B\n\nBodyB\n");

        let mut store = DocumentStore::new();
        store.add_file(&path_a).unwrap();
        store.add_file(&path_b).unwrap();

        let out = store
            .execute_sql_mut(
                "INSERT INTO blocks (document_id, block_type, content) VALUES \
                 (0, 'paragraph', 'ExtraA'), (1, 'paragraph', 'ExtraB')",
            )
            .unwrap();
        assert_eq!(out.rows[0][0], "2");

        assert_eq!(
            std::fs::read_to_string(&path_a).unwrap(),
            "# A\n\nBodyA\n\nExtraA\n"
        );
        assert_eq!(
            std::fs::read_to_string(&path_b).unwrap(),
            "# B\n\nBodyB\n\nExtraB\n"
        );
    }

    #[test]
    fn write_back_insert_rejects_unsupported_block_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        let err = store
            .execute_sql_mut(
                "INSERT INTO blocks (document_id, block_type, content) VALUES (0, 'code', 'fn f(){}')",
            )
            .unwrap_err();
        assert!(err.to_string().contains("heading/paragraph"));
    }

    #[test]
    fn write_back_insert_rejects_missing_depth_for_heading() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        let err = store
            .execute_sql_mut(
                "INSERT INTO blocks (document_id, block_type, content) VALUES (0, 'heading', 'New')",
            )
            .unwrap_err();
        assert!(err.to_string().contains("depth"));
    }

    #[test]
    fn write_back_insert_rejects_depth_for_paragraph() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        let err = store
            .execute_sql_mut(
                "INSERT INTO blocks (document_id, block_type, content, depth) VALUES (0, 'paragraph', 'New', 2)",
            )
            .unwrap_err();
        assert!(err.to_string().contains("depth"));
    }

    #[test]
    fn write_back_insert_rejects_positional_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        let err = store
            .execute_sql_mut("INSERT INTO blocks VALUES (0, 'paragraph', 'New', NULL, NULL)")
            .unwrap_err();
        assert!(err.to_string().contains("column list"));
    }

    #[test]
    fn write_back_insert_rejects_unknown_after_pre() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        let err = store
            .execute_sql_mut(
                "INSERT INTO blocks (document_id, block_type, content, after_pre) VALUES (0, 'paragraph', 'New', 999)",
            )
            .unwrap_err();
        assert!(err.to_string().contains("after_pre"));
    }

    #[test]
    fn write_back_insert_rejects_unknown_document_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        let err = store
            .execute_sql_mut(
                "INSERT INTO blocks (document_id, block_type, content) VALUES (99, 'paragraph', 'New')",
            )
            .unwrap_err();
        assert!(err.to_string().contains("no such document"));
    }

    #[test]
    fn write_back_insert_rejects_document_with_no_source_path() {
        let mut store = DocumentStore::new();
        store.add_str("# Title\n\nBody\n").unwrap();

        let err = store
            .execute_sql_mut(
                "INSERT INTO blocks (document_id, block_type, content) VALUES (0, 'paragraph', 'New')",
            )
            .unwrap_err();
        assert!(err.to_string().contains("no source file"));
    }

    #[test]
    fn write_back_read_only_insert_into_blocks_still_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        let engine = SqlEngine::new(&store).unwrap();
        let err = engine
            .execute(
                "INSERT INTO blocks (document_id, block_type, content) VALUES (0, 'paragraph', 'New')",
            )
            .unwrap_err();
        assert!(err.to_string().contains("blocks"));
    }

    #[test]
    fn write_back_insert_into_custom_table_still_works_via_execute_sql_mut() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        store
            .execute_sql_mut("CREATE TABLE notes (name TEXT)")
            .unwrap();
        let out = store
            .execute_sql_mut("INSERT INTO notes (name) VALUES ('hello')")
            .unwrap();
        assert_eq!(out.rows[0][0], "1");
    }
}
