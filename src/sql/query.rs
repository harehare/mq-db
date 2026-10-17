//! `SELECT` execution: CTEs, set expressions and subqueries.

use super::analysis::{expr_has_subquery, validate_expr_columns, validate_projection_columns};
use super::engine::SqlEngine;
use super::expr::eval_expr;
use super::external::table_factor_ident;
use super::fts::{
    Bm25CorpusScope, QUERY_HAS_BM25, TableBm25Scope, TableTokenizersScope, TokenizerScope,
};
use super::join::parse_display_value;
use super::output::QueryOutput;
use super::plan::{candidate_hints_for_where, from_names_unshadowed_blocks};
use super::result_ops::{
    apply_limit_offset, combine_set_operation, limit_expr_of, offset_expr_of, unwrap_nested,
};
use super::rewrite::{fold_constants, substitute_outer_refs_query};
use super::value::Row;
use crate::MqdbError;
use crate::indexes::IndexHint;
use rustc_hash::FxHashMap;
use sqlparser::ast::{
    Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Query, Select, SetExpr,
    SetOperator, SetQuantifier, Value as SqlValue, Values,
};

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

impl<'a> SqlEngine<'a> {
    /// Materialises any `WITH` clause's CTEs into a new scope frame, then
    /// delegates to [`Self::exec_query_body`].
    pub(super) fn exec_query(&self, query: &Query) -> Result<QueryOutput, MqdbError> {
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
    /// *previous iteration's new rows* (not the whole accumulated result;
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
        // Match FROM's own tokenizer, not necessarily this engine's; the
        // default for an unqualified column, overridden per-alias below for
        // `match`/`score`/`bm25` calls against a non-driving joined table.
        let _tokenizer_scope = TokenizerScope::set(self.tokenizer_for_from(&select.from));
        let _table_tokenizers_scope =
            TableTokenizersScope::set(self.tokenizer_map_for_from(&select.from));

        // Same driving-table-default + per-alias-override split for
        // `bm25()`'s corpus, gated on whether the statement uses `bm25()` at
        // all (set once in `execute()`) so a plain query never pays for
        // resolving/building a corpus it doesn't need.
        let (default_bm25, table_bm25) = if QUERY_HAS_BM25.with(std::cell::Cell::get) {
            (
                Some(self.bm25_corpus_for_from(&select.from)?),
                self.bm25_corpus_map_for_from(&select.from)?,
            )
        } else {
            (None, FxHashMap::default())
        };
        let _bm25_scope = Bm25CorpusScope::set(default_bm25);
        let _table_bm25_scope = TableBm25Scope::set(table_bm25);

        // 1. Materialise FROM, with cost-based index predicate pushdown
        let where_expr = select.selection.as_ref();
        // A hint built from the whole WHERE clause is only sound to push
        // into `from[0]`'s own scan when there's exactly one FROM table: a
        // JOIN's WHERE may reference a same-named column on a *different*
        // table (e.g. `match(o.content, …)` when `from[0]` also has a
        // `content` column), and pre-filtering `from[0]` by a predicate
        // that isn't actually about `from[0]` can drop rows that should
        // have survived the join. Same reasoning as `zone_filter` below.
        let single_unjoined_from = select.from.len() == 1 && select.from[0].joins.is_empty();
        let hint = if single_unjoined_from {
            where_expr
                .map(|we| self.choose_best_hint(candidate_hints_for_where(we)))
                .unwrap_or(IndexHint::FullScan)
        } else {
            IndexHint::FullScan
        };
        // Unlike `hint`, a skip has no later row-by-row recheck, so only
        // allow it for a single un-joined FROM table (no alias ambiguity).
        let zone_filter = where_expr.filter(|_| single_unjoined_from);
        let mut rows = self.materialise_from_with_hint(&select.from, &hint, zone_filter)?;

        // Best-effort "does this column exist" check, against the schema of
        // whatever FROM actually materialised (joins included). Skipped when
        // that's empty: a `FROM`-less `SELECT` (no schema at all) or a
        // table with zero rows (nothing to infer a schema from), rather
        // than false-reject; the tradeoff is a real typo against an empty
        // table still silently passing, same as before this check existed.
        let known_columns: Vec<String> =
            rows.first().map(|r| r.columns.clone()).unwrap_or_default();
        if !known_columns.is_empty() {
            if let Some(where_expr) = &select.selection {
                validate_expr_columns(where_expr, &known_columns)?;
            }
            validate_projection_columns(&select.projection, &known_columns)?;
        }

        // 2. WHERE (full predicate evaluation; index only pre-filtered)
        //
        // Exception: `WHERE match(content, 'q')` with nothing else ANDed in,
        // against a plain (un-shadowed, un-joined) `blocks` table, is already
        // an *exact* result; `TermIndex::intersect` and `match()` share the
        // same `tokenize()` (see its doc comment), so there's no false
        // positive to filter out. Re-tokenizing every matched block's
        // content here would be pure waste, and for a common query term that
        // can mean re-scanning most of the table.
        let where_fully_indexed = matches!(hint, IndexHint::TermMatch(_))
            && single_unjoined_from
            && matches!(where_expr.map(unwrap_nested), Some(Expr::Function(_)))
            && from_names_unshadowed_blocks(&select.from[0], &self.cte_scopes.borrow());
        if !where_fully_indexed && let Some(where_expr) = &select.selection {
            if expr_has_subquery(where_expr) {
                // A subquery may be correlated (reference this row's
                // columns), so it must be re-resolved per row rather than
                // once up front.
                let mut err = None;
                rows.retain(|row| {
                    if err.is_some() {
                        return false;
                    }
                    match self.resolve_subqueries(where_expr, Some(row)) {
                        Ok(resolved) => eval_expr(&fold_constants(&resolved), row).is_truthy(),
                        Err(e) => {
                            err = Some(e);
                            false
                        }
                    }
                });
                if let Some(e) = err {
                    return Err(e);
                }
            } else {
                let resolved = fold_constants(&self.resolve_subqueries(where_expr, None)?);
                rows.retain(|row| eval_expr(&resolved, row).is_truthy());
            }
        }

        // 3. PROJECT / GROUP / ORDER / LIMIT
        self.project_and_aggregate(select, rows, order_by, limit, offset, &known_columns)
    }

    /// Resolves scalar subqueries, `[NOT] IN (SELECT ...)`, and
    /// `[NOT] EXISTS (SELECT ...)` into constant `Expr`s so the rest of the
    /// evaluator never has to know about them. When `outer_row` is given,
    /// the subquery is first correlated against it (see
    /// [`substitute_outer_refs_query`]) before being executed.
    pub(super) fn resolve_subqueries(
        &self,
        expr: &Expr,
        outer_row: Option<&Row>,
    ) -> Result<Expr, MqdbError> {
        match expr {
            Expr::BinaryOp { left, op, right } => Ok(Expr::BinaryOp {
                left: Box::new(self.resolve_subqueries(left, outer_row)?),
                op: op.clone(),
                right: Box::new(self.resolve_subqueries(right, outer_row)?),
            }),
            Expr::UnaryOp { op, expr: inner } => Ok(Expr::UnaryOp {
                op: *op,
                expr: Box::new(self.resolve_subqueries(inner, outer_row)?),
            }),
            Expr::IsNull(inner) => Ok(Expr::IsNull(Box::new(
                self.resolve_subqueries(inner, outer_row)?,
            ))),
            Expr::IsNotNull(inner) => Ok(Expr::IsNotNull(Box::new(
                self.resolve_subqueries(inner, outer_row)?,
            ))),
            Expr::InList {
                expr: e,
                list,
                negated,
            } => Ok(Expr::InList {
                expr: Box::new(self.resolve_subqueries(e, outer_row)?),
                list: list
                    .iter()
                    .map(|x| self.resolve_subqueries(x, outer_row))
                    .collect::<Result<_, _>>()?,
                negated: *negated,
            }),
            Expr::Between {
                expr: e,
                negated,
                low,
                high,
            } => Ok(Expr::Between {
                expr: Box::new(self.resolve_subqueries(e, outer_row)?),
                negated: *negated,
                low: Box::new(self.resolve_subqueries(low, outer_row)?),
                high: Box::new(self.resolve_subqueries(high, outer_row)?),
            }),
            Expr::Subquery(q) => {
                let q = self.correlate_subquery(q, outer_row);
                let out = self.exec_query(&q)?;
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
            Expr::InSubquery {
                expr: e,
                subquery,
                negated,
            } => {
                let q = self.correlate_subquery(subquery, outer_row);
                let out = self.exec_query(&q)?;
                let needle_expr = self.resolve_subqueries(e, outer_row)?;
                let empty = Row {
                    columns: vec![],
                    values: vec![],
                };
                let needle_val = eval_expr(&needle_expr, outer_row.unwrap_or(&empty));
                let found = out.rows.iter().any(|r| {
                    r.first()
                        .is_some_and(|s| parse_display_value(s) == needle_val)
                });
                let result = if *negated { !found } else { found };
                Ok(Expr::Value(SqlValue::Boolean(result).with_empty_span()))
            }
            Expr::Exists { subquery, negated } => {
                let q = self.correlate_subquery(subquery, outer_row);
                let out = self.exec_query(&q)?;
                let exists = !out.rows.is_empty();
                let result = if *negated { !exists } else { exists };
                Ok(Expr::Value(SqlValue::Boolean(result).with_empty_span()))
            }
            Expr::Nested(inner) => Ok(Expr::Nested(Box::new(
                self.resolve_subqueries(inner, outer_row)?,
            ))),
            Expr::Function(f) => {
                let new_args = match &f.args {
                    FunctionArguments::List(al) => {
                        let resolved: Result<Vec<_>, _> = al
                            .args
                            .iter()
                            .map(|a| match a {
                                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                                    Ok::<FunctionArg, MqdbError>(FunctionArg::Unnamed(
                                        FunctionArgExpr::Expr(
                                            self.resolve_subqueries(e, outer_row)?,
                                        ),
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

    /// Binds a nested query's free references to the outer row it's
    /// correlated against (a no-op clone when there is no outer row, i.e.
    /// this subquery sits somewhere that isn't evaluated per-row).
    fn correlate_subquery(&self, query: &Query, outer_row: Option<&Row>) -> Query {
        match outer_row {
            Some(row) => substitute_outer_refs_query(query, row),
            None => query.clone(),
        }
    }
}
