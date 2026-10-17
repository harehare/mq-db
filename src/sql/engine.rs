//! `SqlEngine` and statement dispatch.

use super::fts::{BM25_CACHE, QUERY_HAS_BM25, QUERY_TOKENIZER};
use super::output::QueryOutput;
use super::plan::expr_str_val;
use super::rows::ok_result;
use crate::document::Document;
use crate::indexes::DocumentIndex;
use crate::store::DatabaseAlias;
use crate::{DocumentStore, MqdbError};
use rustc_hash::FxHashMap;
use sqlparser::ast::{ObjectType, Statement};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

/// Custom SQL execution engine backed by a [`DocumentStore`] reference.
///
/// Secondary indexes are built once on construction (O(n) in total block count)
/// and reused for every query. Commands that do not create a `SqlEngine`
/// (mq, list, show, stats …) pay no index-construction cost.
pub struct SqlEngine<'a> {
    pub(super) store: &'a DocumentStore,
    /// One `DocumentIndex` per document, in the same order as `store.documents()`.
    pub(super) indexes: Vec<DocumentIndex>,
    /// Stack of CTE scopes from `WITH` clauses, one frame per nested
    /// `exec_query` call. Looked up innermost-first so a nested subquery's
    /// own `WITH` shadows an outer CTE of the same name.
    pub(super) cte_scopes: std::cell::RefCell<Vec<FxHashMap<String, std::rc::Rc<QueryOutput>>>>,
    pub(super) view_stack: std::cell::RefCell<Vec<String>>,
    /// Identifies this engine as a transaction owner; `ThreadId` can't, since
    /// engines may share a thread or a session may move between threads.
    pub(super) session_id: u64,
}

impl<'a> SqlEngine<'a> {
    /// Build the engine and its secondary indexes.
    ///
    /// Uses cached indexes from [`DocumentStore::load_all_indexes`] when
    /// available (O(1) per document); otherwise rebuilds from blocks (O(n)).
    pub fn new(store: &'a DocumentStore) -> Result<Self, MqdbError> {
        Self::with_session(store, DocumentStore::new_session_id())
    }

    /// Like [`new`](Self::new), but runs as an existing session (see
    /// [`DocumentStore::new_session_id`]), so a transaction opened by one
    /// engine can be continued or rolled back by a later one.
    pub fn with_session(store: &'a DocumentStore, session_id: u64) -> Result<Self, MqdbError> {
        let indexes = store
            .documents()
            .iter()
            .enumerate()
            .map(|(i, doc)| {
                if let Some(idx) = store.get_doc_index(i) {
                    idx.clone()
                } else {
                    DocumentIndex::build(&doc.blocks, store.tokenizer())
                }
            })
            .collect();
        Ok(Self {
            store,
            indexes,
            cte_scopes: std::cell::RefCell::new(Vec::new()),
            view_stack: std::cell::RefCell::new(Vec::new()),
            session_id,
        })
    }

    pub(super) fn documents_with_indexes(
        &self,
    ) -> impl Iterator<Item = (&Document, &DocumentIndex)> {
        self.store.documents().iter().zip(self.indexes.iter())
    }

    /// Execute a SQL statement against the store.
    ///
    /// Supports `SELECT`, `CREATE TABLE`, `INSERT INTO`, `DROP TABLE`,
    /// `DESC`/`DESCRIBE`, and `SHOW TABLES`.
    pub fn execute(&self, sql: &str) -> Result<QueryOutput, MqdbError> {
        QUERY_TOKENIZER.with(|c| c.set(self.store.tokenizer()));

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

        // Cheap text scan gating whether `exec_select` bothers resolving any
        // `Bm25Corpus` at all; the corpora themselves are built lazily,
        // per referenced store, in `exec_select` (see `BM25_CACHE`).
        struct Bm25QueryScope(bool);
        impl Drop for Bm25QueryScope {
            fn drop(&mut self) {
                QUERY_HAS_BM25.with(|c| c.set(self.0));
                BM25_CACHE.with(|c| c.borrow_mut().clear());
            }
        }
        let prev_has_bm25 = QUERY_HAS_BM25.with(std::cell::Cell::get);
        QUERY_HAS_BM25.with(|c| c.set(trimmed.to_ascii_lowercase().contains("bm25(")));
        let _bm25_guard = Bm25QueryScope(prev_has_bm25);

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
                self.store.begin_transaction(self.session_id)?;
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
}
