//! `UPDATE`/`DELETE`/`INSERT` on `blocks` with write-back to Markdown files.

use super::engine::SqlEngine;
use super::expr::eval_expr;
use super::output::QueryOutput;
use super::rewrite::fold_constants;
use super::rows::{ident_value, ok_result, require_unqualified};
use super::value::{Row, Value};
use crate::block::{Block, BlockType};
use crate::indexes::IndexHint;
use crate::{DocumentStore, MqdbError};
use rustc_hash::FxHashMap;
use sqlparser::ast::{
    AssignmentTarget, Expr, FromTable, Insert, SetExpr, Statement, TableFactor, TableObject,
    TableWithJoins, Values,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

/// A single matched `blocks` row targeted by `UPDATE`/`DELETE`, identified
/// by `(document_id, pre)`: `pre` is a unique per-document DFS number, so
/// this is stable even though the SQL-visible `id` column is a store-wide
/// running index that doesn't correspond to any field on [`Block`].
struct MatchedBlockEdit {
    document_id: u32,
    pub(super) pre: u32,
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
        let resolved = fold_constants(&engine.resolve_subqueries(sel, None)?);
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
                        // first block): swallow it.
                        remove_end += 1;
                    } else if remove_start > 0 && lines[remove_start - 1].trim().is_empty() {
                        // No blank line after (block was the last one in the
                        // file): swallow the blank line before it instead.
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
    pub(super) block_type: BlockType,
    pub(super) content: String,
    /// Required (1-6) iff `block_type` is `Heading`.
    pub(super) depth: Option<u8>,
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
    /// `UPDATE`/`DELETE` against the `blocks` table are handled directly,
    /// see the module-level write-back notes above, and are written back
    /// to the affected document's *source Markdown file* (re-parsed in
    /// place, same `DocumentId`). Everything else (`SELECT`, `CREATE
    /// TABLE`, `INSERT`, `DROP TABLE`, `DESC`, `SHOW TABLES`) delegates to
    /// the regular read-only [`SqlEngine::execute`].
    ///
    /// Callers that expose this over an interface an end user might not
    /// expect to mutate files (a CLI, an HTTP/MCP endpoint) should gate it
    /// behind an explicit opt-in before calling this: write-back mutates
    /// the user's Markdown source on disk.
    pub fn execute_sql_mut(&mut self, sql: &str) -> Result<QueryOutput, MqdbError> {
        let trimmed = sql.trim().trim_end_matches(';');
        let upper = trimmed.to_ascii_uppercase();
        if upper.starts_with("DESC ") || upper.starts_with("DESCRIBE ") || upper == "SHOW TABLES" {
            return SqlEngine::with_session(self, self.write_session)?.execute(sql);
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
                    SqlEngine::with_session(self, self.write_session)?.execute(sql)
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
            _ => SqlEngine::with_session(self, self.write_session)?.execute(sql),
        }
    }
}
