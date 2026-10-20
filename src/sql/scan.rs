//! Table and view row materialisation with index hints.

use super::engine::SqlEngine;
use super::expr::eval_expr;
use super::external::resolve_table_function;
use super::join::{combine_rows, cross_join, hash_equi_join, null_extend_row, output_to_rows};
use super::output::QueryOutput;
use super::plan::{find_equi_join_exprs, zone_map_skip};
use super::rewrite::fold_constants;
use super::rows::{block_to_row, doc_to_row, ident_value, link_rows, qualify_row};
use super::value::{Row, Value};
use crate::MqdbError;
use crate::indexes::IndexHint;
use sqlparser::ast::{Expr, JoinConstraint, JoinOperator, Statement, TableFactor};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

impl<'a> SqlEngine<'a> {
    pub(super) fn materialise_from_with_hint(
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
            let outer = matches!(
                &join.join_operator,
                JoinOperator::Left(_) | JoinOperator::LeftOuter(_)
            );
            match &join.join_operator {
                JoinOperator::Inner(JoinConstraint::On(on))
                | JoinOperator::Join(JoinConstraint::On(on))
                | JoinOperator::Left(JoinConstraint::On(on))
                | JoinOperator::LeftOuter(JoinConstraint::On(on)) => {
                    let resolved = fold_constants(&self.resolve_subqueries(on, None)?);
                    let left_cols = rows.first().map(|r| r.columns.clone()).unwrap_or_default();
                    let right_cols = right.first().map(|r| r.columns.clone()).unwrap_or_default();
                    rows = match find_equi_join_exprs(&resolved, &left_cols, &right_cols) {
                        Some((left_key, right_key)) => hash_equi_join(
                            rows,
                            right,
                            left_key,
                            right_key,
                            &resolved,
                            outer,
                            &right_cols,
                        ),
                        None => {
                            let mut combined = Vec::new();
                            for l in &rows {
                                let mut matched = false;
                                for r in &right {
                                    let row = combine_rows(l, r);
                                    if eval_expr(&resolved, &row).is_truthy() {
                                        matched = true;
                                        combined.push(row);
                                    }
                                }
                                if outer && !matched {
                                    combined.push(null_extend_row(l, &right_cols));
                                }
                            }
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

        // `<alias>.<table>`: resolve against an ATTACHed store instead of
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
    /// schema qualifier or CTE shadowing, shared by local and
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
            "links" => {
                let prefix = alias.unwrap_or("links");
                let mut rows = Vec::new();
                let mut global_idx: u32 = 0;
                for doc in self.store.documents() {
                    for (i, block) in doc.blocks.iter().enumerate() {
                        for row in link_rows(doc.id, block, global_idx + i as u32) {
                            rows.push(qualify_row(row, prefix));
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

    pub(super) fn exec_view_query(
        &self,
        name: &str,
        sql_text: &str,
    ) -> Result<QueryOutput, MqdbError> {
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
}
