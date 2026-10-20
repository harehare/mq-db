//! Custom SQL execution engine for mq-db.
//!
//! Executes SQL queries directly against the in-memory [`DocumentStore`](crate::DocumentStore)
//! without copying data into an external database. Uses [`sqlparser`] to parse
//! SQL and evaluates predicates natively against [`Block`](crate::block::Block) data, including the
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

mod aggregate;
mod analysis;
mod datetime;
mod ddl;
mod dml;
mod engine;
mod explain;
mod expr;
mod external;
mod fts;
mod join;
mod output;
mod pattern;
mod plan;
mod query;
mod result_ops;
mod rewrite;
mod rows;
mod scalar;
mod scan;
#[cfg(test)]
mod tests;
mod value;
mod write_back;

pub use engine::SqlEngine;
pub use output::{QueryOutput, html_escape};
pub use value::Value;
pub use write_back::statement_needs_write_back;
