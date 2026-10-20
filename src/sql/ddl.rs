//! DDL, custom-table `INSERT` and metadata statements.

use super::engine::SqlEngine;
use super::output::QueryOutput;
use super::rows::{ident_value, ok_result, require_unqualified};
use crate::MqdbError;
use crate::store::CustomTableState;
use sqlparser::ast::{
    ColumnDef, ColumnOption, CreateTable, CreateView, Expr, IndexColumn, Insert, ObjectName,
    TableConstraint, TableObject,
};

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

impl<'a> SqlEngine<'a> {
    /// `DETACH [DATABASE] <alias>`: not parsed generically by `sqlparser`
    /// under `GenericDialect`, so handled here like `DESC`/`SHOW TABLES`.
    pub(super) fn exec_detach(&self, trimmed: &str) -> Result<QueryOutput, MqdbError> {
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

    pub(super) fn exec_desc(&self, table_name: &str) -> Result<QueryOutput, MqdbError> {
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
            "links" => Some(vec![
                ("document_id", "integer"),
                ("block_id", "integer"),
                ("kind", "text"),
                ("url", "text"),
                ("text", "text"),
                ("title", "text"),
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
                ("properties", "text"),
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

    pub(super) fn exec_show_tables(&self) -> Result<QueryOutput, MqdbError> {
        let mut rows = vec![
            vec!["blocks".to_string(), "built-in".to_string()],
            vec!["documents".to_string(), "built-in".to_string()],
            vec!["links".to_string(), "built-in".to_string()],
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

    pub(super) fn exec_create_table(&self, ct: &CreateTable) -> Result<QueryOutput, MqdbError> {
        let table_name = require_unqualified(&ct.name)?;
        if matches!(table_name.as_str(), "blocks" | "documents" | "links") {
            return Err(MqdbError::SqlExec(format!(
                "cannot override built-in table '{table_name}'"
            )));
        }
        // Guard the cross-kind check too, or a concurrent CREATE VIEW can slip in.
        let _catalog_guard = self.store.catalog_commit.lock().unwrap();
        if self.store.views.read().unwrap().contains_key(&table_name) {
            return Err(MqdbError::SqlExec(format!(
                "'{table_name}' is already defined as a view"
            )));
        }

        if let Some(query) = &ct.query {
            // CREATE TABLE name AS SELECT ...
            let already_exists = self
                .store
                .custom_tables
                .read()
                .unwrap()
                .contains_key(&table_name);
            if already_exists && !ct.or_replace {
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
            let result = self.exec_query(query)?;
            let n = result.rows.len();
            let previous = self.store.custom_tables.write().unwrap().insert(
                table_name.clone(),
                CustomTableState {
                    columns: result.columns,
                    rows: result.rows,
                    first_row_page: 0,
                    last_row_page: 0,
                    not_null: vec![],
                    unique: vec![],
                },
            );
            if let Err(err) = self.store.flush_catalog_to_storage() {
                let mut tables = self.store.custom_tables.write().unwrap();
                match previous {
                    Some(previous_state) => {
                        tables.insert(table_name, previous_state);
                    }
                    None => {
                        tables.remove(&table_name);
                    }
                }
                return Err(err);
            }
            self.store.note_catalog_mutation(self.session_id);
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
            table_name.clone(),
            CustomTableState {
                columns,
                rows: vec![],
                first_row_page: 0,
                last_row_page: 0,
                not_null,
                unique,
            },
        );
        if let Err(err) = self.store.flush_catalog_to_storage() {
            self.store
                .custom_tables
                .write()
                .unwrap()
                .remove(&table_name);
            return Err(err);
        }
        self.store.note_catalog_mutation(self.session_id);
        Ok(QueryOutput {
            columns: vec!["result".to_string()],
            rows: vec![vec!["ok".to_string()]],
        })
    }

    pub(super) fn exec_create_view(&self, cv: &CreateView) -> Result<QueryOutput, MqdbError> {
        let view_name = require_unqualified(&cv.name)?;
        if matches!(view_name.as_str(), "blocks" | "documents" | "links") {
            return Err(MqdbError::SqlExec(format!(
                "cannot override built-in table '{view_name}'"
            )));
        }
        if !cv.columns.is_empty() {
            return Err(MqdbError::SqlExec(
                "explicit view columns (CREATE VIEW v (a, b) AS ...) are not supported".into(),
            ));
        }

        // Guard the cross-kind check too, or a concurrent CREATE TABLE can slip in.
        let _catalog_guard = self.store.catalog_commit.lock().unwrap();
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

        let previous = self
            .store
            .views
            .write()
            .unwrap()
            .insert(view_name.clone(), sql_text);
        if let Err(err) = self.store.flush_catalog_to_storage() {
            let mut views = self.store.views.write().unwrap();
            match previous {
                Some(previous_sql) => {
                    views.insert(view_name, previous_sql);
                }
                None => {
                    views.remove(&view_name);
                }
            }
            return Err(err);
        }
        self.store.note_catalog_mutation(self.session_id);
        Ok(QueryOutput {
            columns: vec!["result".to_string()],
            rows: vec![vec!["ok".to_string()]],
        })
    }

    pub(super) fn exec_drop_views(
        &self,
        names: &[ObjectName],
        if_exists: bool,
    ) -> Result<QueryOutput, MqdbError> {
        let mut view_names = Vec::with_capacity(names.len());
        for name in names {
            let view_name = require_unqualified(name)?;
            if matches!(view_name.as_str(), "blocks" | "documents" | "links") {
                return Err(MqdbError::SqlExec(format!(
                    "cannot drop built-in table '{view_name}'"
                )));
            }
            view_names.push(view_name);
        }

        // Serialize mutate+flush+rollback against other catalog writers.
        let _catalog_guard = self.store.catalog_commit.lock().unwrap();

        let mut removed = Vec::new();
        let dropped = {
            let mut guard = self.store.views.write().unwrap();
            // Validate every target before mutating so a missing name later
            // in the list can't leave earlier names partially dropped.
            if !if_exists {
                for view_name in &view_names {
                    if !guard.contains_key(view_name) {
                        return Err(MqdbError::SqlExec(format!(
                            "view '{view_name}' does not exist"
                        )));
                    }
                }
            }
            let mut dropped = 0usize;
            for view_name in view_names {
                if let Some(sql) = guard.remove(&view_name) {
                    removed.push((view_name, sql));
                    dropped += 1;
                }
            }
            dropped
        };
        if let Err(err) = self.store.flush_catalog_to_storage() {
            let mut guard = self.store.views.write().unwrap();
            for (name, sql) in removed {
                guard.insert(name, sql);
            }
            return Err(err);
        }
        if dropped > 0 {
            self.store.note_catalog_mutation(self.session_id);
        }
        Ok(QueryOutput {
            columns: vec!["result".to_string()],
            rows: vec![vec![format!("{dropped} view(s) dropped")]],
        })
    }

    pub(super) fn exec_insert(&self, ins: &Insert) -> Result<QueryOutput, MqdbError> {
        let table_name = match &ins.table {
            TableObject::TableName(name) => require_unqualified(name)?,
            _ => return Err(MqdbError::SqlExec("unsupported INSERT target".into())),
        };

        let source = ins
            .source
            .as_ref()
            .ok_or_else(|| MqdbError::SqlExec("INSERT requires VALUES or SELECT".into()))?;
        let values_out = self.exec_query(source)?;

        // Serialize mutate+flush+rollback against other catalog writers.
        let _catalog_guard = self.store.catalog_commit.lock().unwrap();

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
        if let Err(err) = self
            .store
            .append_table_rows_to_storage(&table_name, &new_rows)
        {
            self.store.rollback_appended_rows(&table_name, inserted);
            return Err(err);
        }
        if inserted > 0 {
            self.store.note_catalog_mutation(self.session_id);
        }
        Ok(QueryOutput {
            columns: vec!["rows_affected".to_string()],
            rows: vec![vec![inserted.to_string()]],
        })
    }

    pub(super) fn exec_drop_tables(
        &self,
        names: &[ObjectName],
        if_exists: bool,
    ) -> Result<QueryOutput, MqdbError> {
        let mut table_names = Vec::with_capacity(names.len());
        for name in names {
            let table_name = require_unqualified(name)?;
            if matches!(table_name.as_str(), "blocks" | "documents" | "links") {
                return Err(MqdbError::SqlExec(format!(
                    "cannot drop built-in table '{table_name}'"
                )));
            }
            table_names.push(table_name);
        }

        // Serialize mutate+flush+rollback against other catalog writers.
        let _catalog_guard = self.store.catalog_commit.lock().unwrap();

        let mut removed = Vec::new();
        let dropped = {
            let mut guard = self.store.custom_tables.write().unwrap();
            // Validate every target before mutating so a missing name later
            // in the list can't leave earlier names partially dropped.
            if !if_exists {
                for table_name in &table_names {
                    if !guard.contains_key(table_name) {
                        return Err(MqdbError::SqlExec(format!(
                            "table '{table_name}' does not exist"
                        )));
                    }
                }
            }
            let mut dropped = 0usize;
            for table_name in table_names {
                if let Some(state) = guard.remove(&table_name) {
                    removed.push((table_name, state));
                    dropped += 1;
                }
            }
            dropped
        }; // write lock released before flush
        if let Err(err) = self.store.flush_catalog_to_storage() {
            let mut guard = self.store.custom_tables.write().unwrap();
            for (name, state) in removed {
                guard.insert(name, state);
            }
            return Err(err);
        }
        if dropped > 0 {
            self.store.note_catalog_mutation(self.session_id);
        }
        Ok(QueryOutput {
            columns: vec!["result".to_string()],
            rows: vec![vec![format!("{dropped} table(s) dropped")]],
        })
    }
}
