//! Index-hint selection, zone-map skipping and join planning.

use super::engine::SqlEngine;
use super::fts::query_tokenizer;
use super::output::QueryOutput;
use super::rows::ident_value;
use crate::block::BlockType;
use crate::document::ZoneMaps;
use crate::indexes::{IndexHint, tokenize};
use rustc_hash::FxHashMap;
use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, TableFactor,
    TableWithJoins, Value as SqlValue,
};

/// Flattens a top-level AND-chain into its conjuncts, unwrapping parens.
/// Anything else (including `OR`) is returned as a single, unrecognized leaf.
pub(super) fn flatten_and_conjuncts(expr: &Expr) -> Vec<&Expr> {
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
/// computed key like `nxt.pre = h.pre + 1`); caller falls back to cross-join.
pub(super) fn find_equi_join_exprs<'a>(
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
pub(super) fn zone_map_skip(zone_maps: &ZoneMaps, where_expr: &Expr) -> bool {
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
    // Only safe when `block_type = 'heading'` is also required; `content`
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

/// True if `twj`'s relation is the real `blocks` table, not a `WITH`-clause
/// CTE of the same name shadowing it, which is what `table_rows_with_hint`
/// actually applies [`IndexHint`]s to. Used to gate skipping the WHERE
/// row-by-row recheck: that's only sound when the index was truly consulted.
pub(super) fn from_names_unshadowed_blocks(
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

/// Recognises a single conjunct's index-hint shape; no `AND` handling (see
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
                        // depth 0 means "no heading depth" and isn't in the index
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
/// `expr`'s conjuncts. The actual choice among them is cost-based; see
/// [`SqlEngine::choose_best_hint`].
pub(super) fn candidate_hints_for_where(expr: &Expr) -> Vec<IndexHint> {
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
    let terms = tokenize(&query_str, query_tokenizer());
    if terms.is_empty() { None } else { Some(terms) }
}

/// Returns the column name if the expression is a bare identifier or `alias.col`.
pub(super) fn expr_col_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(i) => Some(i.value.to_lowercase()),
        Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.to_lowercase()),
        _ => None,
    }
}

pub(super) fn expr_str_val(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Value(v) => match &v.value {
            SqlValue::SingleQuotedString(s) | SqlValue::DoubleQuotedString(s) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

pub(super) fn expr_int_val(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Value(v) => match &v.value {
            SqlValue::Number(n, _) => n.parse::<i64>().ok(),
            _ => None,
        },
        _ => None,
    }
}

impl BlockType {
    pub(super) fn from_str(s: &str) -> Option<Self> {
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

impl<'a> SqlEngine<'a> {
    /// Sum of `hint`'s predicted matching-block count across every document,
    /// read directly from each document's already-built secondary index,
    /// with no scanning. Shared by [`Self::choose_best_hint`] and `EXPLAIN`'s plan
    /// describer, so both report the same numbers.
    pub(super) fn estimate_hint_cost(&self, hint: &IndexHint) -> u64 {
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
    pub(super) fn choose_best_hint(&self, candidates: Vec<IndexHint>) -> IndexHint {
        candidates
            .into_iter()
            .min_by_key(|h| self.estimate_hint_cost(h))
            .unwrap_or(IndexHint::FullScan)
    }
}
