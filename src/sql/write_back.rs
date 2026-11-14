//! `UPDATE`/`DELETE`/`INSERT` on `blocks` with write-back to Markdown files.

use super::engine::SqlEngine;
use super::expr::eval_expr;
use super::output::QueryOutput;
use super::rewrite::fold_constants;
use super::rows::{ident_value, ok_result, require_unqualified};
use super::source_file;
use super::text_edit;
use super::value::{Row, Value};
use crate::block::{Block, BlockType};
use crate::indexes::IndexHint;
use crate::{DocumentStore, MqdbError};
use rustc_hash::{FxHashMap, FxHashSet};
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

/// Whether `sql` would modify Markdown files, i.e. `UPDATE`/`DELETE`/`INSERT`
/// targeting `blocks`. Statements on custom tables do not. A target that is
/// not a plain table name counts as needing write-back, the safe default; SQL
/// that does not parse needs nothing, since executing it only reports the
/// parse error.
pub fn statement_needs_write_back(sql: &str) -> bool {
    let Ok(stmts) = Parser::parse_sql(&GenericDialect {}, sql) else {
        return false;
    };
    let is_blocks = |twj: &TableWithJoins| single_table_name(twj).is_ok_and(|n| n == "blocks");
    match stmts.first() {
        Some(Statement::Update(update)) => {
            single_table_name(&update.table).map_or(true, |n| n == "blocks")
        }
        Some(Statement::Delete(delete)) => {
            let (FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables)) =
                &delete.from;
            tables.len() != 1 || tables.iter().any(is_blocks) || !delete.tables.is_empty()
        }
        Some(Statement::Insert(ins)) => match &ins.table {
            TableObject::TableName(name) => {
                require_unqualified(name).map_or(true, |n| n == "blocks")
            }
            _ => true,
        },
        _ => false,
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

/// Extra rendering inputs for non-text blocks.
#[derive(Default, Clone)]
struct RenderOpts {
    depth: Option<u8>,
    lang: Option<String>,
    meta: Option<String>,
    /// `false` renders an indented (non-fenced) code block.
    indented_code: bool,
}

/// Whether `UPDATE ... SET content` can rewrite blocks of this type.
fn is_updatable_type(bt: &BlockType) -> bool {
    matches!(
        bt,
        BlockType::Heading
            | BlockType::Paragraph
            | BlockType::Code
            | BlockType::Blockquote
            | BlockType::Math
            | BlockType::Html
            | BlockType::List
            | BlockType::TableCell
    )
}

/// Whether `INSERT INTO blocks` can create blocks of this type. A table cell
/// cannot be inserted on its own.
fn is_insertable_type(bt: &BlockType) -> bool {
    is_updatable_type(bt) && *bt != BlockType::TableCell
}

const WRITABLE_TYPES: &str = "heading/paragraph/code/blockquote/math/html/list";

/// Renders Markdown source text for a block that is replaced or inserted as
/// whole lines. List items and table cells are edited in place instead, see
/// [`Action`].
fn render_markdown_for(
    block_type: &BlockType,
    opts: &RenderOpts,
    content: &str,
) -> Result<String, MqdbError> {
    match block_type {
        BlockType::Heading => Ok(format!(
            "{} {}",
            "#".repeat(opts.depth.unwrap_or(1).max(1) as usize),
            content
        )),
        BlockType::Paragraph | BlockType::Html => Ok(content.to_string()),
        BlockType::Code if opts.indented_code => Ok(content
            .lines()
            .map(|l| format!("    {l}"))
            .collect::<Vec<_>>()
            .join("\n")),
        BlockType::Code => {
            let longest_run = content.split(|c| c != '`').map(str::len).max().unwrap_or(0);
            let fence = "`".repeat((longest_run + 1).max(3));
            let mut info = opts.lang.clone().unwrap_or_default();
            if let Some(meta) = opts.meta.as_deref().filter(|m| !m.is_empty()) {
                info.push(' ');
                info.push_str(meta);
            }
            Ok(format!("{fence}{info}\n{content}\n{fence}"))
        }
        BlockType::Math => Ok(format!("$$\n{content}\n$$")),
        BlockType::Blockquote => Ok(content
            .lines()
            .map(|l| {
                if l.is_empty() {
                    ">".to_string()
                } else {
                    format!("> {l}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")),
        other => Err(MqdbError::SqlExec(format!(
            "write-back is only supported for {WRITABLE_TYPES} blocks (found {})",
            other.as_str()
        ))),
    }
}

/// Renders `edit`'s replacement text for an existing matched block,
/// keeping its depth / language / fence style.
fn render_replacement(block: &Block, new_content: &str) -> Result<String, MqdbError> {
    let prop_str = |k: &str| {
        block
            .properties
            .get(k)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let opts = RenderOpts {
        depth: block.heading_depth(),
        lang: prop_str("lang"),
        meta: prop_str("meta"),
        indented_code: block
            .properties
            .get("fence")
            .and_then(|v| v.as_bool())
            .is_some_and(|f| !f),
    };
    render_markdown_for(&block.block_type, &opts, new_content)
}

/// One planned change to a document's source text. Line numbers are
/// 1-based; columns count UTF-16 units, as the parser reports them.
enum Action {
    /// Replace lines `start_line..=end_line` with `text`.
    ReplaceLines {
        start_line: usize,
        end_line: usize,
        text: String,
    },
    /// Replace the text between two `(line, column)` points. Used for list
    /// items, whose span is just their text: the indent, marker and task
    /// checkbox before it stay as they are.
    ReplaceRange {
        start: (usize, usize),
        end: (usize, usize),
        text: String,
    },
    /// Replace one cell of the table row on `line`.
    SetCell {
        line: usize,
        column: usize,
        text: String,
    },
    /// Remove lines `start_line..=end_line`.
    RemoveLines { start_line: usize, end_line: usize },
}

impl Action {
    /// Where the change starts. Applying from the largest to the smallest
    /// keeps the positions of the changes still to come valid.
    fn position(&self) -> (usize, usize) {
        match self {
            Action::ReplaceLines { start_line, .. } | Action::RemoveLines { start_line, .. } => {
                (*start_line, 0)
            }
            Action::ReplaceRange { start, .. } => *start,
            Action::SetCell { line, column, .. } => (*line, *column),
        }
    }
}

fn single_line(what: &str, text: &str) -> Result<(), MqdbError> {
    if text.contains(['\n', '\r']) {
        return Err(MqdbError::SqlExec(format!(
            "{what} content must be a single line"
        )));
    }
    Ok(())
}

/// Decides how to apply `edit` to `block`. `matched` holds the `pre` of every
/// block the statement touches in this document.
fn plan_edit(
    blocks: &[Block],
    block: &Block,
    edit: &MatchedBlockEdit,
    matched: &FxHashSet<u32>,
) -> Result<Action, MqdbError> {
    let span = block.span.as_ref().ok_or_else(|| {
        MqdbError::SqlExec("write-back requires source spans; reindex without --no-spans".into())
    })?;
    let Some(new_content) = edit.new_content.as_deref() else {
        // DELETE
        match block.block_type {
            BlockType::TableCell | BlockType::TableAlign | BlockType::TableRow => {
                return Err(MqdbError::SqlExec(
                    "write-back cannot delete a table cell or row on its own; \
                     UPDATE the cell's content instead"
                        .into(),
                ));
            }
            BlockType::List => {
                let nested = list_descendants(blocks, block);
                if nested.iter().any(|b| !matched.contains(&b.pre)) {
                    return Err(MqdbError::SqlExec(format!(
                        "list item '{}' has nested items; delete those in the same \
                         statement, or the nested items would be left behind",
                        block.content.lines().next().unwrap_or("")
                    )));
                }
            }
            _ => {}
        }
        return Ok(Action::RemoveLines {
            start_line: span.start_line,
            end_line: span.end_line,
        });
    };
    // UPDATE
    match block.block_type {
        BlockType::List => {
            single_line("list item", new_content)?;
            Ok(Action::ReplaceRange {
                start: (span.start_line, span.start_col),
                end: (span.end_line, span.end_col),
                text: new_content.to_string(),
            })
        }
        BlockType::TableCell => {
            let column = block
                .properties
                .get("column")
                .and_then(|v| v.as_int())
                .and_then(|c| usize::try_from(c).ok())
                .ok_or_else(|| MqdbError::SqlExec("table cell has no column number".into()))?;
            text_edit::escape_cell(new_content)?;
            Ok(Action::SetCell {
                line: span.start_line,
                column,
                text: new_content.to_string(),
            })
        }
        _ => Ok(Action::ReplaceLines {
            start_line: span.start_line,
            end_line: span.end_line,
            text: render_replacement(block, new_content)?,
        }),
    }
}

/// The list items nested under `item`: the run of list blocks right after it
/// with a deeper level.
fn list_descendants<'a>(blocks: &'a [Block], item: &Block) -> Vec<&'a Block> {
    let level = list_level(item);
    let Some(idx) = blocks.iter().position(|b| b.pre == item.pre) else {
        return Vec::new();
    };
    blocks[idx + 1..]
        .iter()
        .take_while(|b| b.block_type == BlockType::List && list_level(b) > level)
        .collect()
}

fn list_level(block: &Block) -> i64 {
    block
        .properties
        .get("level")
        .and_then(|v| v.as_int())
        .unwrap_or(0)
}

/// Applies one [`Action`] to `lines`.
fn apply_action(lines: &mut Vec<String>, action: &Action) -> Result<(), MqdbError> {
    let stale = |line: usize| {
        MqdbError::SqlExec(format!(
            "line {line} no longer matches the index; the file changed since it was indexed (re-run `mq-db index`)"
        ))
    };
    match action {
        Action::ReplaceLines {
            start_line,
            end_line,
            text,
        } => {
            let start = start_line.saturating_sub(1);
            let end = (*end_line).min(lines.len());
            if start < end && start < lines.len() {
                lines.splice(start..end, std::iter::once(text.clone()));
            }
        }
        Action::RemoveLines {
            start_line,
            end_line,
        } => {
            let start = start_line.saturating_sub(1);
            let end = (*end_line).min(lines.len());
            if start < end && start < lines.len() {
                text_edit::remove_lines(lines, start, end);
            }
        }
        Action::ReplaceRange { start, end, text } => {
            let (first, last) = (start.0.checked_sub(1), end.0.checked_sub(1));
            let (Some(first), Some(last)) = (first, last) else {
                return Err(stale(start.0));
            };
            if first > last || last >= lines.len() {
                return Err(stale(start.0));
            }
            let from =
                text_edit::byte_offset(&lines[first], start.1).ok_or_else(|| stale(start.0))?;
            let to = text_edit::byte_offset(&lines[last], end.1).ok_or_else(|| stale(end.0))?;
            let prefix = &lines[first][..from];
            if !text_edit::is_list_prefix(prefix) {
                return Err(stale(start.0));
            }
            let replaced = format!("{prefix}{text}{}", &lines[last][to..]);
            lines.splice(first..=last, std::iter::once(replaced));
        }
        Action::SetCell { line, column, text } => {
            let idx = line.checked_sub(1).filter(|i| *i < lines.len());
            let idx = idx.ok_or_else(|| stale(*line))?;
            lines[idx] = text_edit::set_table_cell(&lines[idx], *column, text)?;
        }
    }
    Ok(())
}

/// One document's new source text, computed and checked but not yet written.
struct Patch {
    doc_id: u32,
    path: std::path::PathBuf,
    original: String,
    patched: String,
}

/// Writes every patch, then re-parses each document in the store.
///
/// All the patches were computed and checked before this runs, so a bad
/// statement writes nothing. Each file is replaced atomically. If a later
/// write fails, the files already replaced are put back, so a multi-file
/// statement is applied to all of its files or to none. Two documents that
/// resolve to the same file are refused, since their patches would overwrite
/// each other.
fn commit_patches(store: &mut DocumentStore, patches: &[Patch]) -> Result<(), MqdbError> {
    let mut seen = FxHashSet::default();
    for p in patches {
        let target = std::fs::canonicalize(&p.path)?;
        if !seen.insert(target.clone()) {
            return Err(MqdbError::SqlExec(format!(
                "several documents in this statement share the source file {}; edit them in separate statements",
                target.display()
            )));
        }
    }
    let restore = |patches: &[Patch]| -> Vec<String> {
        patches
            .iter()
            .filter_map(|p| {
                source_file::write_atomic(&p.path, &p.original)
                    .err()
                    .map(|e| e.to_string())
            })
            .collect()
    };
    let with_failures = |e: MqdbError, failed: Vec<String>| {
        if failed.is_empty() {
            e
        } else {
            MqdbError::SqlExec(format!("{e}; could not restore: {}", failed.join("; ")))
        }
    };
    for (written, patch) in patches.iter().enumerate() {
        if let Err(e) = source_file::write_atomic(&patch.path, &patch.patched) {
            // The failing file may already have been replaced (e.g. the
            // directory sync after the rename failed), so restore it too.
            let failed = restore(&patches[..=written]);
            return Err(with_failures(e, failed));
        }
    }
    let items: Vec<_> = patches
        .iter()
        .map(|p| (p.doc_id, p.patched.clone(), Some(p.path.clone())))
        .collect();
    if let Err(e) = store.replace_documents(&items) {
        let failed = restore(patches);
        return Err(with_failures(e, failed));
    }
    Ok(())
}

/// Applies `edits` (grouped by document) as a source-text patch + reparse:
/// for each affected document, reads the *current* file off disk, applies
/// each matched block's planned [`Action`] (replacing its lines, its list-item
/// text or a table cell, or removing its lines for a `DELETE`), writes the
/// patched text back to the file, then calls
/// [`DocumentStore::replace_document`] to re-parse it in place (same
/// `DocumentId`, fresh blocks/index/catalog entry).
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
    let mut patches = Vec::with_capacity(by_doc.len());
    for (doc_id, doc_edits) in by_doc {
        let (path, original, mut actions) = {
            let doc = store
                .get_document(doc_id)
                .ok_or_else(|| MqdbError::SqlExec(format!("no such document: {doc_id}")))?;
            let path = doc.path.clone().ok_or_else(|| {
                MqdbError::SqlExec(
                    "cannot write back: document has no source file (added via add_str)".into(),
                )
            })?;

            let original = std::fs::read_to_string(&path)?;
            source_file::ensure_fresh(doc, &original)?;

            let matched: FxHashSet<u32> = doc_edits.iter().map(|e| e.pre).collect();
            let mut actions = Vec::with_capacity(doc_edits.len());
            for edit in &doc_edits {
                let block = doc
                    .blocks
                    .iter()
                    .find(|b| b.pre == edit.pre)
                    .ok_or_else(|| MqdbError::SqlExec("matched block no longer exists".into()))?;
                actions.push(plan_edit(&doc.blocks, block, edit, &matched)?);
            }
            (path, original, actions)
        };

        let mut source = source_file::SourceLines::parse(&original)?;
        // Apply from the bottom up so earlier edits don't shift later
        // (already-resolved) positions.
        actions.sort_by_key(|a| std::cmp::Reverse(a.position()));
        for action in &actions {
            apply_action(&mut source.lines, action)?;
        }
        affected += doc_edits.len();
        patches.push(Patch {
            doc_id,
            path,
            original,
            patched: source.join(),
        });
    }

    // Everything is planned and checked; only now touch any file.
    commit_patches(store, &patches)?;
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
    /// Info-string language; only valid for `Code`.
    lang: Option<String>,
    /// `pre` of the block to insert after; `None` appends at document end.
    after_pre: Option<u32>,
    /// Position within `VALUES`, to preserve order among same-anchor rows.
    row_index: usize,
}

const INSERT_BLOCKS_COLUMNS: [&str; 6] = [
    "document_id",
    "block_type",
    "content",
    "depth",
    "after_pre",
    "lang",
];

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
        columns: vec![].into(),
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
            let mut lang: Option<String> = None;

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
                            .filter(is_insertable_type)
                            .ok_or_else(|| {
                                MqdbError::SqlExec(format!(
                                    "write-back is only supported for {WRITABLE_TYPES} blocks (found {s})"
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
                    "lang" => {
                        lang = match value {
                            Value::Null => None,
                            other => Some(other.display()),
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

            if block_type == BlockType::List {
                single_line("list item", &content)?;
            }
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
                _ if depth.is_some() => {
                    return Err(MqdbError::SqlExec(
                        "depth is only valid for block_type 'heading'".into(),
                    ));
                }
                _ => {}
            }
            if lang.is_some() && block_type != BlockType::Code {
                return Err(MqdbError::SqlExec(
                    "lang is only valid for block_type 'code'".into(),
                ));
            }

            Ok(NewBlockSpec {
                document_id,
                block_type,
                content,
                depth,
                lang,
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
    let mut patches = Vec::with_capacity(by_doc.len());
    for (doc_id, doc_specs) in by_doc {
        /// What to write for one inserted block.
        enum Rendered {
            /// Ready-made text, separated from its neighbours by blank lines.
            Block(String),
            /// A sibling of the list item whose text starts at
            /// (`marker_line`, `text_col`): it copies that item's indent and
            /// marker, and goes in without blank lines so the list stays
            /// tight. Resolved once the file's lines are known.
            ListItem {
                marker_line: usize,
                text_col: usize,
                content: String,
            },
        }

        struct Insertion {
            /// 0-indexed line to insert before. `usize::MAX` means "end of
            /// file", resolved once the line count is known, below.
            at: usize,
            row_index: usize,
            rendered: Rendered,
        }

        let (path, original, mut insertions) = {
            let doc = store
                .get_document(doc_id)
                .ok_or_else(|| MqdbError::SqlExec(format!("no such document: {doc_id}")))?;
            let path = doc.path.clone().ok_or_else(|| {
                MqdbError::SqlExec(
                    "cannot write back: document has no source file (added via add_str)".into(),
                )
            })?;

            let original = std::fs::read_to_string(&path)?;
            source_file::ensure_fresh(doc, &original)?;

            let mut insertions = Vec::with_capacity(doc_specs.len());
            for spec in &doc_specs {
                let anchor = match spec.after_pre {
                    Some(pre) => {
                        Some(doc.blocks.iter().find(|b| b.pre == pre).ok_or_else(|| {
                            MqdbError::SqlExec(format!(
                                "after_pre {pre} does not match any block in document {doc_id}"
                            ))
                        })?)
                    }
                    None => None,
                };
                let anchor_span = match anchor {
                    Some(block) => Some(block.span.as_ref().ok_or_else(|| {
                        MqdbError::SqlExec(
                            "write-back requires source spans; reindex without --no-spans".into(),
                        )
                    })?),
                    None => None,
                };
                let mut at = anchor_span.map_or(usize::MAX, |span| span.end_line);

                let rendered = match (&spec.block_type, anchor, anchor_span) {
                    (BlockType::List, Some(block), Some(span))
                        if block.block_type == BlockType::List =>
                    {
                        // After the anchor's whole subtree, not between it and
                        // its nested items.
                        let subtree_end = list_descendants(&doc.blocks, block)
                            .iter()
                            .filter_map(|b| b.span.as_ref())
                            .map(|s| s.end_line)
                            .fold(span.end_line, usize::max);
                        at = subtree_end;
                        Rendered::ListItem {
                            marker_line: span.start_line,
                            text_col: span.start_col,
                            content: spec.content.clone(),
                        }
                    }
                    // A list item with no list item to follow starts a new list.
                    (BlockType::List, _, _) => Rendered::Block(format!("- {}", spec.content)),
                    _ => {
                        let opts = RenderOpts {
                            depth: spec.depth,
                            lang: spec.lang.clone(),
                            ..RenderOpts::default()
                        };
                        Rendered::Block(render_markdown_for(
                            &spec.block_type,
                            &opts,
                            &spec.content,
                        )?)
                    }
                };
                insertions.push(Insertion {
                    at,
                    row_index: spec.row_index,
                    rendered,
                });
            }
            (path, original, insertions)
        };

        let mut source = source_file::SourceLines::parse(&original)?;
        let lines = &mut source.lines;

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
            let (text, tight) = match &insertion.rendered {
                Rendered::Block(text) => (text.clone(), false),
                Rendered::ListItem {
                    marker_line,
                    text_col,
                    content,
                } => {
                    let stale = || {
                        MqdbError::SqlExec(format!(
                            "line {marker_line} no longer matches the index; the file changed since it was indexed (re-run `mq-db index`)"
                        ))
                    };
                    let line = lines.get(marker_line - 1).ok_or_else(stale)?;
                    let offset = text_edit::byte_offset(line, *text_col).ok_or_else(stale)?;
                    let prefix = text_edit::sibling_prefix(&line[..offset]).ok_or_else(stale)?;
                    (format!("{prefix}{content}"), true)
                }
            };
            let mut new_lines = vec![text];
            if !tight {
                let needs_leading_blank = at > 0 && !lines[at - 1].trim().is_empty();
                let needs_trailing_blank = at < lines.len() && !lines[at].trim().is_empty();
                if needs_trailing_blank {
                    new_lines.push(String::new());
                }
                if needs_leading_blank {
                    new_lines.insert(0, String::new());
                }
            }
            lines.splice(at..at, new_lines);
        }

        inserted += doc_specs.len();
        patches.push(Patch {
            doc_id,
            path,
            original,
            patched: source.join(),
        });
    }

    // Everything is planned and checked; only now touch any file.
    commit_patches(store, &patches)?;
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
        if upper.starts_with("DESC ")
            || upper.starts_with("DESCRIBE ")
            || upper == "SHOW TABLES"
            || upper == "SHOW INDEXES"
            || upper == "SHOW INDEX"
        {
            return SqlEngine::with_session(self, self.write_session)?.execute(sql);
        }

        let stmts = Parser::parse_sql(&GenericDialect {}, sql)
            .map_err(|e| MqdbError::SqlParse(e.to_string()))?;
        let stmt = stmts
            .into_iter()
            .next()
            .ok_or_else(|| MqdbError::SqlParse("empty query".into()))?;

        match stmt {
            Statement::Update(_) if !statement_needs_write_back(sql) => {
                SqlEngine::with_session(self, self.write_session)?.execute(sql)
            }
            Statement::Delete(_) if !statement_needs_write_back(sql) => {
                SqlEngine::with_session(self, self.write_session)?.execute(sql)
            }
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
