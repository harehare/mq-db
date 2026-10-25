//! `UPDATE` and `DELETE` on custom tables.

use super::engine::SqlEngine;
use super::expr::eval_expr;
use super::output::QueryOutput;
use super::rows::{ident_value, qualify_row, require_unqualified};
use super::value::{Row, Value};
use crate::MqdbError;
use crate::store::CustomTableState;
use sqlparser::ast::{
    Assignment, AssignmentTarget, Delete, Expr, FromTable, TableFactor, TableWithJoins, Update,
};

const BUILT_IN: [&str; 3] = ["blocks", "documents", "links"];

/// The table an `UPDATE`/`DELETE` names, and the prefix its columns carry in
/// expressions (the alias if there is one).
fn target(twj: &TableWithJoins) -> Result<(String, String), MqdbError> {
    if !twj.joins.is_empty() {
        return Err(MqdbError::SqlExec(
            "UPDATE/DELETE does not support JOIN".into(),
        ));
    }
    let TableFactor::Table { name, alias, .. } = &twj.relation else {
        return Err(MqdbError::SqlExec(
            "unsupported UPDATE/DELETE target".into(),
        ));
    };
    let table = require_unqualified(name)?;
    if BUILT_IN.contains(&table.as_str()) {
        return Err(MqdbError::SqlExec(format!(
            "cannot modify built-in table '{table}' with plain SQL; \
             UPDATE/DELETE on blocks needs write-back mode"
        )));
    }
    let prefix = alias
        .as_ref()
        .map_or_else(|| table.clone(), |a| a.name.value.clone());
    Ok((table, prefix))
}

fn row_of(columns: &[String], cells: &[String], prefix: &str) -> Row {
    qualify_row(
        Row {
            columns: columns.to_vec(),
            values: cells.iter().map(|c| Value::Str(c.clone())).collect(),
        },
        prefix,
    )
}

impl SqlEngine<'_> {
    pub(super) fn exec_update(&self, update: &Update) -> Result<QueryOutput, MqdbError> {
        if update.from.is_some()
            || update.returning.is_some()
            || !update.order_by.is_empty()
            || update.limit.is_some()
        {
            return Err(MqdbError::SqlExec(
                "UPDATE supports only SET and WHERE".into(),
            ));
        }
        let (table, prefix) = target(&update.table)?;
        let columns = self.table_columns(&table)?;
        let assignments = resolve_assignments(&table, &columns, &update.assignments)?;
        let n = self.rewrite_rows(&table, &prefix, update.selection.as_ref(), |row, cells| {
            for (idx, expr) in &assignments {
                cells[*idx] = eval_expr(expr, row).display();
            }
            true
        })?;
        Ok(count_result("updated", n))
    }

    pub(super) fn exec_delete(&self, delete: &Delete) -> Result<QueryOutput, MqdbError> {
        if !delete.tables.is_empty()
            || delete.using.is_some()
            || delete.returning.is_some()
            || !delete.order_by.is_empty()
            || delete.limit.is_some()
        {
            return Err(MqdbError::SqlExec(
                "DELETE supports only FROM and WHERE".into(),
            ));
        }
        let (FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables)) = &delete.from;
        let [twj] = tables.as_slice() else {
            return Err(MqdbError::SqlExec(
                "DELETE supports exactly one target table".into(),
            ));
        };
        let (table, prefix) = target(twj)?;
        self.table_columns(&table)?;
        let n = self.rewrite_rows(&table, &prefix, delete.selection.as_ref(), |_, _| false)?;
        Ok(count_result("deleted", n))
    }

    fn table_columns(&self, table: &str) -> Result<Vec<String>, MqdbError> {
        self.store
            .custom_tables
            .read()
            .unwrap()
            .get(table)
            .map(|s| s.columns.clone())
            .ok_or_else(|| MqdbError::SqlExec(format!("unknown table: {table}")))
    }

    /// Applies `edit` to every row matching `selection` and persists the
    /// result; returns how many rows matched. `edit` mutates the row's cells
    /// and returns whether to keep it. Predicates and new values are all
    /// evaluated against the table as it was before the statement.
    fn rewrite_rows(
        &self,
        table: &str,
        prefix: &str,
        selection: Option<&Expr>,
        edit: impl Fn(&Row, &mut Vec<String>) -> bool,
    ) -> Result<usize, MqdbError> {
        // Serialize mutate+flush+rollback against other catalog writers.
        let _catalog_guard = self.store.catalog_commit.lock().unwrap();

        let (columns, old_rows) = {
            let guard = self.store.custom_tables.read().unwrap();
            let state = guard
                .get(table)
                .ok_or_else(|| MqdbError::SqlExec(format!("unknown table: {table}")))?;
            (state.columns.clone(), state.rows.clone())
        };

        let mut matched = 0;
        let mut new_rows = Vec::with_capacity(old_rows.len());
        for cells in &old_rows {
            let row = row_of(&columns, cells, prefix);
            let hit = match selection {
                None => true,
                Some(expr) => {
                    let resolved = self.resolve_subqueries(expr, Some(&row))?;
                    eval_expr(&resolved, &row).is_truthy()
                }
            };
            if !hit {
                new_rows.push(cells.clone());
                continue;
            }
            matched += 1;
            let mut updated = cells.clone();
            if edit(&row, &mut updated) {
                new_rows.push(updated);
            }
        }
        if matched == 0 {
            return Ok(0);
        }

        let previous = {
            let mut guard = self.store.custom_tables.write().unwrap();
            let state = guard
                .get_mut(table)
                .ok_or_else(|| MqdbError::SqlExec(format!("unknown table: {table}")))?;
            check_constraints(table, state, &new_rows)?;
            let previous = (
                state.rows.clone(),
                state.first_row_page,
                state.last_row_page,
            );
            state.rows = new_rows;
            previous
        }; // write lock released before flush
        if let Err(err) = self.store.append_table_rows_to_storage(table, &[]) {
            let mut guard = self.store.custom_tables.write().unwrap();
            if let Some(state) = guard.get_mut(table) {
                (state.rows, state.first_row_page, state.last_row_page) = previous;
            }
            return Err(err);
        }
        self.store.note_catalog_mutation(self.session_id);
        Ok(matched)
    }
}

fn resolve_assignments(
    table: &str,
    columns: &[String],
    assignments: &[Assignment],
) -> Result<Vec<(usize, Expr)>, MqdbError> {
    assignments
        .iter()
        .map(|a| {
            let AssignmentTarget::ColumnName(name) = &a.target else {
                return Err(MqdbError::SqlExec(
                    "UPDATE does not support tuple assignment targets".into(),
                ));
            };
            let col = name.0.last().map(ident_value).unwrap_or("");
            let idx = columns
                .iter()
                .position(|c| c.eq_ignore_ascii_case(col))
                .ok_or_else(|| MqdbError::SqlExec(format!("unknown column '{col}' in {table}")))?;
            Ok((idx, a.value.clone()))
        })
        .collect()
}

/// Mirrors the checks `INSERT` applies, over the whole resulting table.
fn check_constraints(
    table: &str,
    state: &CustomTableState,
    rows: &[Vec<String>],
) -> Result<(), MqdbError> {
    for row in rows {
        for &idx in &state.not_null {
            if row.get(idx).is_none_or(|v| v.is_empty() || v == "NULL") {
                return Err(MqdbError::SqlExec(format!(
                    "NOT NULL constraint failed: {}.{}",
                    table, state.columns[idx]
                )));
            }
        }
    }
    for group in &state.unique {
        let mut seen = std::collections::HashSet::new();
        for row in rows {
            let key: Vec<&String> = group.iter().map(|&i| &row[i]).collect();
            if !seen.insert(key) {
                let cols: Vec<&str> = group.iter().map(|&i| state.columns[i].as_str()).collect();
                return Err(MqdbError::SqlExec(format!(
                    "UNIQUE constraint failed: {}.{}",
                    table,
                    cols.join(", ")
                )));
            }
        }
    }
    Ok(())
}

fn count_result(label: &str, n: usize) -> QueryOutput {
    QueryOutput {
        columns: vec![label.to_string()],
        rows: vec![vec![n.to_string()]],
    }
}
