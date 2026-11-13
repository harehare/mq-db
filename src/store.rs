use std::{
    collections::HashSet,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Mutex, RwLock},
};

use rustc_hash::FxHashMap;

/// In-memory state for a user-defined table.
///
/// `first_row_page`/`last_row_page` track where this table's rows live in
/// the backing storage file (0 = not persisted yet), so a SQL `INSERT`
/// can append just the new rows to the chain instead of rewriting `rows`
/// in full on every call. See [`Storage::write_table_rows`].
#[derive(Clone)]
pub(crate) struct CustomTableState {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub first_row_page: u32,
    pub last_row_page: u32,
    pub not_null: Vec<usize>,
    pub unique: Vec<Vec<usize>>,
}

pub(crate) struct TxSnapshot {
    pub documents: Vec<Document>,
    pub custom_tables: FxHashMap<String, CustomTableState>,
    pub views: FxHashMap<String, String>,
    pub vector_indexes: FxHashMap<String, VectorIndexState>,
    pub content_hashes: FxHashMap<DocumentId, u64>,
    /// Session (`SqlEngine`) that ran `BEGIN`. Catalog mutations from any
    /// other session while this transaction is open are external commits.
    pub owner: u64,
    /// Set by [`DocumentStore::note_catalog_mutation`] when a session other
    /// than `owner` mutates the catalog while this transaction is open.
    /// Rollback refuses to run over such a mutation rather than silently
    /// discarding it.
    pub external_write: bool,
}

use mq_markdown::Markdown;

mod ingest;
mod persist;
mod transaction;
mod vector_indexes;
pub(crate) use vector_indexes::VectorIndexState;

/// A validated alias for a store mounted via `ATTACH DATABASE ... AS
/// <alias>` (or the CLI's `--attach PATH:ALIAS`).
///
/// Normalizes to lowercase, since aliases are matched case-insensitively
/// like unquoted SQL identifiers, and rejects names that would collide with
/// the local store's own namespace (`main`, `blocks`, `documents`).
/// [`DatabaseAlias::parse`] is the only constructor, so every attach path
/// gets the same validation instead of each call site re-implementing it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DatabaseAlias(String);

impl DatabaseAlias {
    /// Names reserved for the local store's own namespace; an attached
    /// database may not use one of these as its alias.
    const RESERVED: [&'static str; 4] = ["main", "blocks", "documents", "links"];

    /// Validates and normalizes a raw alias string.
    pub fn parse(raw: &str) -> Result<Self, MqdbError> {
        if raw.is_empty() {
            return Err(MqdbError::SqlExec("database alias cannot be empty".into()));
        }
        let lower = raw.to_ascii_lowercase();
        if Self::RESERVED.contains(&lower.as_str()) {
            return Err(MqdbError::SqlExec(format!(
                "'{raw}' is a reserved name and cannot be used as a database alias"
            )));
        }
        Ok(Self(lower))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DatabaseAlias {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::borrow::Borrow<str> for DatabaseAlias {
    fn borrow(&self) -> &str {
        &self.0
    }
}

use crate::{
    block::{BlockType, DocumentId},
    document::Document,
    error::MqdbError,
    index,
    indexes::{DocumentIndex, TokenizerKind},
    storage::{
        Storage,
        catalog::{CatalogEntry, CustomTableEntry, ViewEntry},
        codec::{decode_zone_map, encode_zone_map},
        page::{FILE_VERSION, PAGE_SIZE},
    },
};

/// Persists any table whose rows have never been written to `storage` (i.e.
/// `first_row_page == 0`), then builds catalog entries for every table.
///
/// Tables that already have a row-page chain are left untouched here; their
/// pages were already written by an earlier flush or incremental `INSERT`
/// append (see [`DocumentStore::append_table_rows_to_storage`]).
fn persist_unsaved_table_rows(
    storage: &mut Storage,
    custom_tables: &RwLock<FxHashMap<String, CustomTableState>>,
) -> Result<Vec<CustomTableEntry>, MqdbError> {
    let mut guard = custom_tables.write().unwrap();
    for state in guard.values_mut() {
        if state.first_row_page == 0 && !state.rows.is_empty() {
            let (first, last) = storage.write_table_rows(&state.rows)?;
            state.first_row_page = first;
            state.last_row_page = last;
        }
    }
    Ok(guard
        .iter()
        .map(|(name, state)| CustomTableEntry {
            name: name.clone(),
            columns: state.columns.clone(),
            first_row_page: state.first_row_page,
            last_row_page: state.last_row_page,
            num_rows: state.rows.len() as u32,
        })
        .collect())
}

/// Summary of a [`DocumentStore::reindex_paths`] run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReindexReport {
    /// Paths that were newly indexed.
    pub added: Vec<PathBuf>,
    /// Paths whose content changed and were re-parsed in place.
    pub updated: Vec<PathBuf>,
    /// Count of paths whose content hash matched the catalog (skipped).
    pub unchanged: usize,
    /// Paths that were dropped from the store because `prune` was set and
    /// they were no longer present in the reindexed file list.
    pub removed: Vec<PathBuf>,
    /// Paths that could not be read or parsed, with the error message.
    /// Reindexing continues with the remaining paths.
    pub failed: Vec<(PathBuf, String)>,
}

/// Aggregate statistics over a store's documents/blocks (see
/// [`DocumentStore::stats`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoreStats {
    pub documents: usize,
    pub blocks: usize,
    /// `(block_type, count)`, most frequent first.
    pub block_type_counts: Vec<(BlockType, usize)>,
    /// `(language, count)`, most frequent first, code blocks only.
    pub code_lang_counts: Vec<(String, usize)>,
}

/// Result of [`DocumentStore::vacuum`]: page counts before/after compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VacuumReport {
    pub pages_before: u32,
    pub pages_after: u32,
}

impl VacuumReport {
    pub fn bytes_reclaimed(&self) -> u64 {
        u64::from(self.pages_before.saturating_sub(self.pages_after)) * PAGE_SIZE as u64
    }
}

/// The top-level embedded document store.
///
/// Holds a collection of parsed Markdown documents and provides access to
/// the query interface. Documents are stored in memory with their flattened
/// block lists and interval indexes.
///
/// ## Load modes
///
/// - [`DocumentStore::new`] / [`DocumentStore::add_str`]: in-memory, blocks immediately available
/// - [`DocumentStore::load`]: reads all blocks from a `.mq-db` file into memory
/// - [`DocumentStore::open`]: reads catalog only; blocks loaded on demand via
///   [`load_all_blocks`](DocumentStore::load_all_blocks)
///
/// Secondary indexes ([`DocumentIndex`]) are built once via
/// [`load_all_indexes`](DocumentStore::load_all_indexes) and cached, so
/// subsequent [`crate::SqlEngine`] construction is O(1).
///
/// # Example
///
/// ```rust
/// use mq_db::DocumentStore;
///
/// let mut store = DocumentStore::new();
/// store.add_str("# Hello\n\nWorld").unwrap();
///
/// assert_eq!(store.len(), 1);
/// ```
pub struct DocumentStore {
    documents: Vec<Document>,
    next_doc_id: DocumentId,
    /// When `false`, source line/column spans are discarded after parsing.
    store_spans: bool,
    /// Full-text tokenizer strategy, fixed at creation (see `set_tokenizer`).
    tokenizer: TokenizerKind,
    /// Open storage file kept for lazy block / index loading. `None` when the
    /// store was built entirely in memory or fully loaded via `load()`.
    /// Wrapped in `Mutex` so DDL operations (which hold only `&DocumentStore`)
    /// can flush the updated catalog to disk.
    pub(crate) storage: Mutex<Option<Storage>>,
    /// Per-document secondary index cache (same order as `documents`).
    /// `None` means the index has not been built/loaded for that document yet.
    pub(crate) doc_indexes: Vec<Option<DocumentIndex>>,
    /// User-registered virtual tables: name → (columns, rows).
    /// Uses `RwLock` for interior mutability so `SqlEngine` can execute DDL
    /// (`CREATE TABLE`, `INSERT INTO`, `DROP TABLE`) with only `&DocumentStore`.
    pub(crate) custom_tables: RwLock<FxHashMap<String, CustomTableState>>,
    /// `CREATE VIEW` definitions: name → `SELECT` SQL text, re-executed live
    /// on every reference rather than materialized.
    pub(crate) views: RwLock<FxHashMap<String, String>>,
    /// Vector (HNSW) indexes over custom-table columns, keyed by index name.
    pub(crate) vector_indexes: RwLock<FxHashMap<String, VectorIndexState>>,
    /// Stores attached via `ATTACH DATABASE`, keyed by alias. Session-scoped
    /// only: never persisted.
    pub(crate) attached: RwLock<FxHashMap<DatabaseAlias, DocumentStore>>,
    /// Content hash of each document's source, keyed by `DocumentId`. Used by
    /// [`reindex_paths`](DocumentStore::reindex_paths) to skip re-parsing
    /// files whose content hasn't changed since the last index run. Absent
    /// entries (e.g. documents added via `add_str`, or loaded from an older
    /// `.mq-db` file predating this feature) are treated as "unknown, always
    /// reindex".
    content_hashes: FxHashMap<DocumentId, u64>,
    pub(crate) tx_snapshot: Mutex<Option<TxSnapshot>>,
    /// Serializes mutate-then-flush-or-rollback DDL/DML as one operation, so
    /// a failing flush can't roll back into a catalog another thread already
    /// committed durably.
    pub(crate) catalog_commit: Mutex<()>,
    /// Session identity for [`execute_sql_mut`](DocumentStore::execute_sql_mut);
    /// stable across calls so `BEGIN`/`ROLLBACK` see one owner.
    pub(crate) write_session: u64,
}

static NEXT_SESSION_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Default for DocumentStore {
    fn default() -> Self {
        Self {
            documents: Vec::new(),
            next_doc_id: 0,
            store_spans: true,
            tokenizer: TokenizerKind::Word,
            storage: Mutex::new(None),
            doc_indexes: Vec::new(),
            custom_tables: RwLock::new(FxHashMap::default()),
            views: RwLock::new(FxHashMap::default()),
            vector_indexes: RwLock::new(FxHashMap::default()),
            attached: RwLock::new(FxHashMap::default()),
            content_hashes: FxHashMap::default(),
            tx_snapshot: Mutex::new(None),
            catalog_commit: Mutex::new(()),
            write_session: DocumentStore::new_session_id(),
        }
    }
}

/// Hash of file/document content used for change detection (not
/// cryptographic: this only needs to detect "did this file change since
/// last index", not resist adversarial collisions).
/// The directory holding `path`; `.` for a bare file name, whose `parent()`
/// is the empty path rather than `None`.
fn containing_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn diff_written_paths(before: &[Document], after: &[Document]) -> Vec<String> {
    after
        .iter()
        .filter_map(|a| {
            let b = before.iter().find(|d| d.id == a.id)?;
            if b.blocks != a.blocks {
                a.path.as_ref().map(|p| p.to_string_lossy().to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Reads every file in `files`, in order, using a small worker-thread pool:
/// indexing many small files is I/O-latency bound, not CPU bound.
fn read_files_parallel(files: &[PathBuf]) -> Vec<Result<String, MqdbError>> {
    let worker_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(files.len().max(1));
    if worker_count <= 1 {
        return files
            .iter()
            .map(|p| std::fs::read_to_string(p).map_err(MqdbError::from))
            .collect();
    }

    let chunk_size = files.len().div_ceil(worker_count);
    std::thread::scope(|scope| {
        files
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|p| std::fs::read_to_string(p).map_err(MqdbError::from))
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|handle| handle.join().expect("file-read worker thread panicked"))
            .collect()
    })
}

impl DocumentStore {
    /// Creates an empty document store.
    /// Allocate a fresh transaction-owner ID. Pass it to
    /// [`SqlEngine::with_session`](crate::sql::SqlEngine::with_session) so
    /// every statement of one client's transaction shares an owner.
    pub fn new_session_id() -> u64 {
        NEXT_SESSION_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// When set to `false`, source line/column spans are stripped from every
    /// block added after this call. Reduces memory by ~21 bytes per block.
    pub fn set_store_spans(&mut self, val: bool) {
        self.store_spans = val;
    }

    /// This store's full-text tokenizer strategy (see [`TokenizerKind`]).
    pub fn tokenizer(&self) -> TokenizerKind {
        self.tokenizer
    }

    /// Sets the tokenizer. Errors if the store already has documents, since
    /// `save()` may reuse indexes cached under the old tokenizer otherwise.
    pub fn set_tokenizer(&mut self, kind: TokenizerKind) -> Result<(), MqdbError> {
        if !self.documents.is_empty() {
            return Err(MqdbError::Storage(
                "cannot change tokenizer on a store that already has documents".into(),
            ));
        }
        self.tokenizer = kind;
        Ok(())
    }

    /// Register a custom virtual table that can be queried via SQL.
    ///
    /// The table is queryable with `SELECT … FROM <name>`. All column values
    /// are treated as strings; cast them in SQL as needed.
    ///
    /// Calling this a second time with the same name replaces the previous table.
    pub fn register_table(
        &mut self,
        name: impl Into<String>,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    ) {
        let name = name.into();
        self.drop_vector_indexes_for_table(&name);
        self.custom_tables.write().unwrap().insert(
            name,
            CustomTableState {
                columns,
                rows,
                first_row_page: 0,
                last_row_page: 0,
                not_null: vec![],
                unique: vec![],
            },
        );
    }

    /// Remove a previously registered custom table. Returns `true` if it existed.
    pub fn unregister_table(&mut self, name: &str) -> bool {
        self.drop_vector_indexes_for_table(name);
        self.custom_tables.write().unwrap().remove(name).is_some()
    }

    /// Attach another `.mq-db` store under `alias`, queryable as
    /// `<alias>.<table>`. Session-scoped; not persisted.
    pub fn attach(&self, alias: DatabaseAlias, path: &Path) -> Result<(), MqdbError> {
        if self.attached.read().unwrap().contains_key(&alias) {
            return Err(MqdbError::SqlExec(format!(
                "database alias '{alias}' is already attached — DETACH it first"
            )));
        }
        let mut other = DocumentStore::load(path)?;
        other.load_all_indexes()?;
        self.attached.write().unwrap().insert(alias, other);
        Ok(())
    }

    /// Detach a previously `attach`ed store. Returns `true` if `alias` was
    /// attached, `false` otherwise.
    pub fn detach(&self, alias: &str) -> bool {
        self.attached
            .write()
            .unwrap()
            .remove(alias.to_ascii_lowercase().as_str())
            .is_some()
    }

    /// Returns a slice of all documents in the store.
    pub fn documents(&self) -> &[Document] {
        &self.documents
    }

    /// Looks up a document by its `DocumentId`.
    pub fn get_document(&self, id: DocumentId) -> Option<&Document> {
        self.documents.iter().find(|d| d.id == id)
    }

    /// Returns the number of documents in the store.
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Returns `true` if the store contains no documents.
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Aggregate block-type / code-language statistics across every
    /// document currently loaded in memory.
    pub fn stats(&self) -> StoreStats {
        let mut type_counts: FxHashMap<BlockType, usize> = FxHashMap::default();
        let mut lang_counts: FxHashMap<String, usize> = FxHashMap::default();
        let mut total_blocks = 0usize;

        for doc in &self.documents {
            total_blocks += doc.blocks.len();
            for block in &doc.blocks {
                *type_counts.entry(block.block_type.clone()).or_insert(0) += 1;
                if block.block_type == BlockType::Code
                    && let Some(lang) = block.code_lang()
                {
                    *lang_counts.entry(lang.to_string()).or_insert(0) += 1;
                }
            }
        }

        let mut block_type_counts: Vec<(BlockType, usize)> = type_counts.into_iter().collect();
        block_type_counts.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
        let mut code_lang_counts: Vec<(String, usize)> = lang_counts.into_iter().collect();
        code_lang_counts.sort_by_key(|(_, v)| std::cmp::Reverse(*v));

        StoreStats {
            documents: self.documents.len(),
            blocks: total_blocks,
            block_type_counts,
            code_lang_counts,
        }
    }

    // Lazy loading

    /// Load blocks for every document that has not yet been loaded.
    ///
    /// No-op when the store was built in memory or fully loaded via `load()`.
    pub fn load_all_blocks(&mut self) -> Result<(), MqdbError> {
        let mut guard = self.storage.lock().unwrap();
        let storage = match guard.as_mut() {
            Some(s) => s,
            None => return Ok(()),
        };
        for doc in &mut self.documents {
            if doc.blocks.is_empty() && doc.block_count > 0 {
                doc.blocks = storage.read_blocks(doc.first_block_page, doc.block_count)?;
            }
        }
        Ok(())
    }

    /// Build or load persisted secondary indexes for every document and cache them.
    ///
    /// Must be called after [`load_all_blocks`](DocumentStore::load_all_blocks).
    /// Subsequent [`crate::SqlEngine`] construction reuses the cache and pays no
    /// per-block index rebuild cost.
    pub fn load_all_indexes(&mut self) -> Result<(), MqdbError> {
        for i in 0..self.documents.len() {
            if self.doc_indexes[i].is_some() {
                continue;
            }

            let idx = self.build_or_load_index_at(i)?;
            self.doc_indexes[i] = Some(idx);
        }
        Ok(())
    }

    fn build_or_load_index_at(&mut self, i: usize) -> Result<DocumentIndex, MqdbError> {
        let index_start_page = self.documents[i].index_start_page;

        if index_start_page > 0 {
            let mut guard = self.storage.lock().unwrap();
            if let Some(storage) = guard.as_mut() {
                let bytes = storage.read_index_bytes(index_start_page)?;
                return DocumentIndex::from_bytes(&bytes);
            }
        }

        Ok(DocumentIndex::build(
            &self.documents[i].blocks,
            self.tokenizer,
        ))
    }

    /// Returns the cached `DocumentIndex` for the document at position `i`.
    pub(crate) fn get_doc_index(&self, i: usize) -> Option<&DocumentIndex> {
        self.doc_indexes.get(i).and_then(|o| o.as_ref())
    }

    /// Builds catalog entries for every in-memory document.
    fn catalog_entries(&self) -> Vec<CatalogEntry> {
        self.documents
            .iter()
            .map(|d| CatalogEntry {
                document_id: d.id,
                path: d.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                first_block_page: d.first_block_page,
                num_blocks: d.block_count,
                zone_map_bytes: encode_zone_map(&d.zone_maps),
                index_start_page: d.index_start_page,
            })
            .collect()
    }

    /// Builds the `(document_id, content_hash)` pairs to persist alongside the catalog.
    fn content_hash_pairs(&self) -> Vec<(u32, u64)> {
        self.content_hashes.iter().map(|(k, v)| (*k, *v)).collect()
    }

    /// Builds the `CREATE VIEW` entries to persist alongside the catalog.
    fn views_entries(&self) -> Vec<ViewEntry> {
        self.views
            .read()
            .unwrap()
            .iter()
            .map(|(name, sql)| ViewEntry {
                name: name.clone(),
                sql: sql.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod alias_tests {
    use super::*;

    #[test]
    fn parse_lowercases() {
        assert_eq!(DatabaseAlias::parse("Other").unwrap().as_str(), "other");
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(DatabaseAlias::parse("").is_err());
    }

    #[test]
    fn parse_rejects_reserved_names_case_insensitively() {
        for reserved in ["main", "BLOCKS", "Documents"] {
            let err = DatabaseAlias::parse(reserved).unwrap_err();
            assert!(err.to_string().contains("reserved"));
        }
    }
}

#[cfg(test)]
mod reindex_tests {
    use super::*;

    fn write_md(dir: &tempfile::TempDir, name: &str, content: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn reindex_in_memory_store_adds_new_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_md(&dir, "a.md", "# A\n\nHello\n");
        let b = write_md(&dir, "b.md", "# B\n\nWorld\n");

        let mut store = DocumentStore::new();
        let report = store.reindex_paths(&[a.clone(), b.clone()], false).unwrap();

        assert_eq!(report.added, vec![a, b]);
        assert!(report.updated.is_empty());
        assert_eq!(report.unchanged, 0);
        assert!(report.removed.is_empty());
        assert!(report.failed.is_empty());
        assert_eq!(store.documents().len(), 2);
    }

    #[test]
    fn reindex_skips_unchanged_file_on_second_run() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_md(&dir, "a.md", "# A\n\nHello\n");

        let mut store = DocumentStore::new();
        store
            .reindex_paths(std::slice::from_ref(&a), false)
            .unwrap();
        let doc_id_before = store.documents()[0].id;

        let report = store
            .reindex_paths(std::slice::from_ref(&a), false)
            .unwrap();

        assert!(report.added.is_empty());
        assert!(report.updated.is_empty());
        assert_eq!(report.unchanged, 1);
        assert_eq!(store.documents()[0].id, doc_id_before);
    }

    #[test]
    fn reindex_replaces_changed_file_keeping_document_id() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_md(&dir, "a.md", "# A\n\nHello\n");

        let mut store = DocumentStore::new();
        store
            .reindex_paths(std::slice::from_ref(&a), false)
            .unwrap();
        let doc_id_before = store.documents()[0].id;

        std::fs::write(&a, "# A Changed\n\nNew body\n").unwrap();
        let report = store
            .reindex_paths(std::slice::from_ref(&a), false)
            .unwrap();

        assert!(report.added.is_empty());
        assert_eq!(report.updated, vec![a]);
        assert_eq!(report.unchanged, 0);
        assert_eq!(store.documents()[0].id, doc_id_before);
        assert!(
            store.documents()[0]
                .blocks
                .iter()
                .any(|b| b.content == "A Changed")
        );
    }

    #[test]
    fn reindex_prune_removes_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_md(&dir, "a.md", "# A\n");
        let b = write_md(&dir, "b.md", "# B\n");

        let mut store = DocumentStore::new();
        store.reindex_paths(&[a.clone(), b.clone()], false).unwrap();
        assert_eq!(store.documents().len(), 2);

        let report = store.reindex_paths(std::slice::from_ref(&a), true).unwrap();

        assert_eq!(report.removed, vec![b]);
        assert_eq!(report.unchanged, 1);
        assert_eq!(store.documents().len(), 1);
        assert_eq!(store.documents()[0].path.as_deref(), Some(a.as_path()));
    }

    #[test]
    fn reindex_on_backing_store_persists_hash_across_reload() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_md(&dir, "a.md", "# A\n\nHello\n");
        let db_path = dir.path().join("store.mq-db");

        let mut store = DocumentStore::new();
        store
            .reindex_paths(std::slice::from_ref(&a), false)
            .unwrap();
        store.save(&db_path).unwrap();

        let mut reopened = DocumentStore::open(&db_path).unwrap();
        let report = reopened
            .reindex_paths(std::slice::from_ref(&a), false)
            .unwrap();

        assert_eq!(report.unchanged, 1);
        assert!(report.added.is_empty());
        assert!(report.updated.is_empty());
    }

    #[test]
    fn reindex_reports_failure_for_unreadable_path_without_aborting_others() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_md(&dir, "a.md", "# A\n");
        let missing = dir.path().join("does-not-exist.md");

        let mut store = DocumentStore::new();
        let report = store
            .reindex_paths(&[a.clone(), missing.clone()], false)
            .unwrap();

        assert_eq!(report.added, vec![a]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, missing);
    }
}

#[cfg(test)]
mod vacuum_tests {
    use super::*;

    fn write_md(dir: &tempfile::TempDir, name: &str, content: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    fn open_for_writes(path: &Path) -> DocumentStore {
        let mut store = DocumentStore::open(path).unwrap();
        store.load_all_blocks().unwrap();
        store.load_all_indexes().unwrap();
        store
    }

    #[test]
    fn vacuum_reclaims_space_after_document_replace() {
        let dir = tempfile::tempdir().unwrap();
        let md_path = write_md(&dir, "a.md", "# A\n\nHello\n");
        let db_path = dir.path().join("store.mq-db");

        let mut store = DocumentStore::new();
        let doc_id = store.add_file(&md_path).unwrap();
        store.save(&db_path).unwrap();

        let mut opened = open_for_writes(&db_path);
        for i in 0..5 {
            opened
                .replace_document(
                    doc_id,
                    &format!("# A\n\nHello {i}\n"),
                    Some(md_path.clone()),
                )
                .unwrap();
        }

        let report = opened.vacuum(&db_path).unwrap();
        assert!(
            report.pages_before > report.pages_after,
            "expected reclaim, got before={} after={}",
            report.pages_before,
            report.pages_after
        );

        let reloaded = DocumentStore::load(&db_path).unwrap();
        assert_eq!(reloaded.documents().len(), 1);
        assert!(
            reloaded.documents()[0]
                .blocks
                .iter()
                .any(|b| b.content == "Hello 4")
        );
    }

    #[test]
    fn vacuum_reclaims_space_after_drop_table() {
        let dir = tempfile::tempdir().unwrap();
        let md_path = write_md(&dir, "a.md", "# A\n\nHello\n");
        let db_path = dir.path().join("store.mq-db");

        let mut store = DocumentStore::new();
        store.add_file(&md_path).unwrap();
        store.save(&db_path).unwrap();

        let mut opened = open_for_writes(&db_path);
        opened.execute_sql_mut("CREATE TABLE t (x TEXT)").unwrap();
        for i in 0..200 {
            opened
                .execute_sql_mut(&format!("INSERT INTO t VALUES ('row {i}')"))
                .unwrap();
        }
        opened.execute_sql_mut("DROP TABLE t").unwrap();

        let report = opened.vacuum(&db_path).unwrap();
        assert!(
            report.pages_before > report.pages_after,
            "expected reclaim, got before={} after={}",
            report.pages_before,
            report.pages_after
        );
    }

    #[test]
    fn vacuum_is_a_noop_when_nothing_to_reclaim() {
        let dir = tempfile::tempdir().unwrap();
        let md_path = write_md(&dir, "a.md", "# A\n\nHello\n");
        let db_path = dir.path().join("store.mq-db");

        let mut store = DocumentStore::new();
        store.add_file(&md_path).unwrap();
        store.save(&db_path).unwrap();

        let mut opened = open_for_writes(&db_path);
        let report = opened.vacuum(&db_path).unwrap();
        assert_eq!(report.pages_before, report.pages_after);
        assert_eq!(report.bytes_reclaimed(), 0);
    }

    #[test]
    fn vacuum_rejects_in_memory_only_store() {
        let mut store = DocumentStore::new();
        store.add_str("# A\n\nHello\n").unwrap();
        let err = store.vacuum("/tmp/does-not-matter.mq-db").unwrap_err();
        assert!(err.to_string().contains("backing file"));
    }

    #[test]
    fn vacuum_rejects_a_different_target_path() {
        let dir = tempfile::tempdir().unwrap();
        let md_path = write_md(&dir, "a.md", "# A\n\nHello\n");
        let db_path = dir.path().join("store.mq-db");
        let other_path = dir.path().join("other.mq-db");

        let mut store = DocumentStore::new();
        store.add_file(&md_path).unwrap();
        store.save(&db_path).unwrap();

        let mut opened = open_for_writes(&db_path);
        let err = opened.vacuum(&other_path).unwrap_err();
        assert!(err.to_string().contains("not the file"));

        let report = opened.vacuum(&db_path).unwrap();
        assert_eq!(report.pages_before, report.pages_after);
    }

    #[test]
    fn save_rejects_targeting_its_own_open_backing_file() {
        let dir = tempfile::tempdir().unwrap();
        let md_path = write_md(&dir, "a.md", "# A\n\nHello\n");
        let db_path = dir.path().join("store.mq-db");

        let mut store = DocumentStore::new();
        store.add_file(&md_path).unwrap();
        store.save(&db_path).unwrap();

        let mut opened = open_for_writes(&db_path);
        let err = opened.save(&db_path).unwrap_err();
        assert!(err.to_string().contains("vacuum"));

        // The original handle must still be able to commit.
        opened.execute_sql_mut("CREATE TABLE t (x TEXT)").unwrap();
        let reloaded = DocumentStore::load(&db_path).unwrap();
        assert!(reloaded.custom_tables.read().unwrap().contains_key("t"));
    }

    #[test]
    fn vacuum_preserves_views_and_custom_tables() {
        let dir = tempfile::tempdir().unwrap();
        let md_path = write_md(&dir, "a.md", "# A\n\nHello\n");
        let db_path = dir.path().join("store.mq-db");

        let mut store = DocumentStore::new();
        store.add_file(&md_path).unwrap();
        store.save(&db_path).unwrap();

        let mut opened = open_for_writes(&db_path);
        opened
            .execute_sql_mut(
                "CREATE VIEW v AS SELECT content FROM blocks WHERE block_type = 'heading'",
            )
            .unwrap();
        opened.execute_sql_mut("CREATE TABLE t (x TEXT)").unwrap();
        opened
            .execute_sql_mut("INSERT INTO t VALUES ('hello')")
            .unwrap();

        opened.vacuum(&db_path).unwrap();

        let out = opened.execute_sql_mut("SELECT content FROM v").unwrap();
        assert_eq!(out.rows, vec![vec!["A".to_string()]]);
        let out = opened.execute_sql_mut("SELECT x FROM t").unwrap();
        assert_eq!(out.rows, vec![vec!["hello".to_string()]]);

        // Also confirm a *fresh* open of the vacuumed file (not just the
        // live handle vacuum() reopened) sees the same state.
        let reloaded = DocumentStore::load(&db_path).unwrap();
        assert_eq!(reloaded.documents().len(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn vacuum_locks_the_replacement_inode_against_a_hard_link_alias() {
        let dir = tempfile::tempdir().unwrap();
        let md_path = write_md(&dir, "a.md", "# A\n\nHello\n");
        let db_path = dir.path().join("store.mq-db");

        let mut store = DocumentStore::new();
        store.add_file(&md_path).unwrap();
        store.save(&db_path).unwrap();

        let mut opened = open_for_writes(&db_path);
        opened.vacuum(&db_path).unwrap();

        // `db_path` now names the inode vacuum's rename left behind. A hard
        // link to it must still contend for the writer lock.
        let alias = dir.path().join("alias.mq-db");
        std::fs::hard_link(&db_path, &alias).unwrap();
        let err = Storage::open(&alias)
            .err()
            .expect("second writer through the post-vacuum hard link must fail");
        assert!(err.to_string().contains("already open for writing"));
    }
}

#[cfg(test)]
mod rollback_tests {
    use super::*;

    #[test]
    fn rollback_appended_rows_truncates_the_tail() {
        let store = DocumentStore::new();
        store.custom_tables.write().unwrap().insert(
            "t".to_string(),
            CustomTableState {
                columns: vec!["x".to_string()],
                rows: vec![
                    vec!["a".to_string()],
                    vec!["b".to_string()],
                    vec!["c".to_string()],
                ],
                first_row_page: 0,
                last_row_page: 0,
                not_null: vec![],
                unique: vec![],
            },
        );

        store.rollback_appended_rows("t", 2);

        let rows = store.custom_tables.read().unwrap()["t"].rows.clone();
        assert_eq!(rows, vec![vec!["a".to_string()]]);
    }

    #[test]
    fn rollback_appended_rows_ignores_unknown_table_and_zero_count() {
        let store = DocumentStore::new();
        store.rollback_appended_rows("missing", 3);
        store.rollback_appended_rows("missing", 0);
    }
}

#[cfg(test)]
mod tokenizer_tests {
    use super::*;

    #[test]
    fn set_tokenizer_rejects_non_empty_store() {
        let mut store = DocumentStore::new();
        store.add_str("# A\n\nHello\n").unwrap();
        let err = store.set_tokenizer(TokenizerKind::Trigram).unwrap_err();
        assert!(err.to_string().contains("cannot change tokenizer"));
    }

    #[test]
    fn save_then_open_round_trips_tokenizer_kind() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("store.mq-db");

        let mut store = DocumentStore::new();
        store.set_tokenizer(TokenizerKind::Trigram).unwrap();
        store.add_str("# 日記\n\n今日は良い天気です\n").unwrap();
        store.save(&db_path).unwrap();

        let opened = DocumentStore::open(&db_path).unwrap();
        assert_eq!(opened.tokenizer(), TokenizerKind::Trigram);

        let loaded = DocumentStore::load(&db_path).unwrap();
        assert_eq!(loaded.tokenizer(), TokenizerKind::Trigram);
    }

    #[test]
    fn saving_non_word_tokenizer_bumps_past_pre_tokenizer_version() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("store.mq-db");

        let mut store = DocumentStore::new();
        store.set_tokenizer(TokenizerKind::Trigram).unwrap();
        store.add_str("# Doc\n\n今日は良い天気です\n").unwrap();
        store.save(&db_path).unwrap();

        assert_ne!(DocumentStore::file_version(&db_path).unwrap(), 6);
    }

    #[test]
    fn load_reads_legacy_v6_file_without_tokenizer_tag_as_word() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("store.mq-db");

        let mut store = DocumentStore::new();
        store.add_str("# Doc\n\nHello world\n").unwrap();
        store.save(&db_path).unwrap();
        crate::storage::tests::patch_version(&db_path, 6);

        let loaded = DocumentStore::load(&db_path).unwrap();
        assert_eq!(loaded.tokenizer(), TokenizerKind::Word);
    }
}

#[cfg(test)]
mod containing_dir_tests {
    use super::containing_dir;
    use rstest::rstest;
    use std::path::Path;

    #[rstest]
    #[case("store.mq-db", ".")]
    #[case("./store.mq-db", ".")]
    #[case("dir/store.mq-db", "dir")]
    #[case("/abs/store.mq-db", "/abs")]
    fn bare_file_names_resolve_to_the_current_directory(
        #[case] path: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(containing_dir(Path::new(path)), Path::new(expected));
    }
}

#[cfg(test)]
mod vacuum_then_write_tests {
    use super::*;
    use crate::SqlEngine;

    /// A store with table `t` (2 rows) opened from a file, then vacuumed in
    /// the same process.
    fn vacuumed() -> (tempfile::TempDir, std::path::PathBuf, DocumentStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.mq-db");
        let mut s = DocumentStore::new();
        s.add_str("# T\n\nbody\n").unwrap();
        s.save(&path).unwrap();
        let mut s = DocumentStore::open(&path).unwrap();
        s.load_all_blocks().unwrap();
        s.load_all_indexes().unwrap();
        let e = SqlEngine::new(&s).unwrap();
        e.execute("CREATE TABLE t (a TEXT)").unwrap();
        e.execute("INSERT INTO t VALUES ('one'), ('two')").unwrap();
        drop(e);
        s.vacuum(&path).unwrap();
        (dir, path, s)
    }

    fn count_after_reopen(path: &std::path::Path) -> String {
        let r = DocumentStore::open(path).unwrap();
        SqlEngine::new(&r)
            .unwrap()
            .execute("SELECT count(*) FROM t")
            .unwrap()
            .rows[0][0]
            .clone()
    }

    #[test]
    fn catalog_only_write_after_vacuum_keeps_table_rows() {
        let (_dir, path, s) = vacuumed();
        SqlEngine::new(&s)
            .unwrap()
            .execute("CREATE VIEW v AS SELECT 1")
            .unwrap();
        drop(s);
        assert_eq!(count_after_reopen(&path), "2");
    }

    #[test]
    fn insert_after_vacuum_keeps_table_rows() {
        let (_dir, path, s) = vacuumed();
        SqlEngine::new(&s)
            .unwrap()
            .execute("INSERT INTO t VALUES ('three')")
            .unwrap();
        drop(s);
        assert_eq!(count_after_reopen(&path), "3");
    }
}
