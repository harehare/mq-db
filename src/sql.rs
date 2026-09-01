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
//! SELECT id, path, title, tags, block_count, max_heading_depth,
//!        code_languages, frontmatter_keys FROM documents;
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
//! | `bm25(content, query)` | Okapi BM25 relevance score (IDF-weighted, corpus-wide; `k1=1.2`, `b=0.75`) |
//! | `count`/`min`/`max`/`sum`/`avg`/`group_concat`/`string_agg` | Aggregates (`count` and `group_concat`/`string_agg` support `DISTINCT`); `GROUP BY ... HAVING` filters on them |
//! | `lower`/`upper`/`length`/`trim`/`ltrim`/`rtrim`/`concat`/`concat_ws`/`replace`/`left`/`right`/`lpad`/`rpad`/`reverse`/`repeat`/`initcap`/`ascii`/`chr`/`instr`/`split_part`/`substring`/`substr`/`position` | String functions |
//! | `REGEXP`/`RLIKE` operator, `regexp_like`/`regexp_replace`/`regexp_extract` | Regular-expression matching |
//! | `abs`/`round`/`ceil`/`floor`/`trunc`/`mod`/`power`/`sqrt`/`sign`/`exp`/`ln`/`log`/`log10`/`log2`/`pi`/`greatest`/`least` | Numeric functions |
//! | `coalesce`/`ifnull`/`nullif` | Null handling |
//! | `typeof`/`now`/`current_timestamp`/`current_date`/`current_time`/`CASE WHEN` | Misc |
//! | `date_trunc`/`date_diff`/`date_add`/`date_sub`/`strftime`/`EXTRACT(field FROM ...)` | Date/time functions |
//!
//! `SELECT ... LIMIT n OFFSET m` pages through results; `UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT`
//! combine two `SELECT`s; `BEGIN`/`COMMIT`/`ROLLBACK` wrap a run of statements (custom tables and
//! views always roll back; `--write-back` block edits already committed to the source Markdown
//! file cannot be undone and are called out in the `ROLLBACK` result). `CREATE TABLE` accepts
//! `NOT NULL`/`UNIQUE`/`PRIMARY KEY` column and table constraints, enforced on `INSERT` (session-only,
//! not persisted across reload).
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

use regex::Regex;
use rustc_hash::FxHashMap;
use sqlparser::{
    ast::{
        AssignmentTarget, BinaryOperator, CaseWhen, CeilFloorKind, ColumnDef, ColumnOption,
        CreateTable, CreateView, DateTimeField, DuplicateTreatment, Expr, FromTable, Function,
        FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr, IndexColumn, Insert,
        JoinConstraint, JoinOperator, LimitClause, ObjectName, ObjectNamePart, ObjectType,
        OrderByExpr, OrderByKind, Query, Select, SelectItem, SetExpr, SetOperator, SetQuantifier,
        Statement, TableConstraint, TableFactor, TableFunctionArgs, TableObject, TableWithJoins,
        TrimWhereField, UnaryOperator, Value as SqlValue, Values,
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
    store::{CustomTableState, DatabaseAlias},
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

fn doc_to_row(doc: &Document) -> Row {
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

fn parse_display_value(s: &str) -> Value {
    if let Ok(n) = s.parse::<i64>() {
        Value::Int(n)
    } else if let Ok(f) = s.parse::<f64>() {
        Value::Float(f)
    } else {
        Value::Str(s.to_string())
    }
}

fn output_to_rows(out: &QueryOutput, prefix: &str) -> Vec<Row> {
    out.rows
        .iter()
        .map(|r| {
            qualify_row(
                Row {
                    columns: out.columns.clone(),
                    values: r.iter().map(|v| parse_display_value(v)).collect(),
                },
                prefix,
            )
        })
        .collect()
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

/// Table name for DDL/DML targets, which must be unqualified — writes
/// through an ATTACHed database's `<alias>.<table>` are not supported.
fn require_unqualified(name: &ObjectName) -> Result<String, MqdbError> {
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
fn ok_result() -> QueryOutput {
    QueryOutput {
        columns: vec!["result".to_string()],
        rows: vec![vec!["ok".to_string()]],
    }
}

const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

// Corpus-wide stats for `bm25()`, built once per `execute()` call.
thread_local! {
    static BM25_CORPUS: std::cell::RefCell<Option<std::rc::Rc<Bm25Corpus>>> =
        const { std::cell::RefCell::new(None) };
}

struct Bm25Corpus {
    n: u64,
    avgdl: f64,
    df: FxHashMap<String, u32>,
}

impl Bm25Corpus {
    fn build(indexes: &[DocumentIndex]) -> Self {
        let mut n: u64 = 0;
        let mut total_tokens: u64 = 0;
        let mut df: FxHashMap<String, u32> = FxHashMap::default();
        for idx in indexes {
            n += u64::from(idx.term.block_count);
            total_tokens += idx.term.total_token_count;
            for (term, count) in idx.term.document_frequencies() {
                *df.entry(term.to_string()).or_default() += count;
            }
        }
        let avgdl = if n == 0 {
            0.0
        } else {
            total_tokens as f64 / n as f64
        };
        Self { n, avgdl, df }
    }

    fn score(&self, content: &str, query: &str) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        let content_terms = tokenize(content);
        let dl = content_terms.len() as f64;
        if dl == 0.0 {
            return 0.0;
        }
        let mut freq: FxHashMap<&str, u32> = FxHashMap::default();
        for t in &content_terms {
            *freq.entry(t.as_str()).or_default() += 1;
        }
        tokenize(query)
            .iter()
            .map(|qt| {
                let f = f64::from(*freq.get(qt.as_str()).unwrap_or(&0));
                if f == 0.0 {
                    return 0.0;
                }
                let df = f64::from(*self.df.get(qt.as_str()).unwrap_or(&0));
                let idf = ((self.n as f64 - df + 0.5) / (df + 0.5) + 1.0).ln();
                let numer = f * (BM25_K1 + 1.0);
                let denom = f + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / self.avgdl);
                idf * numer / denom
            })
            .sum()
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
        "bm25" => {
            let (Some(content), Some(query)) = (
                args.first().and_then(Value::as_str),
                args.get(1).and_then(Value::as_str),
            ) else {
                return Value::Float(0.0);
            };
            let score = BM25_CORPUS.with(|c| {
                c.borrow()
                    .as_ref()
                    .map(|corpus| corpus.score(content, query))
            });
            Value::Float(score.unwrap_or(0.0))
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
        "regexp_like" => {
            let (Some(s), Some(pat)) = (
                args.first().and_then(Value::as_str),
                args.get(1).and_then(Value::as_str),
            ) else {
                return Value::Bool(false);
            };
            compile_regex(pat)
                .map(|re| Value::Bool(re.is_match(s)))
                .unwrap_or(Value::Bool(false))
        }
        "regexp_replace" => {
            if args.len() < 3 {
                return Value::Null;
            }
            match (args[0].as_str(), args[1].as_str(), args[2].as_str()) {
                (Some(s), Some(pat), Some(rep)) => compile_regex(pat)
                    .map(|re| Value::Str(re.replace_all(s, rep).into_owned()))
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            }
        }
        "regexp_extract" => {
            let (Some(s), Some(pat)) = (
                args.first().and_then(Value::as_str),
                args.get(1).and_then(Value::as_str),
            ) else {
                return Value::Null;
            };
            let group = args.get(2).and_then(Value::as_i64).unwrap_or(0).max(0) as usize;
            compile_regex(pat)
                .and_then(|re| re.captures(s))
                .and_then(|caps| caps.get(group))
                .map(|m| Value::Str(m.as_str().to_string()))
                .unwrap_or(Value::Null)
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
        "date_trunc" => match (
            args.first().and_then(Value::as_str),
            args.get(1).and_then(Value::as_str),
        ) {
            (Some(unit), Some(date)) => eval_date_trunc(unit, date),
            _ => Value::Null,
        },
        "date_diff" => {
            match (
                args.first().and_then(Value::as_str),
                args.get(1).and_then(Value::as_str),
                args.get(2).and_then(Value::as_str),
            ) {
                (Some(unit), Some(d1), Some(d2)) => eval_date_diff(unit, d1, d2),
                _ => Value::Null,
            }
        }
        "date_add" => match (
            args.first().and_then(Value::as_str),
            args.get(1).and_then(Value::as_i64),
            args.get(2).and_then(Value::as_str),
        ) {
            (Some(date), Some(n), Some(unit)) => eval_date_add(date, n, unit),
            (Some(date), Some(n), None) => eval_date_add(date, n, "day"),
            _ => Value::Null,
        },
        "date_sub" => match (
            args.first().and_then(Value::as_str),
            args.get(1).and_then(Value::as_i64),
            args.get(2).and_then(Value::as_str),
        ) {
            (Some(date), Some(n), Some(unit)) => eval_date_add(date, -n, unit),
            (Some(date), Some(n), None) => eval_date_add(date, -n, "day"),
            _ => Value::Null,
        },
        "strftime" => match (
            args.first().and_then(Value::as_str),
            args.get(1).and_then(Value::as_str),
        ) {
            (Some(fmt), Some(date)) => eval_strftime(fmt, date),
            _ => Value::Null,
        },
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

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(y) => 29,
        2 => 28,
        _ => 30,
    }
}

fn parse_datetime(s: &str) -> Option<(i64, i64)> {
    let s = s.trim();
    let (date_part, time_part) = match s.find(['T', ' ']) {
        Some(idx) => (&s[..idx], Some(&s[idx + 1..])),
        None => (s, None),
    };
    let mut parts = date_part.splitn(3, '-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, m, d);
    let secs = match time_part {
        Some(tp) => {
            let tp = tp.trim_end_matches('Z');
            let tp = tp.split(['+']).next().unwrap_or(tp);
            let tp = match tp.rfind('-') {
                Some(pos) => &tp[..pos],
                None => tp,
            };
            let tp = tp.split('.').next().unwrap_or(tp);
            let mut tparts = tp.splitn(3, ':');
            let h: i64 = tparts.next()?.parse().ok()?;
            let mi: i64 = tparts.next().unwrap_or("0").parse().ok()?;
            let se: i64 = tparts.next().unwrap_or("0").parse().ok()?;
            h * 3600 + mi * 60 + se
        }
        None => 0,
    };
    Some((days, secs))
}

fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn format_datetime(days: i64, secs: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

fn weekday_from_days(days: i64) -> u32 {
    (((days % 7) + 11) % 7) as u32
}

const WEEKDAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn eval_extract(field: &DateTimeField, expr: &Expr, row: &Row) -> Value {
    let Some(s) = eval_expr(expr, row).as_str().map(str::to_string) else {
        return Value::Null;
    };
    let Some((days, secs)) = parse_datetime(&s) else {
        return Value::Null;
    };
    let (y, m, d) = civil_from_days(days);
    match field {
        DateTimeField::Year | DateTimeField::Years => Value::Int(y),
        DateTimeField::Month | DateTimeField::Months => Value::Int(m as i64),
        DateTimeField::Day | DateTimeField::Days => Value::Int(d as i64),
        DateTimeField::Hour | DateTimeField::Hours => Value::Int(secs / 3600),
        DateTimeField::Minute | DateTimeField::Minutes => Value::Int((secs / 60) % 60),
        DateTimeField::Second | DateTimeField::Seconds => Value::Int(secs % 60),
        DateTimeField::Dow => Value::Int(weekday_from_days(days) as i64),
        DateTimeField::Doy => Value::Int(days - days_from_civil(y, 1, 1) + 1),
        DateTimeField::Epoch => Value::Int(days * 86400 + secs),
        _ => Value::Null,
    }
}

fn add_months(days: i64, n: i64) -> i64 {
    let (y, m, d) = civil_from_days(days);
    let total_months = y * 12 + (m as i64 - 1) + n;
    let ny = total_months.div_euclid(12);
    let nm = (total_months.rem_euclid(12) + 1) as u32;
    let nd = d.min(days_in_month(ny, nm));
    days_from_civil(ny, nm, nd)
}

fn eval_date_trunc(unit: &str, date: &str) -> Value {
    let Some((days, secs)) = parse_datetime(date) else {
        return Value::Null;
    };
    let (y, m, _) = civil_from_days(days);
    match unit.to_lowercase().as_str() {
        "year" => Value::Str(format_date(days_from_civil(y, 1, 1))),
        "month" => Value::Str(format_date(days_from_civil(y, m, 1))),
        "day" => Value::Str(format_date(days)),
        "hour" => Value::Str(format_datetime(days, (secs / 3600) * 3600)),
        "minute" => Value::Str(format_datetime(days, (secs / 60) * 60)),
        _ => Value::Null,
    }
}

fn eval_date_diff(unit: &str, date1: &str, date2: &str) -> Value {
    let (Some((days1, secs1)), Some((days2, secs2))) =
        (parse_datetime(date1), parse_datetime(date2))
    else {
        return Value::Null;
    };
    match unit.to_lowercase().as_str() {
        "day" => Value::Int(days2 - days1),
        "second" => Value::Int((days2 - days1) * 86400 + (secs2 - secs1)),
        "month" => {
            let (y1, m1, _) = civil_from_days(days1);
            let (y2, m2, _) = civil_from_days(days2);
            Value::Int((y2 * 12 + m2 as i64) - (y1 * 12 + m1 as i64))
        }
        "year" => {
            let (y1, _, _) = civil_from_days(days1);
            let (y2, _, _) = civil_from_days(days2);
            Value::Int(y2 - y1)
        }
        _ => Value::Null,
    }
}

fn eval_date_add(date: &str, n: i64, unit: &str) -> Value {
    let Some((days, secs)) = parse_datetime(date) else {
        return Value::Null;
    };
    let with_time = secs != 0 || date.contains([' ', 'T']);
    let render = |days: i64, secs: i64| {
        if with_time {
            format_datetime(days, secs)
        } else {
            format_date(days)
        }
    };
    match unit.to_lowercase().as_str() {
        "day" => Value::Str(render(days + n, secs)),
        "month" => Value::Str(render(add_months(days, n), secs)),
        "year" => Value::Str(render(add_months(days, n * 12), secs)),
        "hour" => Value::Str(render(days, secs + n * 3600)),
        "minute" => Value::Str(render(days, secs + n * 60)),
        "second" => Value::Str(render(days, secs + n)),
        _ => Value::Null,
    }
}

fn eval_strftime(format: &str, date: &str) -> Value {
    let Some((days, secs)) = parse_datetime(date) else {
        return Value::Null;
    };
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    let wd = weekday_from_days(days) as usize;
    let doy = days - days_from_civil(y, 1, 1) + 1;
    let mut out = String::new();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&format!("{y:04}")),
            Some('m') => out.push_str(&format!("{m:02}")),
            Some('d') => out.push_str(&format!("{d:02}")),
            Some('H') => out.push_str(&format!("{h:02}")),
            Some('M') => out.push_str(&format!("{mi:02}")),
            Some('S') => out.push_str(&format!("{s:02}")),
            Some('j') => out.push_str(&format!("{doy:03}")),
            Some('w') => out.push_str(&wd.to_string()),
            Some('A') => out.push_str(WEEKDAY_NAMES[wd]),
            Some('a') => out.push_str(&WEEKDAY_NAMES[wd][..3]),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    Value::Str(out)
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

fn compile_regex(pattern: &str) -> Option<Regex> {
    Regex::new(pattern).ok()
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
    view_stack: std::cell::RefCell<Vec<String>>,
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
            view_stack: std::cell::RefCell::new(Vec::new()),
        })
    }

    fn documents_with_indexes(&self) -> impl Iterator<Item = (&Document, &DocumentIndex)> {
        self.store.documents().iter().zip(self.indexes.iter())
    }

    /// Sum of `hint`'s predicted matching-block count across every document,
    /// read directly from each document's already-built secondary index —
    /// no scanning. Shared by [`Self::choose_best_hint`] and `EXPLAIN`'s plan
    /// describer, so both report the same numbers.
    fn estimate_hint_cost(&self, hint: &IndexHint) -> u64 {
        self.documents_with_indexes()
            .map(|(doc, idx)| {
                hint.resolve(idx)
                    .map(|v| v.len() as u64)
                    .unwrap_or(doc.blocks.len() as u64)
            })
            .sum()
    }

    /// Cheapest of several [`IndexHint`] candidates for the same WHERE
    /// clause, by [`Self::estimate_hint_cost`] (ties keep the first
    /// candidate, for deterministic output). `FullScan` if there are none.
    fn choose_best_hint(&self, candidates: Vec<IndexHint>) -> IndexHint {
        candidates
            .into_iter()
            .min_by_key(|h| self.estimate_hint_cost(h))
            .unwrap_or(IndexHint::FullScan)
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
        if upper.starts_with("DETACH") {
            return self.exec_detach(trimmed);
        }

        let stmts = Parser::parse_sql(&GenericDialect {}, sql)
            .map_err(|e| MqdbError::SqlParse(e.to_string()))?;
        let stmt = stmts
            .into_iter()
            .next()
            .ok_or_else(|| MqdbError::SqlParse("empty query".into()))?;

        struct Bm25CorpusGuard;
        impl Drop for Bm25CorpusGuard {
            fn drop(&mut self) {
                BM25_CORPUS.with(|c| *c.borrow_mut() = None);
            }
        }
        let _bm25_guard = if trimmed.to_ascii_lowercase().contains("bm25(") {
            BM25_CORPUS.with(|c| {
                *c.borrow_mut() = Some(std::rc::Rc::new(Bm25Corpus::build(&self.indexes)))
            });
            Some(Bm25CorpusGuard)
        } else {
            None
        };

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
            Statement::Drop {
                object_type: ObjectType::View,
                names,
                if_exists,
                ..
            } => self.exec_drop_views(&names, if_exists),
            Statement::CreateView(cv) => self.exec_create_view(&cv),
            Statement::Explain {
                analyze, statement, ..
            } => self.exec_explain(analyze, &statement),
            Statement::Vacuum(_) => Err(MqdbError::SqlExec(
                "VACUUM is a CLI command, not a SQL statement here — run `mq-db vacuum --db <path>`".into(),
            )),
            Statement::StartTransaction { .. } => {
                self.store.begin_transaction()?;
                Ok(ok_result())
            }
            Statement::Commit { .. } => {
                self.store.commit_transaction()?;
                Ok(ok_result())
            }
            Statement::Rollback { .. } => {
                self.store.rollback_transaction_tables_only()?;
                Ok(ok_result())
            }
            Statement::AttachDatabase {
                schema_name,
                database_file_name,
                ..
            } => {
                let path = expr_str_val(&database_file_name).ok_or_else(|| {
                    MqdbError::SqlExec(
                        "ATTACH DATABASE: file path must be a string literal".into(),
                    )
                })?;
                let alias = DatabaseAlias::parse(&schema_name.value)?;
                self.store.attach(alias, std::path::Path::new(&path))?;
                Ok(ok_result())
            }
            _ => Err(MqdbError::SqlExec(
                "unsupported statement; supported: SELECT, CREATE TABLE, INSERT INTO, DROP TABLE, CREATE VIEW, DROP VIEW, ATTACH DATABASE, DETACH, DESC, SHOW TABLES, EXPLAIN, BEGIN, COMMIT, ROLLBACK".into(),
            )),
        }
    }

    /// `DETACH [DATABASE] <alias>` — not parsed generically by `sqlparser`
    /// under `GenericDialect`, so handled here like `DESC`/`SHOW TABLES`.
    fn exec_detach(&self, trimmed: &str) -> Result<QueryOutput, MqdbError> {
        let mut tokens = trimmed.split_whitespace().skip(1); // skip "DETACH"
        let mut tok = tokens.next();
        if let Some(t) = tok
            && t.eq_ignore_ascii_case("DATABASE")
        {
            tok = tokens.next();
        }
        let alias = tok.ok_or_else(|| {
            MqdbError::SqlParse("malformed DETACH statement: expected a database alias".into())
        })?;
        if tokens.next().is_some() {
            return Err(MqdbError::SqlParse(
                "malformed DETACH statement: unexpected trailing tokens".into(),
            ));
        }
        if !self.store.detach(alias) {
            return Err(MqdbError::SqlExec(format!(
                "database alias '{alias}' is not attached"
            )));
        }
        Ok(ok_result())
    }

    fn exec_explain(&self, analyze: bool, inner: &Statement) -> Result<QueryOutput, MqdbError> {
        let Statement::Query(query) = inner else {
            return Err(MqdbError::SqlExec(
                "EXPLAIN only supports SELECT queries".into(),
            ));
        };

        let mut rows: Vec<(String, String)> = Vec::new();
        self.describe_query(query, "query", &mut Vec::new(), &mut rows);

        if analyze {
            let start = std::time::Instant::now();
            let out = self.exec_query(query)?;
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            rows.push(("actual:elapsed".to_string(), format!("{elapsed_ms:.3}ms")));
            rows.push((
                "actual:rows".to_string(),
                format!("{} row(s) returned", out.rows.len()),
            ));
            self.explain_analyze_scan_stats(query, &mut rows);
        }

        Ok(QueryOutput {
            columns: vec!["step".to_string(), "detail".to_string()],
            rows: rows.into_iter().map(|(s, d)| vec![s, d]).collect(),
        })
    }

    fn describe_query(
        &self,
        query: &Query,
        label: &str,
        cte_names: &mut Vec<String>,
        out: &mut Vec<(String, String)>,
    ) {
        let mut local_ctes = 0;
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                let name = cte.alias.name.value.to_lowercase();
                self.describe_query(&cte.query, &format!("cte:{name}"), cte_names, out);
                cte_names.push(name);
                local_ctes += 1;
            }
        }
        match query.body.as_ref() {
            SetExpr::Select(select) => {
                let limit = limit_expr_of(query);
                let offset = offset_expr_of(query);
                self.describe_select(
                    select,
                    &query.order_by,
                    (limit.as_ref(), offset.as_ref()),
                    label,
                    cte_names,
                    out,
                );
            }
            _ => {
                out.push((
                    label.to_string(),
                    "non-SELECT set expression (VALUES/UNION/...) — no index plan".to_string(),
                ));
            }
        }
        cte_names.truncate(cte_names.len() - local_ctes);
    }

    fn describe_select(
        &self,
        select: &Select,
        order_by: &Option<sqlparser::ast::OrderBy>,
        limit_offset: (Option<&Expr>, Option<&Expr>),
        label: &str,
        cte_names: &[String],
        out: &mut Vec<(String, String)>,
    ) {
        let (limit, offset) = limit_offset;
        if select.from.is_empty() {
            out.push((
                format!("{label}:from"),
                "no FROM clause (constant SELECT)".to_string(),
            ));
        } else {
            let twj = &select.from[0];
            match table_factor_ident(&twj.relation) {
                Some(table_name) => {
                    let is_cte = cte_names.contains(&table_name);
                    let kind = if is_cte {
                        "cte"
                    } else {
                        match table_name.as_str() {
                            "blocks" => "blocks",
                            "documents" => "documents",
                            other
                                if self.store.custom_tables.read().unwrap().contains_key(other) =>
                            {
                                "custom table"
                            }
                            _ => "unknown",
                        }
                    };
                    out.push((format!("{label}:from"), format!("{table_name} ({kind})")));

                    let single_unjoined_from = select.from.len() == 1 && twj.joins.is_empty();
                    let is_plain_blocks = kind == "blocks";

                    match select.selection.as_ref() {
                        Some(we) if is_plain_blocks => {
                            let candidates = candidate_hints_for_where(we);
                            let hint = self.choose_best_hint(candidates.clone());
                            if candidates.len() <= 1 {
                                out.push((
                                    format!("{label}:where"),
                                    format!("{} used", describe_hint(&hint)),
                                ));
                            } else {
                                let mut costed: Vec<(u64, &IndexHint)> = candidates
                                    .iter()
                                    .map(|h| (self.estimate_hint_cost(h), h))
                                    .collect();
                                costed.sort_by_key(|(cost, _)| *cost);
                                let others: Vec<String> = costed
                                    .iter()
                                    .filter(|(_, h)| **h != hint)
                                    .map(|(cost, h)| format!("{} [est. {cost}]", describe_hint(h)))
                                    .collect();
                                out.push((
                                    format!("{label}:where"),
                                    format!(
                                        "{} used (est. {} row(s); also considered: {})",
                                        describe_hint(&hint),
                                        self.estimate_hint_cost(&hint),
                                        others.join(", ")
                                    ),
                                ));
                            }

                            if single_unjoined_from {
                                let fields = zone_map_candidate_fields(we);
                                if fields.is_empty() {
                                    out.push((
                                        format!("{label}:zone-map"),
                                        "not eligible (no lang=/depth=/heading-content= conjunct)"
                                            .to_string(),
                                    ));
                                } else {
                                    out.push((
                                        format!("{label}:zone-map"),
                                        format!("eligible via {}", fields.join(", ")),
                                    ));
                                }
                            } else {
                                out.push((
                                    format!("{label}:zone-map"),
                                    "disabled (JOIN or multiple FROM tables)".to_string(),
                                ));
                            }

                            let where_fully_indexed = matches!(hint, IndexHint::TermMatch(_))
                                && single_unjoined_from
                                && matches!(unwrap_nested(we), Expr::Function(_));
                            out.push((
                                format!("{label}:where-recheck"),
                                if where_fully_indexed {
                                    "skipped (fully covered by TermIndex match())".to_string()
                                } else {
                                    "row-by-row (full predicate re-evaluated after scan)"
                                        .to_string()
                                },
                            ));
                        }
                        Some(_) => {
                            out.push((
                                format!("{label}:where"),
                                "row-by-row (no secondary index for this table)".to_string(),
                            ));
                        }
                        None => {
                            out.push((format!("{label}:where"), "none — full scan".to_string()));
                        }
                    }
                }
                None => {
                    out.push((
                        format!("{label}:from"),
                        "unsupported FROM clause (subquery/derived table)".to_string(),
                    ));
                }
            }

            for (i, join) in twj.joins.iter().enumerate() {
                let jname = table_factor_ident(&join.relation).unwrap_or_else(|| "?".to_string());
                let strategy = match &join.join_operator {
                    JoinOperator::Inner(JoinConstraint::On(on))
                    | JoinOperator::Join(JoinConstraint::On(on))
                    | JoinOperator::Left(JoinConstraint::On(on))
                    | JoinOperator::LeftOuter(JoinConstraint::On(on)) => describe_join_strategy(on),
                    _ => "cross join (no ON, or unsupported join type)".to_string(),
                };
                out.push((
                    format!("{label}:join[{i}]"),
                    format!("{jname}: {strategy} — join partner always full-scanned"),
                ));
            }

            for twj_extra in select.from.iter().skip(1) {
                let n = table_factor_ident(&twj_extra.relation).unwrap_or_else(|| "?".to_string());
                out.push((
                    format!("{label}:from+"),
                    format!("{n}: cross join (comma-separated FROM)"),
                ));
            }
        }

        let group_by_exprs: Vec<Expr> = match &select.group_by {
            GroupByExpr::Expressions(exprs, _) => exprs.clone(),
            _ => vec![],
        };
        if !group_by_exprs.is_empty() {
            out.push((
                format!("{label}:group-by"),
                format!("{} key(s)", group_by_exprs.len()),
            ));
        }

        if let Some(ob) = order_by
            && let OrderByKind::Expressions(exprs) = &ob.kind
        {
            let desc = exprs
                .iter()
                .map(|e| {
                    format!(
                        "{} {}",
                        e.expr,
                        if e.options.asc == Some(false) {
                            "DESC"
                        } else {
                            "ASC"
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            out.push((format!("{label}:order-by"), desc));
        }

        if let Some(lim) = limit {
            out.push((format!("{label}:limit"), format!("{lim}")));
        }
        if let Some(off) = offset {
            out.push((format!("{label}:offset"), format!("{off}")));
        }
    }

    fn explain_analyze_scan_stats(&self, query: &Query, out: &mut Vec<(String, String)>) {
        let SetExpr::Select(select) = query.body.as_ref() else {
            return;
        };
        if select.from.len() != 1 || !select.from[0].joins.is_empty() {
            return;
        }
        let Some(table_name) = table_factor_ident(&select.from[0].relation) else {
            return;
        };
        let shadowed_by_cte = query.with.as_ref().is_some_and(|w| {
            w.cte_tables
                .iter()
                .any(|c| c.alias.name.value.eq_ignore_ascii_case("blocks"))
        });
        if table_name != "blocks" || shadowed_by_cte {
            return;
        }

        let where_expr = select.selection.as_ref();
        let hint = where_expr
            .map(|we| self.choose_best_hint(candidate_hints_for_where(we)))
            .unwrap_or(IndexHint::FullScan);

        let (mut docs_total, mut docs_skipped, mut candidate_rows, mut total_rows) =
            (0u32, 0u32, 0u32, 0u32);
        for (doc, doc_idx) in self.documents_with_indexes() {
            docs_total += 1;
            total_rows += doc.blocks.len() as u32;
            if let Some(we) = where_expr
                && zone_map_skip(&doc.zone_maps, we)
            {
                docs_skipped += 1;
                continue;
            }
            candidate_rows += hint
                .resolve(doc_idx)
                .map(|v| v.len() as u32)
                .unwrap_or(doc.blocks.len() as u32);
        }
        out.push((
            "actual".to_string(),
            format!("{docs_skipped}/{docs_total} document(s) skipped by zone map"),
        ));
        out.push((
            "actual".to_string(),
            format!(
                "{candidate_rows} candidate row(s) from index/scan (of {total_rows} total blocks)"
            ),
        ));
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
                ("block_count", "integer"),
                ("max_heading_depth", "integer"),
                ("code_languages", "text"),
                ("frontmatter_keys", "text"),
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
        drop(guard);
        if let Some(sql_text) = self.store.views.read().unwrap().get(table_name).cloned() {
            let out = self.exec_view_query(table_name, &sql_text)?;
            let rows = out
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

        let guard = self.store.views.read().unwrap();
        let mut views: Vec<String> = guard.keys().cloned().collect();
        drop(guard);
        views.sort();
        rows.extend(views.into_iter().map(|n| vec![n, "view".to_string()]));

        Ok(QueryOutput {
            columns: vec!["table".to_string(), "kind".to_string()],
            rows,
        })
    }

    fn exec_create_table(&self, ct: &CreateTable) -> Result<QueryOutput, MqdbError> {
        let table_name = require_unqualified(&ct.name)?;
        if matches!(table_name.as_str(), "blocks" | "documents") {
            return Err(MqdbError::SqlExec(format!(
                "cannot override built-in table '{table_name}'"
            )));
        }
        if self.store.views.read().unwrap().contains_key(&table_name) {
            return Err(MqdbError::SqlExec(format!(
                "'{table_name}' is already defined as a view"
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
                    not_null: vec![],
                    unique: vec![],
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
        let (not_null, unique) = table_constraints(&columns, &ct.columns, &ct.constraints);
        self.store.custom_tables.write().unwrap().insert(
            table_name,
            CustomTableState {
                columns,
                rows: vec![],
                first_row_page: 0,
                last_row_page: 0,
                not_null,
                unique,
            },
        );
        self.store.try_flush_catalog_to_storage();
        Ok(QueryOutput {
            columns: vec!["result".to_string()],
            rows: vec![vec!["ok".to_string()]],
        })
    }

    fn exec_create_view(&self, cv: &CreateView) -> Result<QueryOutput, MqdbError> {
        let view_name = require_unqualified(&cv.name)?;
        if matches!(view_name.as_str(), "blocks" | "documents") {
            return Err(MqdbError::SqlExec(format!(
                "cannot override built-in table '{view_name}'"
            )));
        }
        if self
            .store
            .custom_tables
            .read()
            .unwrap()
            .contains_key(&view_name)
        {
            return Err(MqdbError::SqlExec(format!(
                "'{view_name}' is already defined as a table"
            )));
        }
        if !cv.columns.is_empty() {
            return Err(MqdbError::SqlExec(
                "explicit view columns (CREATE VIEW v (a, b) AS ...) are not supported".into(),
            ));
        }

        let already_exists = self.store.views.read().unwrap().contains_key(&view_name);
        if already_exists && !cv.or_replace {
            if cv.if_not_exists {
                return Ok(QueryOutput {
                    columns: vec!["result".to_string()],
                    rows: vec![vec!["already exists".to_string()]],
                });
            }
            return Err(MqdbError::SqlExec(format!(
                "view '{view_name}' already exists"
            )));
        }

        // Validate the query now so a bad CREATE VIEW fails immediately
        // rather than on first use.
        self.exec_query(&cv.query)?;
        let sql_text = cv.query.to_string();
        if sql_text.len() > u16::MAX as usize {
            return Err(MqdbError::SqlExec(
                "view query is too long to persist (max 65535 bytes)".into(),
            ));
        }

        self.store
            .views
            .write()
            .unwrap()
            .insert(view_name, sql_text);
        self.store.try_flush_catalog_to_storage();
        Ok(QueryOutput {
            columns: vec!["result".to_string()],
            rows: vec![vec!["ok".to_string()]],
        })
    }

    fn exec_drop_views(
        &self,
        names: &[ObjectName],
        if_exists: bool,
    ) -> Result<QueryOutput, MqdbError> {
        let dropped = {
            let mut guard = self.store.views.write().unwrap();
            let mut dropped = 0usize;
            for name in names {
                let view_name = require_unqualified(name)?;
                if matches!(view_name.as_str(), "blocks" | "documents") {
                    return Err(MqdbError::SqlExec(format!(
                        "cannot drop built-in table '{view_name}'"
                    )));
                }
                if guard.remove(&view_name).is_some() {
                    dropped += 1;
                } else if !if_exists {
                    return Err(MqdbError::SqlExec(format!(
                        "view '{view_name}' does not exist"
                    )));
                }
            }
            dropped
        };
        self.store.try_flush_catalog_to_storage();
        Ok(QueryOutput {
            columns: vec!["result".to_string()],
            rows: vec![vec![format!("{dropped} view(s) dropped")]],
        })
    }

    fn exec_insert(&self, ins: &Insert) -> Result<QueryOutput, MqdbError> {
        let table_name = match &ins.table {
            TableObject::TableName(name) => require_unqualified(name)?,
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
                new_rows.push(row);
            }

            for row in &new_rows {
                for &idx in &state.not_null {
                    if row.get(idx).is_none_or(|v| v.is_empty() || v == "NULL") {
                        return Err(MqdbError::SqlExec(format!(
                            "NOT NULL constraint failed: {}.{}",
                            table_name, state.columns[idx]
                        )));
                    }
                }
            }
            for group in &state.unique {
                let key = |row: &[String]| -> Vec<String> {
                    group.iter().map(|&i| row[i].clone()).collect()
                };
                let mut seen: std::collections::HashSet<Vec<String>> =
                    state.rows.iter().map(|r| key(r)).collect();
                for row in &new_rows {
                    let k = key(row);
                    if !seen.insert(k) {
                        let cols: Vec<&str> =
                            group.iter().map(|&i| state.columns[i].as_str()).collect();
                        return Err(MqdbError::SqlExec(format!(
                            "UNIQUE constraint failed: {}.{}",
                            table_name,
                            cols.join(", ")
                        )));
                    }
                }
            }

            state.rows.extend(new_rows.iter().cloned());
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
                let table_name = require_unqualified(name)?;
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

        self.cte_scopes.borrow_mut().push(FxHashMap::default());
        let result = (|| {
            for cte in &with.cte_tables {
                if !cte.alias.columns.is_empty() {
                    return Err(MqdbError::SqlExec(
                        "CTE column aliases (WITH x(a, b) AS ...) are not supported".into(),
                    ));
                }
                let name = cte.alias.name.value.to_lowercase();
                // `name` isn't in scope yet, so no self-reference outside
                // `exec_cte_body`'s own recursive-CTE handling.
                let out = self.exec_cte_body(&name, &cte.query, with.recursive)?;
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

    /// Dispatches a `WITH [RECURSIVE]` CTE body: the recursive fixed-point
    /// driver ([`Self::exec_recursive_cte`]) if `recursive` is set and
    /// `query.body` has the standard `<anchor> UNION [ALL] <term
    /// referencing name>` shape, otherwise the plain evaluate-once path
    /// (also covers a `WITH RECURSIVE` block containing a CTE that doesn't
    /// actually self-reference).
    fn exec_cte_body(
        &self,
        name: &str,
        query: &Query,
        recursive: bool,
    ) -> Result<QueryOutput, MqdbError> {
        if recursive
            && let SetExpr::SetOperation {
                left,
                op: SetOperator::Union,
                set_quantifier,
                right,
            } = query.body.as_ref()
            && let (SetExpr::Select(anchor), SetExpr::Select(term)) =
                (left.as_ref(), right.as_ref())
            && select_references_table(term, name)
        {
            if select_references_table(anchor, name) {
                return Err(MqdbError::SqlExec(format!(
                    "recursive CTE '{name}': the anchor (first) branch must not reference '{name}' itself"
                )));
            }
            let all = matches!(set_quantifier, SetQuantifier::All);
            return self.exec_recursive_cte(name, anchor, term, all);
        }
        self.exec_query(query)
    }

    /// Iterative fixed-point evaluation of a recursive CTE: run `anchor`
    /// once to seed the result, then repeatedly bind `name` to only the
    /// *previous iteration's new rows* (not the whole accumulated result —
    /// standard recursive-CTE semantics) and run `recursive` again, until it
    /// produces no new rows. `all` selects `UNION ALL` (no dedup) vs. `UNION`
    /// (dedup against everything produced so far, which is also what drives
    /// termination for a query that would otherwise cycle).
    fn exec_recursive_cte(
        &self,
        name: &str,
        anchor: &Select,
        recursive: &Select,
        all: bool,
    ) -> Result<QueryOutput, MqdbError> {
        let anchor_out = self.exec_select(anchor, &None, None, None)?;
        let columns = anchor_out.columns.clone();
        let mut result_rows = anchor_out.rows.clone();
        let mut working = anchor_out.rows;
        let mut seen: std::collections::HashSet<Vec<String>> = if all {
            std::collections::HashSet::new()
        } else {
            result_rows.iter().cloned().collect()
        };

        let mut iterations = 0usize;
        while !working.is_empty() {
            iterations += 1;
            if iterations > MAX_RECURSIVE_CTE_ITERATIONS {
                return Err(MqdbError::SqlExec(format!(
                    "recursive CTE '{name}' exceeded {MAX_RECURSIVE_CTE_ITERATIONS} iterations \
                     — check that the recursive branch's WHERE clause terminates"
                )));
            }

            self.cte_scopes.borrow_mut().push(
                [(
                    name.to_string(),
                    std::rc::Rc::new(QueryOutput {
                        columns: columns.clone(),
                        rows: working.clone(),
                    }),
                )]
                .into_iter()
                .collect(),
            );
            let step = self.exec_select(recursive, &None, None, None);
            self.cte_scopes.borrow_mut().pop();
            let step_out = step?;

            if step_out.columns.len() != columns.len() {
                return Err(MqdbError::SqlExec(format!(
                    "recursive CTE '{name}': anchor and recursive branch select \
                     different numbers of columns"
                )));
            }

            working = step_out
                .rows
                .into_iter()
                .filter(|row| all || seen.insert(row.clone()))
                .collect();
            result_rows.extend(working.iter().cloned());
        }

        Ok(QueryOutput {
            columns,
            rows: result_rows,
        })
    }

    fn exec_query_body(&self, query: &Query) -> Result<QueryOutput, MqdbError> {
        if let SetExpr::Select(select) = query.body.as_ref() {
            let limit_expr = limit_expr_of(query);
            let offset_expr = offset_expr_of(query);
            return self.exec_select(
                select,
                &query.order_by,
                limit_expr.as_ref(),
                offset_expr.as_ref(),
            );
        }
        let out = self.exec_set_expr(&query.body)?;
        let limit_expr = limit_expr_of(query);
        let offset_expr = offset_expr_of(query);
        Ok(QueryOutput {
            columns: out.columns,
            rows: apply_limit_offset(out.rows, limit_expr.as_ref(), offset_expr.as_ref()),
        })
    }

    fn exec_set_expr(&self, body: &SetExpr) -> Result<QueryOutput, MqdbError> {
        match body {
            SetExpr::Select(select) => self.exec_select(select, &None, None, None),
            SetExpr::Values(Values { rows, .. }) => {
                let empty = Row {
                    columns: vec![],
                    values: vec![],
                };
                let out: Vec<Vec<String>> = rows
                    .iter()
                    .map(|row| row.iter().map(|e| eval_expr(e, &empty).display()).collect())
                    .collect();
                Ok(QueryOutput {
                    columns: vec![],
                    rows: out,
                })
            }
            SetExpr::Query(q) => self.exec_query(q),
            SetExpr::SetOperation {
                left,
                op,
                set_quantifier,
                right,
            } => {
                let left_out = self.exec_set_expr(left)?;
                let right_out = self.exec_set_expr(right)?;
                combine_set_operation(left_out, right_out, op, set_quantifier)
            }
            _ => Err(MqdbError::SqlExec("unsupported query type".into())),
        }
    }

    fn exec_select(
        &self,
        select: &Select,
        order_by: &Option<sqlparser::ast::OrderBy>,
        limit: Option<&Expr>,
        offset: Option<&Expr>,
    ) -> Result<QueryOutput, MqdbError> {
        // 1. Materialise FROM — with cost-based index predicate pushdown
        let where_expr = select.selection.as_ref();
        let hint = where_expr
            .map(|we| self.choose_best_hint(candidate_hints_for_where(we)))
            .unwrap_or(IndexHint::FullScan);
        // Unlike `hint`, a skip has no later row-by-row recheck, so only
        // allow it for a single un-joined FROM table (no alias ambiguity).
        let single_unjoined_from = select.from.len() == 1 && select.from[0].joins.is_empty();
        let zone_filter = where_expr.filter(|_| single_unjoined_from);
        let mut rows = self.materialise_from_with_hint(&select.from, &hint, zone_filter)?;

        // 2. WHERE (full predicate evaluation; index only pre-filtered)
        //
        // Exception: `WHERE match(content, 'q')` with nothing else ANDed in,
        // against a plain (un-shadowed, un-joined) `blocks` table, is already
        // an *exact* result — `TermIndex::intersect` and `match()` share the
        // same `tokenize()` (see its doc comment), so there's no false
        // positive to filter out. Re-tokenizing every matched block's
        // content here would be pure waste, and for a common query term that
        // can mean re-scanning most of the table.
        let where_fully_indexed = matches!(hint, IndexHint::TermMatch(_))
            && single_unjoined_from
            && matches!(where_expr.map(unwrap_nested), Some(Expr::Function(_)))
            && from_names_unshadowed_blocks(&select.from[0], &self.cte_scopes.borrow());
        if !where_fully_indexed && let Some(where_expr) = &select.selection {
            let resolved = self.resolve_subqueries(where_expr)?;
            rows.retain(|row| eval_expr(&resolved, row).is_truthy());
        }

        // 3. PROJECT / GROUP / ORDER / LIMIT
        self.project_and_aggregate(select, rows, order_by, limit, offset)
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
        let (schema, table_name, alias, func_args) = match factor {
            TableFactor::Table {
                name, alias, args, ..
            } => {
                let parts: Vec<&str> = name.0.iter().map(ident_value).collect();
                let (schema, n) = if parts.len() >= 2 {
                    (
                        Some(parts[parts.len() - 2].to_lowercase()),
                        parts[parts.len() - 1].to_lowercase(),
                    )
                } else {
                    (None, parts.last().unwrap_or(&"").to_lowercase())
                };
                let a = alias.as_ref().map(|a| a.name.value.clone());
                (schema, n, a, args.clone())
            }
            _ => return Err(MqdbError::SqlExec("unsupported FROM clause".into())),
        };

        if let Some(func_args) = &func_args {
            let prefix = alias.as_deref().unwrap_or(&table_name).to_string();
            return resolve_table_function(&table_name, func_args, &prefix);
        }

        // `<alias>.<table>` — resolve against an ATTACHed store instead of
        // this one. No CTE shadowing or transitive attach across it.
        if let Some(schema) = schema {
            let guard = self.store.attached.read().unwrap();
            let other = guard.get(schema.as_str()).ok_or_else(|| {
                MqdbError::SqlExec(format!(
                    "unknown database '{schema}' (attach it first with ATTACH DATABASE '<path>' AS {schema})"
                ))
            })?;
            let engine = SqlEngine::new(other)?;
            return engine.table_rows_unqualified(&table_name, alias.as_deref(), hint, zone_filter);
        }

        // A `WITH x AS (...)` shadows a real table named `x`; search
        // innermost-to-outermost so nested `WITH`s shadow outer ones.
        for scope in self.cte_scopes.borrow().iter().rev() {
            if let Some(out) = scope.get(&table_name) {
                let prefix = alias.as_deref().unwrap_or(&table_name);
                return Ok(output_to_rows(out, prefix));
            }
        }

        self.table_rows_unqualified(&table_name, alias.as_deref(), hint, zone_filter)
    }

    /// Resolves `blocks`/`documents`/a view/a custom table by name, with no
    /// schema qualifier or CTE shadowing — shared by local and
    /// `<alias>.<table>` (attached-store) resolution.
    fn table_rows_unqualified(
        &self,
        table_name: &str,
        alias: Option<&str>,
        hint: &IndexHint,
        zone_filter: Option<&Expr>,
    ) -> Result<Vec<Row>, MqdbError> {
        match table_name {
            "blocks" => {
                let prefix = alias.unwrap_or("blocks");
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
                let prefix = alias.unwrap_or("documents");
                Ok(self
                    .store
                    .documents()
                    .iter()
                    .map(|doc| qualify_row(doc_to_row(doc), prefix))
                    .collect())
            }
            other => {
                if let Some(sql_text) = self.store.views.read().unwrap().get(other).cloned() {
                    let prefix = alias.unwrap_or(other).to_string();
                    return self.resolve_view(other, &sql_text, &prefix);
                }
                let guard = self.store.custom_tables.read().unwrap();
                if let Some(state) = guard.get(other) {
                    let prefix = alias.unwrap_or(other);
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

    fn exec_view_query(&self, name: &str, sql_text: &str) -> Result<QueryOutput, MqdbError> {
        if self.view_stack.borrow().iter().any(|n| n == name) {
            let mut cycle = self.view_stack.borrow().clone();
            cycle.push(name.to_string());
            return Err(MqdbError::SqlExec(format!(
                "circular view reference: {}",
                cycle.join(" -> ")
            )));
        }
        self.view_stack.borrow_mut().push(name.to_string());
        let result = (|| {
            let stmts = Parser::parse_sql(&GenericDialect {}, sql_text)
                .map_err(|e| MqdbError::SqlParse(e.to_string()))?;
            let stmt = stmts
                .into_iter()
                .next()
                .ok_or_else(|| MqdbError::SqlParse("empty view query".into()))?;
            let Statement::Query(query) = stmt else {
                return Err(MqdbError::SqlExec("view query is not a SELECT".into()));
            };
            self.exec_query(&query)
        })();
        self.view_stack.borrow_mut().pop();
        result
    }

    fn resolve_view(
        &self,
        name: &str,
        sql_text: &str,
        prefix: &str,
    ) -> Result<Vec<Row>, MqdbError> {
        Ok(output_to_rows(
            &self.exec_view_query(name, sql_text)?,
            prefix,
        ))
    }

    fn project_and_aggregate(
        &self,
        select: &Select,
        rows: Vec<Row>,
        order_by: &Option<sqlparser::ast::OrderBy>,
        limit: Option<&Expr>,
        offset: Option<&Expr>,
    ) -> Result<QueryOutput, MqdbError> {
        let group_by_exprs: Vec<Expr> = match &select.group_by {
            GroupByExpr::Expressions(exprs, _) => exprs.clone(),
            _ => vec![],
        };
        let is_agg = has_aggregate(&select.projection);

        if is_agg || !group_by_exprs.is_empty() {
            return self.aggregate(select, rows, limit, offset, &group_by_exprs);
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
            rows: apply_limit_offset(result, limit, offset),
        })
    }

    fn aggregate(
        &self,
        select: &Select,
        rows: Vec<Row>,
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

        let out_rows: Vec<Vec<String>> = groups
            .iter()
            .filter(|(key_vals, group_rows)| {
                select.having.as_ref().is_none_or(|h| {
                    eval_having(h, &select.projection, group_by_exprs, key_vals, group_rows)
                })
            })
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

fn resolve_index_columns(table_columns: &[String], idx_cols: &[IndexColumn]) -> Vec<usize> {
    idx_cols
        .iter()
        .filter_map(|ic| match &ic.column.expr {
            Expr::Identifier(id) => table_columns
                .iter()
                .position(|c| c.eq_ignore_ascii_case(&id.value)),
            _ => None,
        })
        .collect()
}

fn table_constraints(
    table_columns: &[String],
    column_defs: &[ColumnDef],
    constraints: &[TableConstraint],
) -> (Vec<usize>, Vec<Vec<usize>>) {
    let mut not_null = Vec::new();
    let mut unique = Vec::new();

    for (i, col) in column_defs.iter().enumerate() {
        for opt in &col.options {
            match &opt.option {
                ColumnOption::NotNull => not_null.push(i),
                ColumnOption::Unique(_) => unique.push(vec![i]),
                ColumnOption::PrimaryKey(_) => {
                    not_null.push(i);
                    unique.push(vec![i]);
                }
                _ => {}
            }
        }
    }

    for c in constraints {
        match c {
            TableConstraint::Unique(u) => {
                let cols = resolve_index_columns(table_columns, &u.columns);
                if !cols.is_empty() {
                    unique.push(cols);
                }
            }
            TableConstraint::PrimaryKey(pk) => {
                let cols = resolve_index_columns(table_columns, &pk.columns);
                not_null.extend(cols.iter().copied());
                if !cols.is_empty() {
                    unique.push(cols);
                }
            }
            _ => {}
        }
    }

    not_null.sort_unstable();
    not_null.dedup();
    (not_null, unique)
}

fn single_table_name(twj: &TableWithJoins) -> Result<String, MqdbError> {
    if !twj.joins.is_empty() {
        return Err(MqdbError::SqlExec(
            "UPDATE/DELETE with write-back do not support joins".into(),
        ));
    }
    match &twj.relation {
        TableFactor::Table { name, .. } => require_unqualified(name),
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
                    TableObject::TableName(name) => require_unqualified(name)?,
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
            Statement::Rollback { .. } => {
                let unrevertable = self.rollback_transaction()?;
                if unrevertable.is_empty() {
                    Ok(ok_result())
                } else {
                    Ok(QueryOutput {
                        columns: vec!["result".to_string()],
                        rows: vec![vec![format!(
                            "rolled back (note: {} source file(s) were already modified by write-back and cannot be reverted: {})",
                            unrevertable.len(),
                            unrevertable.join(", ")
                        )]],
                    })
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

fn value_to_expr(v: &Value) -> Expr {
    match v {
        Value::Int(n) => Expr::Value(SqlValue::Number(n.to_string(), false).with_empty_span()),
        Value::Float(n) => Expr::Value(SqlValue::Number(n.to_string(), false).with_empty_span()),
        Value::Bool(b) => Expr::Value(SqlValue::Boolean(*b).with_empty_span()),
        Value::Str(s) => Expr::Value(SqlValue::SingleQuotedString(s.clone()).with_empty_span()),
        Value::Null => Expr::Value(SqlValue::Null.with_empty_span()),
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
    let substituted = substitute_having_expr(having, group_by_exprs, key_vals, group_rows);
    let dummy = Row {
        columns: vec![],
        values: vec![],
    };
    eval_expr(&substituted, &dummy).is_truthy()
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

fn expr_structurally_eq(a: &Expr, b: &Expr) -> bool {
    a == b
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

fn multiset_counts(rows: &[Vec<String>]) -> FxHashMap<Vec<String>, usize> {
    let mut counts = FxHashMap::default();
    for r in rows {
        *counts.entry(r.clone()).or_insert(0) += 1;
    }
    counts
}

fn dedup_rows(mut rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
    let mut seen = std::collections::HashSet::new();
    rows.retain(|r| seen.insert(r.clone()));
    rows
}

fn combine_set_operation(
    left: QueryOutput,
    right: QueryOutput,
    op: &SetOperator,
    quantifier: &SetQuantifier,
) -> Result<QueryOutput, MqdbError> {
    let n_left = left.rows.first().map(|r| r.len());
    let n_right = right.rows.first().map(|r| r.len());
    if let (Some(a), Some(b)) = (n_left, n_right)
        && a != b
    {
        return Err(MqdbError::SqlExec(format!(
            "set operation: left side has {a} column(s), right side has {b}"
        )));
    }
    let columns = if !left.columns.is_empty() {
        left.columns
    } else {
        right.columns
    };
    let all = matches!(quantifier, SetQuantifier::All);
    let rows = match (op, all) {
        (SetOperator::Union, true) => {
            let mut combined = left.rows;
            combined.extend(right.rows);
            combined
        }
        (SetOperator::Union, false) => {
            let mut combined = left.rows;
            combined.extend(right.rows);
            dedup_rows(combined)
        }
        (SetOperator::Intersect, true) => {
            let mut right_counts = multiset_counts(&right.rows);
            left.rows
                .into_iter()
                .filter(|r| match right_counts.get_mut(r) {
                    Some(c) if *c > 0 => {
                        *c -= 1;
                        true
                    }
                    _ => false,
                })
                .collect()
        }
        (SetOperator::Intersect, false) => {
            let right_set: std::collections::HashSet<_> = right.rows.into_iter().collect();
            dedup_rows(
                left.rows
                    .into_iter()
                    .filter(|r| right_set.contains(r))
                    .collect(),
            )
        }
        (SetOperator::Except | SetOperator::Minus, true) => {
            let mut right_counts = multiset_counts(&right.rows);
            left.rows
                .into_iter()
                .filter(|r| match right_counts.get_mut(r) {
                    Some(c) if *c > 0 => {
                        *c -= 1;
                        false
                    }
                    _ => true,
                })
                .collect()
        }
        (SetOperator::Except | SetOperator::Minus, false) => {
            let right_set: std::collections::HashSet<_> = right.rows.into_iter().collect();
            dedup_rows(
                left.rows
                    .into_iter()
                    .filter(|r| !right_set.contains(r))
                    .collect(),
            )
        }
    };
    Ok(QueryOutput { columns, rows })
}

fn apply_limit_offset(
    mut rows: Vec<Vec<String>>,
    limit: Option<&Expr>,
    offset: Option<&Expr>,
) -> Vec<Vec<String>> {
    let dummy = Row {
        columns: vec![],
        values: vec![],
    };
    if let Some(off) = offset
        && let Value::Int(n) = eval_expr(off, &dummy)
    {
        rows.drain(..(n as usize).min(rows.len()));
    }
    if let Some(lim) = limit
        && let Value::Int(n) = eval_expr(lim, &dummy)
    {
        rows.truncate(n as usize);
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

/// Strips redundant `(...)` wrappers so callers can pattern-match the inner
/// expression directly.
fn unwrap_nested(expr: &Expr) -> &Expr {
    match expr {
        Expr::Nested(inner) => unwrap_nested(inner),
        other => other,
    }
}

fn limit_expr_of(query: &Query) -> Option<Expr> {
    query.limit_clause.as_ref().and_then(|lc| match lc {
        LimitClause::LimitOffset { limit, .. } => limit.clone(),
        LimitClause::OffsetCommaLimit { limit, .. } => Some(limit.clone()),
    })
}

fn offset_expr_of(query: &Query) -> Option<Expr> {
    query.limit_clause.as_ref().and_then(|lc| match lc {
        LimitClause::LimitOffset { offset, .. } => offset.as_ref().map(|o| o.value.clone()),
        LimitClause::OffsetCommaLimit { offset, .. } => Some(offset.clone()),
    })
}

fn table_factor_ident(factor: &TableFactor) -> Option<String> {
    match factor {
        TableFactor::Table { name, .. } => {
            Some(name.0.last().map(ident_value).unwrap_or("").to_lowercase())
        }
        _ => None,
    }
}

fn resolve_table_function(
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

const MAX_RECURSIVE_CTE_ITERATIONS: usize = 10_000;

fn select_references_table(select: &Select, name: &str) -> bool {
    select.from.iter().any(|twj| {
        table_factor_ident(&twj.relation).as_deref() == Some(name)
            || twj
                .joins
                .iter()
                .any(|j| table_factor_ident(&j.relation).as_deref() == Some(name))
    })
}

fn describe_hint(hint: &IndexHint) -> String {
    match hint {
        IndexHint::BlockType(types) => format!(
            "BitmapIndex(block_type IN ({}))",
            types
                .iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        IndexHint::PreExact(n) => format!("BTreeIndex(pre = {n})"),
        IndexHint::PreRange(lo, hi) => format!("BTreeIndex(pre BETWEEN {lo} AND {hi})"),
        IndexHint::ContentExact(s) => format!("HashIndex(content = '{s}')"),
        IndexHint::LangExact(s) => format!("HashIndex(lang = '{s}')"),
        IndexHint::DepthExact(d) => format!("HashIndex(depth = {d})"),
        IndexHint::TermMatch(terms) => format!("TermIndex(match: {})", terms.join(", ")),
        IndexHint::FullScan => "full scan".to_string(),
    }
}

fn zone_map_candidate_fields(where_expr: &Expr) -> Vec<&'static str> {
    let mut eq_block_type: Option<BlockType> = None;
    let mut has_content = false;
    let mut fields = Vec::new();

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
            Some("lang") => {
                if let Some(s) = val
                    && !s.is_empty()
                {
                    fields.push("lang");
                }
            }
            Some("depth") => {
                if let Some(n) = int_val
                    && n > 0
                {
                    fields.push("depth");
                }
            }
            Some("content") => has_content = true,
            _ => {}
        }
    }

    if has_content && eq_block_type == Some(BlockType::Heading) {
        fields.push("heading content");
    }
    fields
}

fn describe_join_strategy(on: &Expr) -> String {
    for conjunct in flatten_and_conjuncts(on) {
        if let Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } = conjunct
            && expr_col_name(left).is_some()
            && expr_col_name(right).is_some()
        {
            return format!("hash join on {left} = {right}");
        }
    }
    "nested loop (cross join + filter)".to_string()
}

/// True if `twj`'s relation is the real `blocks` table — not a `WITH`-clause
/// CTE of the same name shadowing it — which is what `table_rows_with_hint`
/// actually applies [`IndexHint`]s to. Used to gate skipping the WHERE
/// row-by-row recheck: that's only sound when the index was truly consulted.
fn from_names_unshadowed_blocks(
    twj: &TableWithJoins,
    cte_scopes: &[FxHashMap<String, std::rc::Rc<QueryOutput>>],
) -> bool {
    let TableFactor::Table { name, .. } = &twj.relation else {
        return false;
    };
    if name.0.last().map(ident_value).unwrap_or("").to_lowercase() != "blocks" {
        return false;
    }
    !cte_scopes.iter().any(|scope| scope.contains_key("blocks"))
}

/// Recognises a single conjunct's index-hint shape — no `AND` handling (see
/// [`candidate_hints_for_where`] for combining multiple conjuncts). The full
/// WHERE predicate is still evaluated row-by-row after pre-filtering, so a
/// false positive from an index lookup is harmless (but there shouldn't be
/// any).
///
/// Patterns recognised:
/// - `block_type = 'X'` → [`IndexHint::BlockType`]
/// - `block_type IN ('X','Y',...)` → [`IndexHint::BlockType`] (union)
/// - `pre = N` → [`IndexHint::PreExact`]
/// - `pre BETWEEN lo AND hi` → [`IndexHint::PreRange`]
/// - `content = 'X'` → [`IndexHint::ContentExact`]
/// - `lang = 'X'` → [`IndexHint::LangExact`]
/// - `depth = N` → [`IndexHint::DepthExact`]
/// - `match(content, 'terms')` → [`IndexHint::TermMatch`]
fn hint_for_conjunct(expr: &Expr) -> IndexHint {
    match expr {
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
        Expr::Function(_) => match match_terms_from_expr(expr) {
            Some(terms) => IndexHint::TermMatch(terms),
            None => IndexHint::FullScan,
        },
        Expr::Nested(inner) => hint_for_conjunct(inner),
        _ => IndexHint::FullScan,
    }
}

/// Every viable (non-[`IndexHint::FullScan`]) index-hint candidate for
/// `expr`'s conjuncts. The actual choice among them is cost-based — see
/// [`SqlEngine::choose_best_hint`].
fn candidate_hints_for_where(expr: &Expr) -> Vec<IndexHint> {
    flatten_and_conjuncts(expr)
        .into_iter()
        .map(hint_for_conjunct)
        .filter(|h| !matches!(h, IndexHint::FullScan))
        .collect()
}

/// Recognises `match(content, 'query terms')` and returns its tokenized
/// query terms, or `None` if `expr` isn't that exact shape (wrong function
/// name, wrong column, or a non-literal query argument). Shared by
/// [`hint_for_conjunct`] (to build the [`IndexHint::TermMatch`] hint)
/// and [`zone_map_skip`] (to rule out whole documents via the term bloom
/// filter) so the two stay in lockstep.
fn match_terms_from_expr(expr: &Expr) -> Option<Vec<String>> {
    let Expr::Function(f) = expr else {
        return None;
    };
    let name = f
        .name
        .0
        .last()
        .map(ident_value)
        .unwrap_or("")
        .to_lowercase();
    if name != "match" {
        return None;
    }
    let FunctionArguments::List(al) = &f.args else {
        return None;
    };
    let [
        FunctionArg::Unnamed(FunctionArgExpr::Expr(col)),
        FunctionArg::Unnamed(FunctionArgExpr::Expr(q)),
    ] = al.args.as_slice()
    else {
        return None;
    };
    if expr_col_name(col).as_deref() != Some("content") {
        return None;
    }
    let query_str = expr_str_val(q)?;
    let terms = tokenize(&query_str);
    if terms.is_empty() { None } else { Some(terms) }
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
    fn test_sql_documents_zone_map_columns() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "SELECT block_count, max_heading_depth, code_languages, frontmatter_keys \
                 FROM documents",
            )
            .unwrap();
        assert_eq!(
            out.rows,
            vec![vec![
                "6".to_string(),
                "2".to_string(),
                "[\"rust\"]".to_string(),
                "[]".to_string(),
            ]]
        );
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
    fn test_sql_count_with_grouped_column() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT block_type, count(*) FROM blocks GROUP BY block_type")
            .unwrap();
        assert!(!out.rows.is_empty());
    }

    #[test]
    fn test_sql_count_with_ungrouped_column_errors() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let err = engine
            .execute("SELECT count(*), content FROM blocks")
            .unwrap_err();
        assert!(err.to_string().contains("GROUP BY"));
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
    fn test_sql_offset() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let all = engine
            .execute("SELECT content FROM blocks ORDER BY pre")
            .unwrap();
        let offset = engine
            .execute("SELECT content FROM blocks ORDER BY pre LIMIT 100 OFFSET 2")
            .unwrap();
        assert_eq!(offset.rows, all.rows[2..]);
    }

    #[test]
    fn test_sql_offset_past_end_returns_empty() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT content FROM blocks LIMIT 100 OFFSET 1000")
            .unwrap();
        assert!(out.rows.is_empty());
    }

    #[test]
    fn test_sql_having() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "SELECT block_type, count(*) FROM blocks GROUP BY block_type \
                 HAVING count(*) > 1 ORDER BY block_type",
            )
            .unwrap();
        assert_eq!(
            out.rows,
            vec![
                vec!["heading".to_string(), "3".to_string()],
                vec!["paragraph".to_string(), "2".to_string()],
            ]
        );
    }

    #[test]
    fn test_sql_union() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "SELECT content FROM blocks WHERE block_type = 'heading' \
                 UNION SELECT content FROM blocks WHERE block_type = 'heading'",
            )
            .unwrap();
        assert_eq!(out.rows.len(), 3);

        let out_all = engine
            .execute(
                "SELECT content FROM blocks WHERE block_type = 'heading' \
                 UNION ALL SELECT content FROM blocks WHERE block_type = 'heading'",
            )
            .unwrap();
        assert_eq!(out_all.rows.len(), 6);
    }

    #[test]
    fn test_sql_intersect_and_except() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let intersect = engine
            .execute(
                "SELECT content FROM blocks WHERE block_type = 'heading' \
                 INTERSECT SELECT content FROM blocks WHERE content = 'Architecture'",
            )
            .unwrap();
        assert_eq!(intersect.rows, vec![vec!["Architecture".to_string()]]);

        let except = engine
            .execute(
                "SELECT content FROM blocks WHERE block_type = 'heading' \
                 EXCEPT SELECT content FROM blocks WHERE content = 'Architecture'",
            )
            .unwrap();
        assert_eq!(except.rows.len(), 2);
        assert!(!except.rows.iter().any(|r| r[0] == "Architecture"));
    }

    #[test]
    fn test_sql_union_column_count_mismatch_errors() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let err = engine
            .execute("SELECT content FROM blocks UNION SELECT content, block_type FROM blocks")
            .unwrap_err();
        assert!(err.to_string().contains("column"));
    }

    #[test]
    fn test_sql_date_functions() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "SELECT date_trunc('month', '2024-03-15'), \
                 date_diff('day', '2024-01-01', '2024-03-15'), \
                 date_diff('month', '2024-01-01', '2024-03-15'), \
                 date_add('2024-01-31', 1, 'month'), \
                 date_sub('2024-03-15', 10, 'day'), \
                 strftime('%Y/%m/%d', '2024-03-15')",
            )
            .unwrap();
        assert_eq!(
            out.rows,
            vec![vec![
                "2024-03-01".to_string(),
                "74".to_string(),
                "2".to_string(),
                "2024-02-29".to_string(),
                "2024-03-05".to_string(),
                "2024/03/15".to_string(),
            ]]
        );
    }

    #[test]
    fn test_sql_extract() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT EXTRACT(YEAR FROM '2024-03-15T10:30:00'), EXTRACT(HOUR FROM '2024-03-15T10:30:00')")
            .unwrap();
        assert_eq!(out.rows, vec![vec!["2024".to_string(), "10".to_string()]]);
    }

    #[test]
    fn test_sql_regexp_operator() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT content FROM blocks WHERE content REGEXP '^Arch.*ure$'")
            .unwrap();
        assert_eq!(out.rows, vec![vec!["Architecture".to_string()]]);

        let negated = engine
            .execute("SELECT content FROM blocks WHERE content NOT REGEXP '^Arch'")
            .unwrap();
        assert!(!negated.rows.iter().any(|r| r[0] == "Architecture"));
    }

    #[test]
    fn test_sql_regexp_functions() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "SELECT regexp_like('hello world', 'wor.d'), \
                 regexp_replace('hello world', 'o', '0'), \
                 regexp_extract('id-1234', '(\\d+)', 1)",
            )
            .unwrap();
        assert_eq!(
            out.rows,
            vec![vec![
                "true".to_string(),
                "hell0 w0rld".to_string(),
                "1234".to_string(),
            ]]
        );
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

    #[test]
    fn test_ddl_primary_key_rejects_null_and_duplicate() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        engine
            .execute("CREATE TABLE notes (id TEXT PRIMARY KEY, body TEXT)")
            .unwrap();
        engine
            .execute("INSERT INTO notes VALUES ('1', 'hello')")
            .unwrap();

        let dup = engine
            .execute("INSERT INTO notes VALUES ('1', 'again')")
            .unwrap_err();
        assert!(dup.to_string().contains("UNIQUE"));

        let null = engine
            .execute("INSERT INTO notes (body) VALUES ('no id')")
            .unwrap_err();
        assert!(null.to_string().contains("NOT NULL"));

        // Rejected batch must not leave partial rows committed.
        let all = engine.execute("SELECT * FROM notes").unwrap();
        assert_eq!(all.rows.len(), 1);
    }

    #[test]
    fn test_ddl_table_level_unique_constraint() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        engine
            .execute("CREATE TABLE pairs (a TEXT, b TEXT, UNIQUE (a, b))")
            .unwrap();
        engine
            .execute("INSERT INTO pairs VALUES ('x', 'y')")
            .unwrap();
        // Same 'a' alone is fine — uniqueness is on the (a, b) pair.
        engine
            .execute("INSERT INTO pairs VALUES ('x', 'z')")
            .unwrap();
        let dup = engine
            .execute("INSERT INTO pairs VALUES ('x', 'y')")
            .unwrap_err();
        assert!(dup.to_string().contains("UNIQUE"));
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
    fn bm25_ranks_rarer_term_higher_than_common_term() {
        let mut store = DocumentStore::new();
        store
            .add_str("# Doc\n\nwidget\n\ncommon\n\ncommon\n\ncommon\n\ncommon\n")
            .unwrap();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "SELECT content FROM blocks WHERE block_type = 'paragraph'
                 ORDER BY bm25(content, 'widget common') DESC LIMIT 1",
            )
            .unwrap();
        assert_eq!(out.rows[0][0], "widget");
    }

    #[test]
    fn bm25_without_bm25_in_sql_text_is_unaffected() {
        let mut store = DocumentStore::new();
        store.add_str("# Doc\n\nfoo bar\n").unwrap();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT content FROM blocks WHERE block_type = 'paragraph'")
            .unwrap();
        assert_eq!(out.rows[0][0], "foo bar");
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
        let candidates = candidate_hints_for_where(select.selection.as_ref().unwrap());
        assert_eq!(
            candidates,
            vec![IndexHint::TermMatch(vec![
                "foo".to_string(),
                "bar".to_string()
            ])]
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
    fn where_bare_match_skips_recheck_but_result_is_still_correct() {
        // "architecture" only tokenizes out of the heading block, so if the
        // recheck-skip path (bare `match()` fully covering WHERE) somehow
        // returned a false positive, this would catch it.
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("SELECT content FROM blocks WHERE match(content, 'architecture')")
            .unwrap();
        assert_eq!(out.rows, vec![vec!["Architecture".to_string()]]);
    }

    #[test]
    fn where_bare_match_still_rechecked_when_blocks_shadowed_by_cte() {
        // A CTE named `blocks` shadows the real table, so `table_rows_with_hint`
        // never consults the TermIndex for it — the recheck-skip path must
        // not fire here, or a CTE that fabricates non-matching content would
        // slip through uncaught.
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "WITH blocks AS (SELECT 'no match here' AS content)
                 SELECT content FROM blocks WHERE match(content, 'architecture')",
            )
            .unwrap();
        assert!(out.rows.is_empty());
    }

    #[test]
    fn cost_based_planner_picks_cheaper_of_two_candidates() {
        let mut store = DocumentStore::new();
        store
            .add_str("# Doc\n\nP1\n\nP2\n\nP3\n\nP4\n\n```rust\nfn main(){}\n```\n")
            .unwrap();
        let engine = SqlEngine::new(&store).unwrap();

        let stmts = Parser::parse_sql(
            &GenericDialect {},
            "SELECT * FROM blocks WHERE block_type = 'paragraph' AND lang = 'rust'",
        )
        .unwrap();
        let Statement::Query(q) = stmts.into_iter().next().unwrap() else {
            panic!("expected query")
        };
        let SetExpr::Select(select) = q.body.as_ref() else {
            panic!("expected select")
        };
        let candidates = candidate_hints_for_where(select.selection.as_ref().unwrap());
        assert_eq!(candidates.len(), 2);

        // 4 paragraphs vs. 1 rust code block — the lang lookup is cheaper.
        let chosen = engine.choose_best_hint(candidates);
        assert_eq!(chosen, IndexHint::LangExact("rust".to_string()));
    }

    #[test]
    fn cost_based_planner_tie_breaks_deterministically() {
        let mut store = DocumentStore::new();
        store.add_str("# Title\n\nBody\n").unwrap();
        let engine = SqlEngine::new(&store).unwrap();

        let stmts = Parser::parse_sql(
            &GenericDialect {},
            "SELECT * FROM blocks WHERE block_type = 'heading' AND depth = 1",
        )
        .unwrap();
        let Statement::Query(q) = stmts.into_iter().next().unwrap() else {
            panic!("expected query")
        };
        let SetExpr::Select(select) = q.body.as_ref() else {
            panic!("expected select")
        };
        let candidates = candidate_hints_for_where(select.selection.as_ref().unwrap());
        assert_eq!(candidates.len(), 2);

        // Both candidates match exactly one block (the single H1) — equal
        // cost, so the first-encountered candidate (BlockType) must win,
        // consistently across repeated calls.
        let first = engine.choose_best_hint(candidates.clone());
        let second = engine.choose_best_hint(candidates);
        assert_eq!(first, second);
        assert_eq!(first, IndexHint::BlockType(vec![BlockType::Heading]));
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
        let candidates = candidate_hints_for_where(select.selection.as_ref().unwrap());
        assert_eq!(candidates, Vec::<IndexHint>::new());
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
    fn with_recursive_generates_number_sequence() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "WITH RECURSIVE seq AS (
                   SELECT 1 AS n
                   UNION ALL
                   SELECT n + 1 FROM seq WHERE n < 5
                 )
                 SELECT n FROM seq ORDER BY n",
            )
            .unwrap();
        assert_eq!(
            out.rows,
            vec![
                vec!["1".to_string()],
                vec!["2".to_string()],
                vec!["3".to_string()],
                vec!["4".to_string()],
                vec!["5".to_string()],
            ]
        );
    }

    #[test]
    fn with_recursive_union_dedupes_across_iterations() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        // Without dedup this would cycle between 1 and 2 forever; plain
        // UNION must drop the repeat and terminate.
        let out = engine
            .execute(
                "WITH RECURSIVE cyc AS (
                   SELECT 1 AS n
                   UNION
                   SELECT mod(n, 2) + 1 FROM cyc
                 )
                 SELECT n FROM cyc ORDER BY n",
            )
            .unwrap();
        assert_eq!(out.rows, vec![vec!["1".to_string()], vec!["2".to_string()]]);
    }

    #[test]
    fn with_recursive_walks_heading_ancestors_via_interval_containment() {
        let mut store = DocumentStore::new();
        store.add_str("# A\n\n## B\n\n### C\n\nLeaf\n").unwrap();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "WITH RECURSIVE ancestors AS (
                   SELECT pre, post, content FROM blocks
                   WHERE block_type = 'heading' AND content = 'C'
                   UNION
                   SELECT b.pre, b.post, b.content
                   FROM blocks b, ancestors
                   WHERE b.pre < ancestors.pre AND ancestors.post < b.post
                     AND b.block_type = 'heading'
                 )
                 SELECT content FROM ancestors ORDER BY pre",
            )
            .unwrap();
        let contents: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(contents, vec!["A", "B", "C"]);
    }

    #[test]
    fn with_recursive_rejects_anchor_self_reference() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let err = engine
            .execute(
                "WITH RECURSIVE r AS (
                   SELECT n FROM r
                   UNION ALL
                   SELECT n FROM r
                 )
                 SELECT n FROM r",
            )
            .unwrap_err();
        assert!(err.to_string().contains("anchor"));
    }

    #[test]
    fn with_recursive_rejects_mismatched_column_counts() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let err = engine
            .execute(
                "WITH RECURSIVE r AS (
                   SELECT 1 AS n
                   UNION ALL
                   SELECT n, n FROM r WHERE n < 5
                 )
                 SELECT n FROM r",
            )
            .unwrap_err();
        assert!(err.to_string().contains("different numbers of columns"));
    }

    #[test]
    fn with_recursive_hits_iteration_cap_with_clear_error() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let err = engine
            .execute(
                "WITH RECURSIVE r AS (
                   SELECT 1 AS n
                   UNION ALL
                   SELECT n + 1 FROM r
                 )
                 SELECT n FROM r",
            )
            .unwrap_err();
        assert!(err.to_string().contains("iterations"));
    }

    #[test]
    fn with_recursive_non_self_referencing_cte_still_works() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("WITH RECURSIVE r AS (SELECT content FROM blocks) SELECT content FROM r")
            .unwrap();
        assert!(!out.rows.is_empty());
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

    // UPDATE/DELETE write-back

    fn write_md(dir: &tempfile::TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_transaction_rolls_back_custom_table() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        engine.execute("CREATE TABLE notes (id TEXT)").unwrap();
        engine.execute("BEGIN").unwrap();
        engine.execute("INSERT INTO notes VALUES ('1')").unwrap();
        assert_eq!(engine.execute("SELECT * FROM notes").unwrap().rows.len(), 1);
        engine.execute("ROLLBACK").unwrap();
        assert_eq!(engine.execute("SELECT * FROM notes").unwrap().rows.len(), 0);
    }

    #[test]
    fn test_transaction_commit_keeps_changes() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        engine.execute("CREATE TABLE notes (id TEXT)").unwrap();
        engine.execute("BEGIN").unwrap();
        engine.execute("INSERT INTO notes VALUES ('1')").unwrap();
        engine.execute("COMMIT").unwrap();
        assert_eq!(engine.execute("SELECT * FROM notes").unwrap().rows.len(), 1);
        let err = engine.execute("COMMIT").unwrap_err();
        assert!(err.to_string().contains("no transaction"));
    }

    #[test]
    fn test_transaction_nested_begin_errors() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        engine.execute("BEGIN").unwrap();
        let err = engine.execute("BEGIN").unwrap_err();
        assert!(err.to_string().contains("already in progress"));
        engine.execute("ROLLBACK").unwrap();
    }

    #[test]
    fn test_transaction_rollback_without_begin_errors() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        let err = engine.execute("ROLLBACK").unwrap_err();
        assert!(err.to_string().contains("no transaction"));
    }

    #[test]
    fn write_back_transaction_rollback_reverts_in_memory_blocks_but_flags_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Old Title\n\nBody text\n");

        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        store.execute_sql_mut("BEGIN").unwrap();
        store
            .execute_sql_mut("UPDATE blocks SET content = 'New Title' WHERE block_type = 'heading'")
            .unwrap();
        assert!(
            store.documents()[0]
                .blocks
                .iter()
                .any(|b| b.content == "New Title")
        );

        let out = store.execute_sql_mut("ROLLBACK").unwrap();
        assert!(out.rows[0][0].contains("cannot be reverted"));

        assert!(
            store.documents()[0]
                .blocks
                .iter()
                .any(|b| b.content == "Old Title")
        );
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "# New Title\n\nBody text\n");
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

    // EXPLAIN / EXPLAIN ANALYZE

    fn explain_detail<'a>(out: &'a QueryOutput, step: &str) -> &'a str {
        out.rows
            .iter()
            .find(|r| r[0] == step)
            .unwrap_or_else(|| panic!("no EXPLAIN row for step '{step}' in {:?}", out.rows))[1]
            .as_str()
    }

    #[test]
    fn explain_reports_bitmap_index_for_block_type_eq() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("EXPLAIN SELECT content FROM blocks WHERE block_type = 'heading'")
            .unwrap();
        assert_eq!(out.columns, vec!["step", "detail"]);
        assert!(explain_detail(&out, "query:where").contains("BitmapIndex"));
        assert!(explain_detail(&out, "query:zone-map").contains("not eligible"));
    }

    #[test]
    fn explain_reports_zone_map_skip_eligibility() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("EXPLAIN SELECT content FROM blocks WHERE lang = 'rust'")
            .unwrap();
        assert!(explain_detail(&out, "query:zone-map").contains("eligible via lang"));
    }

    #[test]
    fn explain_reports_full_scan_when_no_hint_applies() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("EXPLAIN SELECT content FROM blocks WHERE content LIKE '%foo%'")
            .unwrap();
        assert!(explain_detail(&out, "query:where").contains("full scan"));
    }

    #[test]
    fn explain_reports_multiple_candidates_with_costs() {
        let mut store = DocumentStore::new();
        store
            .add_str("# Doc\n\nP1\n\nP2\n\nP3\n\nP4\n\n```rust\nfn main(){}\n```\n")
            .unwrap();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "EXPLAIN SELECT * FROM blocks WHERE block_type = 'paragraph' AND lang = 'rust'",
            )
            .unwrap();
        let detail = explain_detail(&out, "query:where");
        assert!(detail.contains("est."));
        assert!(detail.contains("also considered"));
        assert!(detail.contains("HashIndex(lang = 'rust') used"));
    }

    #[test]
    fn explain_reports_no_where_row_when_no_where_clause() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("EXPLAIN SELECT content FROM blocks")
            .unwrap();
        assert!(explain_detail(&out, "query:where").contains("full scan"));
    }

    #[test]
    fn explain_reports_hash_join_for_equi_join() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "EXPLAIN SELECT h.content FROM blocks h
                 JOIN blocks n ON n.document_id = h.document_id",
            )
            .unwrap();
        assert!(explain_detail(&out, "query:join[0]").contains("hash join"));
    }

    #[test]
    fn explain_reports_nested_loop_for_non_equi_join() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "EXPLAIN SELECT h.content FROM blocks h
                 JOIN blocks n ON n.pre = h.pre + 1",
            )
            .unwrap();
        assert!(explain_detail(&out, "query:join[0]").contains("nested loop"));
    }

    #[test]
    fn explain_reports_group_by_order_by_and_limit() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "EXPLAIN SELECT block_type, count(*) FROM blocks
                 GROUP BY block_type ORDER BY block_type LIMIT 5",
            )
            .unwrap();
        assert!(explain_detail(&out, "query:group-by").contains("1 key"));
        assert!(explain_detail(&out, "query:order-by").contains("ASC"));
        assert_eq!(explain_detail(&out, "query:limit"), "5");
    }

    #[test]
    fn explain_describes_cte_separately_from_outer_query() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "EXPLAIN WITH headings AS (SELECT content FROM blocks WHERE block_type = 'heading')
                 SELECT content FROM headings",
            )
            .unwrap();
        assert!(explain_detail(&out, "cte:headings:where").contains("BitmapIndex"));
        assert!(explain_detail(&out, "query:from").contains("headings (cte)"));
    }

    #[test]
    fn explain_analyze_runs_query_and_reports_row_count() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute("EXPLAIN ANALYZE SELECT content FROM blocks WHERE block_type = 'heading'")
            .unwrap();
        assert_eq!(explain_detail(&out, "actual:rows"), "3 row(s) returned");
        assert!(explain_detail(&out, "actual:elapsed").contains("ms"));
        assert!(
            out.rows
                .iter()
                .any(|r| r[1].contains("document(s) skipped by zone map"))
        );
    }

    #[test]
    fn explain_analyze_skips_doc_stats_for_joins() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(
                "EXPLAIN ANALYZE SELECT h.content FROM blocks h
                 JOIN blocks n ON n.document_id = h.document_id",
            )
            .unwrap();
        assert!(
            !out.rows
                .iter()
                .any(|r| r[1].contains("document(s) skipped by zone map"))
        );
        assert!(explain_detail(&out, "actual:rows").contains("row(s) returned"));
    }

    #[test]
    fn explain_rejects_non_select_statement() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let err = engine
            .execute("EXPLAIN CREATE TABLE notes (name TEXT)")
            .unwrap_err();
        assert!(err.to_string().contains("SELECT"));
    }

    #[test]
    fn explain_under_write_back_delegates_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        let out = store
            .execute_sql_mut("EXPLAIN SELECT content FROM blocks WHERE block_type = 'heading'")
            .unwrap();
        assert!(explain_detail(&out, "query:where").contains("BitmapIndex"));
    }

    // CREATE VIEW / DROP VIEW

    #[test]
    fn create_view_then_select_reflects_current_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nOld Content\n");
        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        store
            .execute_sql_mut(
                "CREATE VIEW paras AS SELECT content FROM blocks WHERE block_type = 'paragraph'",
            )
            .unwrap();

        let out = SqlEngine::new(&store)
            .unwrap()
            .execute("SELECT content FROM paras")
            .unwrap();
        assert_eq!(out.rows, vec![vec!["Old Content".to_string()]]);

        store
            .execute_sql_mut(
                "UPDATE blocks SET content = 'New Content' WHERE content = 'Old Content'",
            )
            .unwrap();

        let out = SqlEngine::new(&store)
            .unwrap()
            .execute("SELECT content FROM paras")
            .unwrap();
        assert_eq!(out.rows, vec![vec!["New Content".to_string()]]);
    }

    #[test]
    fn create_view_rejects_builtin_name() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let err = engine
            .execute("CREATE VIEW blocks AS SELECT 1")
            .unwrap_err();
        assert!(err.to_string().contains("built-in"));
    }

    #[test]
    fn create_view_rejects_duplicate_without_if_not_exists() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        engine.execute("CREATE VIEW v AS SELECT 1").unwrap();
        let err = engine.execute("CREATE VIEW v AS SELECT 2").unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn create_view_if_not_exists_is_a_noop() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        engine.execute("CREATE VIEW v AS SELECT 1").unwrap();
        let out = engine
            .execute("CREATE VIEW IF NOT EXISTS v AS SELECT 2")
            .unwrap();
        assert_eq!(out.rows[0][0], "already exists");

        let sel = engine.execute("SELECT * FROM v").unwrap();
        assert_eq!(sel.rows, vec![vec!["1".to_string()]]);
    }

    #[test]
    fn create_view_or_replace_overwrites() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        engine.execute("CREATE VIEW v AS SELECT 1").unwrap();
        engine
            .execute("CREATE OR REPLACE VIEW v AS SELECT 2")
            .unwrap();
        let out = engine.execute("SELECT * FROM v").unwrap();
        assert_eq!(out.rows, vec![vec!["2".to_string()]]);
    }

    #[test]
    fn create_view_rejects_name_colliding_with_custom_table() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        engine.execute("CREATE TABLE t (x TEXT)").unwrap();
        let err = engine.execute("CREATE VIEW t AS SELECT 1").unwrap_err();
        assert!(err.to_string().contains("table"));
    }

    #[test]
    fn create_table_rejects_name_colliding_with_view() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        engine.execute("CREATE VIEW v AS SELECT 1").unwrap();
        let err = engine.execute("CREATE TABLE v (x TEXT)").unwrap_err();
        assert!(err.to_string().contains("view"));
    }

    #[test]
    fn view_detects_circular_reference() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        engine.execute("CREATE VIEW a AS SELECT 1").unwrap();
        engine.execute("CREATE VIEW b AS SELECT * FROM a").unwrap();
        // At validation time this only sees b's current (non-circular)
        // definition, so it succeeds — but now a -> b -> a is a real cycle.
        engine
            .execute("CREATE OR REPLACE VIEW a AS SELECT * FROM b")
            .unwrap();

        let err = engine.execute("SELECT * FROM a").unwrap_err();
        assert!(err.to_string().contains("circular"));
    }

    #[test]
    fn drop_view_removes_it() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        engine.execute("CREATE VIEW v AS SELECT 1").unwrap();
        engine.execute("DROP VIEW v").unwrap();
        let err = engine.execute("SELECT * FROM v").unwrap_err();
        assert!(err.to_string().contains("unknown table"));
    }

    #[test]
    fn drop_view_if_exists_on_missing_is_noop() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine.execute("DROP VIEW IF EXISTS missing").unwrap();
        assert!(out.rows[0][0].contains("0 view"));
    }

    #[test]
    fn show_tables_lists_views() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        engine.execute("CREATE VIEW v AS SELECT 1").unwrap();
        let out = engine.execute("SHOW TABLES").unwrap();
        assert!(out.rows.iter().any(|r| r[0] == "v" && r[1] == "view"));
    }

    #[test]
    fn desc_view_reports_query_columns() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        engine
            .execute("CREATE VIEW headings AS SELECT content, depth FROM blocks WHERE block_type = 'heading'")
            .unwrap();
        let out = engine.execute("DESC headings").unwrap();
        let cols: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(cols, vec!["content", "depth"]);
    }

    #[test]
    fn view_persists_across_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let md_path = write_md(&dir, "doc.md", "# Title\n\nHello\n");
        let path = dir.path().join("test.mq-db");

        let mut store = DocumentStore::new();
        store.add_file(&md_path).unwrap();
        store
            .execute_sql_mut(
                "CREATE VIEW paras AS SELECT content FROM blocks WHERE block_type = 'paragraph'",
            )
            .unwrap();
        store.save(&path).unwrap();

        let reloaded = DocumentStore::load(&path).unwrap();
        let engine = SqlEngine::new(&reloaded).unwrap();
        let out = engine.execute("SELECT content FROM paras").unwrap();
        assert_eq!(out.rows, vec![vec!["Hello".to_string()]]);

        // Still live after reload, not a frozen snapshot.
        let mut reloaded = reloaded;
        reloaded
            .execute_sql_mut("UPDATE blocks SET content = 'Updated' WHERE content = 'Hello'")
            .unwrap();
        let out = SqlEngine::new(&reloaded)
            .unwrap()
            .execute("SELECT content FROM paras")
            .unwrap();
        assert_eq!(out.rows, vec![vec!["Updated".to_string()]]);
    }

    #[test]
    fn view_works_through_execute_sql_mut() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "doc.md", "# Title\n\nBody\n");
        let mut store = DocumentStore::new();
        store.add_file(&path).unwrap();

        store
            .execute_sql_mut(
                "CREATE VIEW v AS SELECT content FROM blocks WHERE block_type = 'heading'",
            )
            .unwrap();
        let out = store.execute_sql_mut("SELECT content FROM v").unwrap();
        assert_eq!(out.rows, vec![vec!["Title".to_string()]]);
    }

    // read_csv() / read_json() table functions

    #[test]
    fn read_csv_selects_rows_with_header_as_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(
            &dir,
            "people.csv",
            "name,age\n\"Ann, B\",30\n\"She said \"\"hi\"\"\",25\n",
        );
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(&format!(
                "SELECT name, age FROM read_csv('{}') ORDER BY age",
                path.display()
            ))
            .unwrap();
        assert_eq!(
            out.rows,
            vec![
                vec!["She said \"hi\"".to_string(), "25".to_string()],
                vec!["Ann, B".to_string(), "30".to_string()],
            ]
        );
    }

    #[test]
    fn read_csv_supports_numeric_where_and_arithmetic() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "people.csv", "name,age\nAnn,30\nCarl,25\n");
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(&format!(
                "SELECT name, age + 1 FROM read_csv('{}') WHERE age > 26",
                path.display()
            ))
            .unwrap();
        assert_eq!(out.rows, vec![vec!["Ann".to_string(), "31".to_string()]]);
    }

    #[test]
    fn read_csv_pads_ragged_rows_with_null() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "ragged.csv", "a,b,c\n1,2,3\n4\n");
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(&format!(
                "SELECT a, b, c FROM read_csv('{}') ORDER BY a",
                path.display()
            ))
            .unwrap();
        assert_eq!(
            out.rows,
            vec![
                vec!["1".to_string(), "2".to_string(), "3".to_string()],
                vec!["4".to_string(), "NULL".to_string(), "NULL".to_string()],
            ]
        );
    }

    #[test]
    fn read_csv_rejects_missing_file_with_clear_error() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        let err = engine
            .execute("SELECT * FROM read_csv('/no/such/file.csv')")
            .unwrap_err();
        assert!(err.to_string().contains("read_csv"));
    }

    #[test]
    fn read_json_selects_rows_from_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(
            &dir,
            "people.jsonl",
            "{\"name\":\"Ann\",\"age\":30,\"active\":true}\n{\"name\":\"Carl\",\"age\":25,\"active\":false}\n",
        );
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(&format!(
                "SELECT name, age, active FROM read_json('{}') WHERE age > 26",
                path.display()
            ))
            .unwrap();
        assert_eq!(
            out.rows,
            vec![vec![
                "Ann".to_string(),
                "30".to_string(),
                "true".to_string()
            ]]
        );
    }

    #[test]
    fn read_json_unions_columns_across_varying_objects() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(
            &dir,
            "mixed.jsonl",
            "{\"name\":\"Ann\",\"age\":30}\n{\"name\":\"Carl\",\"city\":\"NYC\"}\n",
        );
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        let out = engine
            .execute(&format!(
                "SELECT name, age, city FROM read_json('{}') ORDER BY name",
                path.display()
            ))
            .unwrap();
        assert_eq!(
            out.rows,
            vec![
                vec!["Ann".to_string(), "30".to_string(), "NULL".to_string()],
                vec!["Carl".to_string(), "NULL".to_string(), "NYC".to_string()],
            ]
        );
    }

    #[test]
    fn read_json_rejects_non_object_line_with_line_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "bad.jsonl", "{\"a\":1}\n[1,2,3]\n");
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        let err = engine
            .execute(&format!("SELECT * FROM read_json('{}')", path.display()))
            .unwrap_err();
        assert!(err.to_string().contains("line 2"));
    }

    #[test]
    fn read_csv_works_inside_create_table_as_select() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_md(&dir, "people.csv", "name,age\nAnn,30\n");
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        engine
            .execute(&format!(
                "CREATE TABLE people AS SELECT * FROM read_csv('{}')",
                path.display()
            ))
            .unwrap();
        let out = engine.execute("SELECT name FROM people").unwrap();
        assert_eq!(out.rows, vec![vec!["Ann".to_string()]]);
    }

    #[test]
    fn read_table_function_rejects_unknown_name() {
        let store = DocumentStore::new();
        let engine = SqlEngine::new(&store).unwrap();
        let err = engine
            .execute("SELECT * FROM read_parquet('/tmp/x.parquet')")
            .unwrap_err();
        assert!(err.to_string().contains("unknown table function"));
    }

    #[test]
    fn vacuum_statement_redirects_to_cli() {
        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let err = engine.execute("VACUUM").unwrap_err();
        assert!(err.to_string().contains("mq-db vacuum"));

        let mut store = DocumentStore::new();
        store.add_str("# Title\n\nBody\n").unwrap();
        let err = store.execute_sql_mut("VACUUM").unwrap_err();
        assert!(err.to_string().contains("mq-db vacuum"));
    }

    // ATTACH / DETACH

    fn saved_store(dir: &tempfile::TempDir, name: &str, md: &str) -> std::path::PathBuf {
        let mut s = DocumentStore::new();
        s.add_str(md).unwrap();
        let path = dir.path().join(name);
        s.save(&path).unwrap();
        path
    }

    #[test]
    fn attach_selects_rows_from_other_store() {
        let dir = tempfile::tempdir().unwrap();
        let other_path = saved_store(&dir, "other.mq-db", "# Other Doc\n\nOther body\n");

        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        engine
            .execute(&format!(
                "ATTACH DATABASE '{}' AS other",
                other_path.display()
            ))
            .unwrap();

        let out = engine
            .execute("SELECT content FROM other.blocks WHERE block_type = 'heading'")
            .unwrap();
        assert_eq!(out.rows, vec![vec!["Other Doc".to_string()]]);
    }

    #[test]
    fn attach_join_across_local_and_other_store() {
        let dir = tempfile::tempdir().unwrap();
        let other_path = saved_store(&dir, "other.mq-db", "# Other Doc\n\nOther body\n");

        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        engine
            .execute(&format!(
                "ATTACH DATABASE '{}' AS other",
                other_path.display()
            ))
            .unwrap();

        let out = engine
            .execute(
                "SELECT b.content, o.content FROM blocks b JOIN other.blocks o \
                 ON b.block_type = o.block_type WHERE b.block_type = 'heading'",
            )
            .unwrap();
        assert!(!out.rows.is_empty());
    }

    #[test]
    fn detach_makes_alias_unknown_again() {
        let dir = tempfile::tempdir().unwrap();
        let other_path = saved_store(&dir, "other.mq-db", "# Other Doc\n\nOther body\n");

        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        engine
            .execute(&format!(
                "ATTACH DATABASE '{}' AS other",
                other_path.display()
            ))
            .unwrap();
        engine.execute("DETACH other").unwrap();

        let err = engine.execute("SELECT * FROM other.blocks").unwrap_err();
        assert!(err.to_string().contains("unknown database"));

        let err = engine.execute("DETACH other").unwrap_err();
        assert!(err.to_string().contains("not attached"));
    }

    #[test]
    fn attach_rejects_duplicate_alias() {
        let dir = tempfile::tempdir().unwrap();
        let other_path = saved_store(&dir, "other.mq-db", "# Other Doc\n\nOther body\n");

        let store = make_store();
        let engine = SqlEngine::new(&store).unwrap();
        let attach_sql = format!("ATTACH DATABASE '{}' AS other", other_path.display());
        engine.execute(&attach_sql).unwrap();

        let err = engine.execute(&attach_sql).unwrap_err();
        assert!(err.to_string().contains("already attached"));
    }

    #[test]
    fn qualified_writes_to_attached_store_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let other_path = saved_store(&dir, "other.mq-db", "# Other Doc\n\nOther body\n");

        let mut store = make_store();
        {
            let engine = SqlEngine::new(&store).unwrap();
            engine
                .execute(&format!(
                    "ATTACH DATABASE '{}' AS other",
                    other_path.display()
                ))
                .unwrap();
        }

        let err = SqlEngine::new(&store)
            .unwrap()
            .execute("CREATE TABLE other.t (a TEXT)")
            .unwrap_err();
        assert!(err.to_string().contains("not supported"));

        let err = store
            .execute_sql_mut("UPDATE other.blocks SET content = 'x' WHERE block_type = 'heading'")
            .unwrap_err();
        assert!(err.to_string().contains("not supported"));

        let err = store
            .execute_sql_mut(
                "INSERT INTO other.blocks (block_type, content) VALUES ('paragraph', 'x')",
            )
            .unwrap_err();
        assert!(err.to_string().contains("not supported"));
    }
}
