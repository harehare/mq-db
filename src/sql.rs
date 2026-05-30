//! Custom SQL execution engine for mqdb.
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
//! | `count(*)`/`min`/`max`/`sum`/`avg` | Aggregates |
//! | `lower`/`upper`/`length`/`coalesce` | Scalar utilities |
//!
//! # Example
//!
//! ```rust,no_run
//! use mqdb::{DocumentStore, SqlEngine};
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

use std::collections::HashMap;

use sqlparser::{
    ast::{
        BinaryOperator, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments,
        GroupByExpr, JoinConstraint, JoinOperator, LimitClause, ObjectNamePart, OrderByExpr,
        OrderByKind, Query, Select, SelectItem, SetExpr, Statement, TableFactor, UnaryOperator,
        Value as SqlValue, Values,
    },
    dialect::GenericDialect,
    parser::Parser,
};

use crate::{
    DocumentStore, MqdbError,
    block::{Block, BlockType, Properties, PropertyValue},
    document::Document,
    indexes::IndexHint,
};

// ─────────────────────────────────────────────────────────────────────────────
// Value — runtime value type
// ─────────────────────────────────────────────────────────────────────────────

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

// ─────────────────────────────────────────────────────────────────────────────
// Row — named tuple
// ─────────────────────────────────────────────────────────────────────────────

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

// ─────────────────────────────────────────────────────────────────────────────
// Output format helpers
// ─────────────────────────────────────────────────────────────────────────────

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
    let mut row = fields.iter().map(|f| csv_cell(f)).collect::<Vec<_>>().join(",");
    row.push('\n');
    row
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ─────────────────────────────────────────────────────────────────────────────
// QueryOutput
// ─────────────────────────────────────────────────────────────────────────────

/// The tabular output of a SQL query.
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
                let escaped = cell.replace('|', "\\|").replace('\n', " ").replace('\r', "");
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
                    widths[i] = widths[i].max(cell.chars().count().min(MAX_CELL));
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
                let truncated: String = if cell.chars().count() > MAX_CELL {
                    let mut s: String = cell.chars().take(MAX_CELL - 1).collect();
                    s.push('…');
                    s
                } else {
                    cell.to_string()
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

// ─────────────────────────────────────────────────────────────────────────────
// Serialisation helpers
// ─────────────────────────────────────────────────────────────────────────────

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

// ─────────────────────────────────────────────────────────────────────────────
// Virtual table materialisation
// ─────────────────────────────────────────────────────────────────────────────

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

// ─────────────────────────────────────────────────────────────────────────────
// Expression evaluation
// ─────────────────────────────────────────────────────────────────────────────

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
        // Subqueries are pre-resolved by resolve_subqueries before eval
        _ => Value::Null,
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
    if matches!(
        name.to_lowercase().as_str(),
        "count" | "sum" | "min" | "max" | "avg"
    ) {
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
        "lower" => args
            .first()
            .and_then(|v| v.as_str())
            .map(|s| Value::Str(s.to_lowercase()))
            .unwrap_or(Value::Null),
        "upper" => args
            .first()
            .and_then(|v| v.as_str())
            .map(|s| Value::Str(s.to_uppercase()))
            .unwrap_or(Value::Null),
        "length" | "len" => args
            .first()
            .and_then(|v| v.as_str())
            .map(|s| Value::Int(s.chars().count() as i64))
            .unwrap_or(Value::Null),
        "coalesce" => args
            .iter()
            .find(|v| !matches!(v, Value::Null))
            .cloned()
            .unwrap_or(Value::Null),
        _ => Value::Null,
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

// ─────────────────────────────────────────────────────────────────────────────
// SqlEngine
// ─────────────────────────────────────────────────────────────────────────────

/// Custom SQL execution engine backed by a [`DocumentStore`] reference.
///
/// Zero-copy: `SqlEngine::new` is O(1). All queries walk `Vec<Block>` directly.
pub struct SqlEngine<'a> {
    store: &'a DocumentStore,
}

impl<'a> SqlEngine<'a> {
    /// Create a new engine. O(1) — no data is copied.
    pub fn new(store: &'a DocumentStore) -> Result<Self, MqdbError> {
        Ok(Self { store })
    }

    /// Execute a SQL SELECT query against the store.
    pub fn execute(&self, sql: &str) -> Result<QueryOutput, MqdbError> {
        let stmts = Parser::parse_sql(&GenericDialect {}, sql)
            .map_err(|e| MqdbError::SqlParse(e.to_string()))?;
        let stmt = stmts
            .into_iter()
            .next()
            .ok_or_else(|| MqdbError::SqlParse("empty query".into()))?;
        match stmt {
            Statement::Query(q) => self.exec_query(&q),
            _ => Err(MqdbError::SqlExec(
                "only SELECT queries are supported".into(),
            )),
        }
    }

    fn exec_query(&self, query: &Query) -> Result<QueryOutput, MqdbError> {
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
        let hint = select
            .selection
            .as_ref()
            .map(analyze_where_for_index)
            .unwrap_or(IndexHint::FullScan);
        let mut rows = self.materialise_from_with_hint(&select.from, &hint)?;

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
    ) -> Result<Vec<Row>, MqdbError> {
        if from.is_empty() {
            return Ok(vec![Row {
                columns: vec![],
                values: vec![],
            }]);
        }
        let mut rows = self.table_rows_with_hint(&from[0].relation, hint)?;
        for join in &from[0].joins {
            // Joined tables always full-scan (join partner)
            let right = self.table_rows_with_hint(&join.relation, &IndexHint::FullScan)?;
            rows = cross_join(rows, right);
            match &join.join_operator {
                JoinOperator::Inner(JoinConstraint::On(on))
                | JoinOperator::Join(JoinConstraint::On(on))
                | JoinOperator::Left(JoinConstraint::On(on))
                | JoinOperator::LeftOuter(JoinConstraint::On(on)) => {
                    let resolved = self.resolve_subqueries(on)?;
                    rows.retain(|row| eval_expr(&resolved, row).is_truthy());
                }
                _ => {}
            }
        }
        for twj in from.iter().skip(1) {
            let right = self.table_rows_with_hint(&twj.relation, &IndexHint::FullScan)?;
            rows = cross_join(rows, right);
            for join in &twj.joins {
                let right2 = self.table_rows_with_hint(&join.relation, &IndexHint::FullScan)?;
                rows = cross_join(rows, right2);
            }
        }
        Ok(rows)
    }

    fn table_rows_with_hint(
        &self,
        factor: &TableFactor,
        hint: &IndexHint,
    ) -> Result<Vec<Row>, MqdbError> {
        let (table_name, alias) = match factor {
            TableFactor::Table { name, alias, .. } => {
                let n = name.0.last().map(ident_value).unwrap_or("").to_lowercase();
                let a = alias.as_ref().map(|a| a.name.value.clone());
                (n, a)
            }
            _ => return Err(MqdbError::SqlExec("unsupported FROM clause".into())),
        };

        match table_name.as_str() {
            "blocks" => {
                let prefix = alias.as_deref().unwrap_or("blocks");
                let mut rows = Vec::new();
                let mut global_idx: u32 = 0;

                for (doc, doc_idx) in self.store.documents_with_indexes() {
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
            other => Err(MqdbError::SqlExec(format!("unknown table: {other}"))),
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
        let mut key_index: HashMap<Vec<String>, usize> = HashMap::new();

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

// ─────────────────────────────────────────────────────────────────────────────
// Projection helpers
// ─────────────────────────────────────────────────────────────────────────────

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
        matches!(name.as_str(), "count" | "sum" | "min" | "max" | "avg")
    })
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
                        "count" => group_rows.len().to_string(),
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

// ─────────────────────────────────────────────────────────────────────────────
// Predicate pushdown analyser — extract IndexHint from WHERE clause
// ─────────────────────────────────────────────────────────────────────────────

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
                    if let Some(s) = val {
                        return IndexHint::LangExact(s);
                    }
                    IndexHint::FullScan
                }
                Some("depth") => {
                    if let Some(n) = int_val {
                        return IndexHint::DepthExact(n as u8);
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

// ─────────────────────────────────────────────────────────────────────────────
// BlockType::from_str helper
// ─────────────────────────────────────────────────────────────────────────────

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

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DocumentStore;

    fn make_store() -> DocumentStore {
        let mut s = DocumentStore::new();
        s.add_str(
            "# Doc\n\n## Architecture\n\nDetails\n\n```rust\nfn main(){}\n```\n\n## Other\n\nOther\n",
        )
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
        assert!(
            elapsed.as_millis() < 1,
            "SqlEngine::new took {}ms — should be O(1)",
            elapsed.as_millis()
        );
    }
}
