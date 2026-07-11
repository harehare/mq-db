use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Mutex, RwLock},
};

/// In-memory state for a user-defined table.
///
/// `first_row_page`/`last_row_page` track where this table's rows live in
/// the backing storage file (0 = not persisted yet), so a SQL `INSERT`
/// can append just the new rows to the chain instead of rewriting `rows`
/// in full on every call. See [`Storage::write_table_rows`].
pub(crate) struct CustomTableState {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub first_row_page: u32,
    pub last_row_page: u32,
}

use mq_markdown::Markdown;

use crate::{
    block::{BlockType, DocumentId},
    document::Document,
    error::MqdbError,
    index,
    indexes::DocumentIndex,
    query::Query,
    storage::{
        Storage,
        catalog::{CatalogEntry, CustomTableEntry},
        codec::{decode_zone_map, encode_zone_map},
    },
};

/// Persists any table whose rows have never been written to `storage` (i.e.
/// `first_row_page == 0`), then builds catalog entries for every table.
///
/// Tables that already have a row-page chain are left untouched here — their
/// pages were already written by an earlier flush or incremental `INSERT`
/// append (see [`DocumentStore::try_append_table_rows_to_storage`]).
fn persist_unsaved_table_rows(
    storage: &mut Storage,
    custom_tables: &RwLock<HashMap<String, CustomTableState>>,
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
    /// `(language, count)`, most frequent first — code blocks only.
    pub code_lang_counts: Vec<(String, usize)>,
}

/// The top-level embedded document store.
///
/// Holds a collection of parsed Markdown documents and provides access to
/// the query interface. Documents are stored in memory with their flattened
/// block lists and interval indexes.
///
/// ## Load modes
///
/// - [`DocumentStore::new`] / [`DocumentStore::add_str`] — in-memory, blocks immediately available
/// - [`DocumentStore::load`] — reads all blocks from a `.mq-db` file into memory
/// - [`DocumentStore::open`] — reads catalog only; blocks loaded on demand via
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
/// let results = store.query().heading_depth(1).blocks();
/// assert_eq!(results.len(), 1);
/// assert_eq!(results[0].content, "Hello");
/// ```
pub struct DocumentStore {
    documents: Vec<Document>,
    next_doc_id: DocumentId,
    /// When `false`, source line/column spans are discarded after parsing.
    store_spans: bool,
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
    pub(crate) custom_tables: RwLock<HashMap<String, CustomTableState>>,
    /// Content hash of each document's source, keyed by `DocumentId`. Used by
    /// [`reindex_paths`](DocumentStore::reindex_paths) to skip re-parsing
    /// files whose content hasn't changed since the last index run. Absent
    /// entries (e.g. documents added via `add_str`, or loaded from an older
    /// `.mq-db` file predating this feature) are treated as "unknown, always
    /// reindex".
    content_hashes: HashMap<DocumentId, u64>,
}

impl Default for DocumentStore {
    fn default() -> Self {
        Self {
            documents: Vec::new(),
            next_doc_id: 0,
            store_spans: true,
            storage: Mutex::new(None),
            doc_indexes: Vec::new(),
            custom_tables: RwLock::new(HashMap::new()),
            content_hashes: HashMap::new(),
        }
    }
}

/// Hash of file/document content used for change detection (not
/// cryptographic — this only needs to detect "did this file change since
/// last index", not resist adversarial collisions).
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Reads every file in `files`, in order, using a small worker-thread pool —
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
    pub fn new() -> Self {
        Self::default()
    }

    /// When set to `false`, source line/column spans are stripped from every
    /// block added after this call. Reduces memory by ~21 bytes per block.
    pub fn set_store_spans(&mut self, val: bool) {
        self.store_spans = val;
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
        self.custom_tables.write().unwrap().insert(
            name.into(),
            CustomTableState {
                columns,
                rows,
                first_row_page: 0,
                last_row_page: 0,
            },
        );
    }

    /// Remove a previously registered custom table. Returns `true` if it existed.
    pub fn unregister_table(&mut self, name: &str) -> bool {
        self.custom_tables.write().unwrap().remove(name).is_some()
    }

    /// Parses and adds a Markdown file from disk.
    ///
    /// Returns the assigned `DocumentId` on success.
    pub fn add_file(&mut self, path: impl AsRef<Path>) -> Result<DocumentId, MqdbError> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)?;
        self.add_str_with_path(&content, Some(path.to_path_buf()))
    }

    /// Parses and adds Markdown content from a string.
    ///
    /// Returns the assigned `DocumentId` on success.
    pub fn add_str(&mut self, content: &str) -> Result<DocumentId, MqdbError> {
        self.add_str_with_path(content, None)
    }

    /// Parses and adds already-read Markdown content, attributing it to
    /// `path`. For callers that read files concurrently and want to skip
    /// [`add_file`](Self::add_file)'s own read.
    pub fn add_str_with_path(
        &mut self,
        content: &str,
        path: Option<std::path::PathBuf>,
    ) -> Result<DocumentId, MqdbError> {
        let md =
            Markdown::from_markdown_str(content).map_err(|e| MqdbError::Parse(e.to_string()))?;

        let doc_id = self.next_doc_id;
        self.next_doc_id += 1;

        let mut blocks = index::build_blocks(doc_id, &md.nodes);
        if !self.store_spans {
            for block in &mut blocks {
                block.span = None;
            }
        }
        let doc = Document::new(doc_id, path, blocks);
        self.documents.push(doc);
        self.doc_indexes.push(None);

        Ok(doc_id)
    }

    /// Append a Markdown string to the existing `.mq-db` file (in-place).
    ///
    /// Works only when the store was opened via [`DocumentStore::open`] (i.e.
    /// `self.storage` is `Some`).  New block pages and an index page chain are
    /// appended to the file and the catalog is rewritten to include the new
    /// entry.
    ///
    /// When called on an in-memory store (no backing file) this behaves
    /// identically to [`add_str`](DocumentStore::add_str).
    pub fn append_str(&mut self, content: &str) -> Result<DocumentId, MqdbError> {
        self.do_append(content, None)
    }

    /// Append a Markdown file to the existing `.mq-db` file (in-place).
    ///
    /// See [`append_str`](DocumentStore::append_str) for full semantics.
    pub fn append_file(&mut self, path: impl AsRef<Path>) -> Result<DocumentId, MqdbError> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)?;
        self.do_append(&content, Some(path.to_path_buf()))
    }

    fn do_append(
        &mut self,
        content: &str,
        md_path: Option<PathBuf>,
    ) -> Result<DocumentId, MqdbError> {
        let md =
            Markdown::from_markdown_str(content).map_err(|e| MqdbError::Parse(e.to_string()))?;
        let doc_id = self.next_doc_id;
        self.next_doc_id += 1;

        let mut blocks = index::build_blocks(doc_id, &md.nodes);
        if !self.store_spans {
            for block in &mut blocks {
                block.span = None;
            }
        }
        let mut doc = Document::new(doc_id, md_path, blocks);

        let idx_opt = {
            let mut storage_guard = self.storage.lock().unwrap();
            if let Some(storage) = storage_guard.as_mut() {
                // Reconstruct catalog entries from already-loaded document metadata.
                let mut entries = self.catalog_entries();

                let first_block_page = storage.write_document(&doc)?;
                doc.first_block_page = first_block_page;

                let idx = DocumentIndex::build(&doc.blocks);
                let index_start_page = storage.write_index(&idx.to_bytes())?;
                doc.index_start_page = index_start_page;

                entries.push(CatalogEntry {
                    document_id: doc.id,
                    path: doc.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    first_block_page,
                    num_blocks: doc.block_count,
                    zone_map_bytes: encode_zone_map(&doc.zone_maps),
                    index_start_page,
                });

                let custom = persist_unsaved_table_rows(storage, &self.custom_tables)?;
                storage.flush_catalog(&entries, &custom, &self.content_hash_pairs())?;
                Some(idx)
            } else {
                None
            }
        };
        self.doc_indexes.push(idx_opt);

        self.documents.push(doc);
        Ok(doc_id)
    }

    /// Replace the content of an existing document in place, keeping its
    /// `DocumentId` stable (so any external references to it — e.g. `mq()`
    /// join columns, application code holding the id — stay valid).
    ///
    /// Re-parses `content`, writes fresh block/index page chains to the
    /// backing storage file (if one is open) and overwrites that document's
    /// catalog entry; the old page chains become orphaned dead space (no
    /// compaction yet — a future `vacuum` command could reclaim them).
    ///
    /// For in-memory-only stores (no backing file open), this only updates
    /// in-memory state — call [`save`](DocumentStore::save) afterward to
    /// persist.
    ///
    /// Returns an error if no document with `doc_id` exists.
    pub fn replace_document(
        &mut self,
        doc_id: DocumentId,
        content: &str,
        path: Option<PathBuf>,
    ) -> Result<(), MqdbError> {
        let pos = self
            .documents
            .iter()
            .position(|d| d.id == doc_id)
            .ok_or_else(|| MqdbError::Storage(format!("no such document: {doc_id}")))?;

        let md =
            Markdown::from_markdown_str(content).map_err(|e| MqdbError::Parse(e.to_string()))?;
        let mut blocks = index::build_blocks(doc_id, &md.nodes);
        if !self.store_spans {
            for block in &mut blocks {
                block.span = None;
            }
        }
        let mut doc = Document::new(doc_id, path, blocks);

        let idx_opt = {
            let mut storage_guard = self.storage.lock().unwrap();
            if let Some(storage) = storage_guard.as_mut() {
                let mut entries = self.catalog_entries();

                let first_block_page = storage.write_document(&doc)?;
                doc.first_block_page = first_block_page;

                let idx = DocumentIndex::build(&doc.blocks);
                let index_start_page = storage.write_index(&idx.to_bytes())?;
                doc.index_start_page = index_start_page;

                if let Some(entry) = entries.iter_mut().find(|e| e.document_id == doc_id) {
                    entry.path = doc.path.as_ref().map(|p| p.to_string_lossy().into_owned());
                    entry.first_block_page = first_block_page;
                    entry.num_blocks = doc.block_count;
                    entry.zone_map_bytes = encode_zone_map(&doc.zone_maps);
                    entry.index_start_page = index_start_page;
                }

                let custom = persist_unsaved_table_rows(storage, &self.custom_tables)?;
                storage.flush_catalog(&entries, &custom, &self.content_hash_pairs())?;
                Some(idx)
            } else {
                None
            }
        };

        self.documents[pos] = doc;
        self.doc_indexes[pos] = idx_opt;
        Ok(())
    }

    /// Index `files`, skipping any whose content hash matches what's already
    /// catalogued (see `content_hashes`), replacing changed ones in place via
    /// [`replace_document`](DocumentStore::replace_document) (same
    /// `DocumentId`), and adding new ones exactly like
    /// [`append_file`](DocumentStore::append_file)/[`add_file`](DocumentStore::add_file)
    /// depending on whether a backing file is open.
    ///
    /// When `prune` is `true`, any catalogued document whose path is not
    /// present in `files` is dropped from the store.
    ///
    /// Documents with no path (added via `add_str`) are left untouched and
    /// are never counted as "removed" by `prune`.
    pub fn reindex_paths(
        &mut self,
        files: &[PathBuf],
        prune: bool,
    ) -> Result<ReindexReport, MqdbError> {
        let mut report = ReindexReport::default();
        let mut seen: HashSet<PathBuf> = HashSet::with_capacity(files.len());
        let contents = read_files_parallel(files);

        for (path, content) in files.iter().zip(contents) {
            seen.insert(path.clone());
            let result = (|| -> Result<(), MqdbError> {
                let content = content?;
                let hash = hash_bytes(content.as_bytes());

                let existing = self
                    .documents
                    .iter()
                    .find(|d| d.path.as_deref() == Some(path.as_path()))
                    .map(|d| d.id);

                match existing {
                    Some(doc_id) if self.content_hashes.get(&doc_id) == Some(&hash) => {
                        report.unchanged += 1;
                    }
                    Some(doc_id) => {
                        self.replace_document(doc_id, &content, Some(path.clone()))?;
                        self.content_hashes.insert(doc_id, hash);
                        report.updated.push(path.clone());
                    }
                    None => {
                        let doc_id = if self.storage.lock().unwrap().is_some() {
                            self.do_append(&content, Some(path.clone()))?
                        } else {
                            self.add_str_with_path(&content, Some(path.clone()))?
                        };
                        self.content_hashes.insert(doc_id, hash);
                        report.added.push(path.clone());
                    }
                }
                Ok(())
            })();

            if let Err(e) = result {
                report.failed.push((path.clone(), e.to_string()));
            }
        }

        if prune {
            let to_remove: Vec<(usize, DocumentId)> = self
                .documents
                .iter()
                .enumerate()
                .filter(|(_, d)| d.path.as_ref().is_some_and(|p| !seen.contains(p)))
                .map(|(i, d)| (i, d.id))
                .collect();
            // Remove back-to-front so earlier indices stay valid.
            for (i, doc_id) in to_remove.into_iter().rev() {
                let removed_doc = self.documents.remove(i);
                self.doc_indexes.remove(i);
                self.content_hashes.remove(&doc_id);
                if let Some(p) = removed_doc.path {
                    report.removed.push(p);
                }
            }
        }

        self.try_flush_catalog_to_storage();
        Ok(report)
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

    /// Creates a new query builder backed by this store.
    pub fn query(&self) -> Query<'_> {
        Query::new(self)
    }

    /// Aggregate block-type / code-language statistics across every
    /// document currently loaded in memory.
    pub fn stats(&self) -> StoreStats {
        let mut type_counts: HashMap<BlockType, usize> = HashMap::new();
        let mut lang_counts: HashMap<String, usize> = HashMap::new();
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

    // ─────────────────────────────────────────────────────────────────────────
    // Lazy loading
    // ─────────────────────────────────────────────────────────────────────────

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

        Ok(DocumentIndex::build(&self.documents[i].blocks))
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

    /// Flush the catalog (including custom tables) to the backing storage file,
    /// if one is open. Called automatically after DDL operations such as
    /// `CREATE TABLE` and `DROP TABLE`. No-op for in-memory stores.
    ///
    /// Any table whose rows have never been persisted is written out in full
    /// here (a one-time cost). Tables already backed by a row-page chain keep
    /// their existing pages untouched — see
    /// [`try_append_table_rows_to_storage`](DocumentStore::try_append_table_rows_to_storage)
    /// for the incremental `INSERT` path.
    pub(crate) fn try_flush_catalog_to_storage(&self) {
        let mut guard = self.storage.lock().unwrap();
        if let Some(storage) = guard.as_mut() {
            let entries = self.catalog_entries();
            if let Ok(custom) = persist_unsaved_table_rows(storage, &self.custom_tables) {
                let _ = storage.flush_catalog(&entries, &custom, &self.content_hash_pairs());
            }
        }
    }

    /// Append `new_rows` to `table_name`'s on-disk row chain and flush a
    /// lightweight catalog update — no full row rewrite. No-op for in-memory
    /// stores or unknown tables.
    ///
    /// This is what makes `INSERT INTO <table>` incremental: the cost is
    /// proportional to the rows being inserted, not to the table's total size.
    pub(crate) fn try_append_table_rows_to_storage(
        &self,
        table_name: &str,
        new_rows: &[Vec<String>],
    ) {
        let mut guard = self.storage.lock().unwrap();
        let storage = match guard.as_mut() {
            Some(s) => s,
            None => return,
        };

        {
            let mut ct_guard = self.custom_tables.write().unwrap();
            if let Some(state) = ct_guard.get_mut(table_name) {
                let persisted = if state.first_row_page == 0 {
                    // Nothing persisted yet for this table — write everything
                    // currently in memory (covers rows seeded via
                    // `register_table` plus the ones just inserted).
                    storage.write_table_rows(&state.rows)
                } else {
                    storage
                        .append_table_rows(state.last_row_page, new_rows)
                        .map(|last| (state.first_row_page, last))
                };
                if let Ok((first, last)) = persisted {
                    state.first_row_page = first;
                    state.last_row_page = last;
                }
            }
        }

        let entries = self.catalog_entries();
        let ct_guard = self.custom_tables.read().unwrap();
        let custom: Vec<CustomTableEntry> = ct_guard
            .iter()
            .map(|(name, state)| CustomTableEntry {
                name: name.clone(),
                columns: state.columns.clone(),
                first_row_page: state.first_row_page,
                last_row_page: state.last_row_page,
                num_rows: state.rows.len() as u32,
            })
            .collect();
        drop(ct_guard);
        let _ = storage.flush_catalog(&entries, &custom, &self.content_hash_pairs());
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Persistence
    // ─────────────────────────────────────────────────────────────────────────

    /// Persist all in-memory documents to a `.mq-db` file, including secondary
    /// indexes. Writes atomically: writes to `path.tmp` then renames to `path`.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), MqdbError> {
        let path = path.as_ref();
        let tmp_path = PathBuf::from(format!("{}.tmp", path.to_string_lossy()));
        if tmp_path.exists() {
            std::fs::remove_file(&tmp_path)?;
        }

        let write_result = (|| -> Result<(), MqdbError> {
            let mut storage = Storage::create(&tmp_path)?;
            let mut entries = Vec::with_capacity(self.documents.len());

            // Phase 1: write block data
            for doc in &self.documents {
                let first_block_page = storage.write_document(doc)?;
                entries.push(CatalogEntry {
                    document_id: doc.id,
                    path: doc.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    first_block_page,
                    num_blocks: doc.block_count,
                    zone_map_bytes: encode_zone_map(&doc.zone_maps),
                    index_start_page: 0,
                });
            }

            // Phase 2: write secondary indexes
            for (i, doc) in self.documents.iter().enumerate() {
                let idx = if let Some(cached) = self.doc_indexes.get(i).and_then(|o| o.as_ref()) {
                    std::borrow::Cow::Borrowed(cached)
                } else {
                    std::borrow::Cow::Owned(DocumentIndex::build(&doc.blocks))
                };
                let bytes = idx.to_bytes();
                entries[i].index_start_page = storage.write_index(&bytes)?;
            }

            // This writes into a brand-new file, so each table's rows are
            // written fresh here rather than reusing `first_row_page` /
            // `last_row_page` from `self`, which (if set) point into a
            // *different*, already-open backing file.
            let ct_guard = self.custom_tables.read().unwrap();
            let mut custom = Vec::with_capacity(ct_guard.len());
            for (name, state) in ct_guard.iter() {
                let (first_row_page, last_row_page) = storage.write_table_rows(&state.rows)?;
                custom.push(CustomTableEntry {
                    name: name.clone(),
                    columns: state.columns.clone(),
                    first_row_page,
                    last_row_page,
                    num_rows: state.rows.len() as u32,
                });
            }
            drop(ct_guard);

            storage.flush_catalog(&entries, &custom, &self.content_hash_pairs())?;
            Ok(())
        })();

        if let Err(err) = write_result {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }

        std::fs::rename(&tmp_path, path)?;
        Ok(())
    }

    /// Open a `.mq-db` file in lazy mode: reads only catalog and zone maps.
    ///
    /// Block data is not loaded until you call
    /// [`load_all_blocks`](DocumentStore::load_all_blocks).  Secondary indexes
    /// are not built until you call
    /// [`load_all_indexes`](DocumentStore::load_all_indexes).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MqdbError> {
        let mut storage = Storage::open(path.as_ref())?;
        let (entries, custom_table_entries, content_hashes) = storage.load_catalog()?;
        let cap = entries.len();
        let mut documents = Vec::with_capacity(cap);
        let mut max_doc_id = None;

        for entry in entries {
            let zone_maps = decode_zone_map(&entry.zone_map_bytes)?;
            let document_id = entry.document_id;
            let path = entry.path.map(PathBuf::from);
            documents.push(Document::from_catalog_lazy(
                document_id,
                path,
                entry.num_blocks,
                zone_maps,
                entry.first_block_page,
                entry.index_start_page,
            ));
            max_doc_id =
                Some(max_doc_id.map_or(document_id, |cur: DocumentId| cur.max(document_id)));
        }

        let mut custom_tables = HashMap::new();
        for ct in custom_table_entries {
            let rows = storage.read_table_rows(ct.first_row_page, ct.num_rows, ct.columns.len())?;
            custom_tables.insert(
                ct.name,
                CustomTableState {
                    columns: ct.columns,
                    rows,
                    first_row_page: ct.first_row_page,
                    last_row_page: ct.last_row_page,
                },
            );
        }

        Ok(Self {
            documents,
            next_doc_id: max_doc_id.map_or(0, |id| id.saturating_add(1)),
            store_spans: true,
            storage: Mutex::new(Some(storage)),
            doc_indexes: vec![None; cap],
            custom_tables: RwLock::new(custom_tables),
            content_hashes: content_hashes.into_iter().collect(),
        })
    }

    /// Load a `.mq-db` file and reconstruct the in-memory `DocumentStore`.
    ///
    /// All block data is read from disk. Secondary indexes are **not** built
    /// here — [`crate::SqlEngine`] builds them lazily on construction.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, MqdbError> {
        let mut storage = Storage::open(path.as_ref())?;
        let (entries, custom_table_entries, content_hashes) = storage.load_catalog()?;
        let cap = entries.len();
        let mut documents = Vec::with_capacity(cap);
        let mut max_doc_id = None;

        for entry in entries {
            let blocks = storage.read_blocks(entry.first_block_page, entry.num_blocks)?;
            let zone_maps = decode_zone_map(&entry.zone_map_bytes)?;
            let document_id = entry.document_id;
            let path = entry.path.map(PathBuf::from);
            let mut doc = Document::from_parts(document_id, path, blocks, zone_maps);
            doc.index_start_page = entry.index_start_page;
            documents.push(doc);
            max_doc_id =
                Some(max_doc_id.map_or(document_id, |cur: DocumentId| cur.max(document_id)));
        }

        let mut custom_tables = HashMap::new();
        for ct in custom_table_entries {
            let rows = storage.read_table_rows(ct.first_row_page, ct.num_rows, ct.columns.len())?;
            custom_tables.insert(
                ct.name,
                CustomTableState {
                    columns: ct.columns,
                    rows,
                    first_row_page: ct.first_row_page,
                    last_row_page: ct.last_row_page,
                },
            );
        }

        Ok(Self {
            documents,
            next_doc_id: max_doc_id.map_or(0, |id| id.saturating_add(1)),
            store_spans: true,
            storage: Mutex::new(None),
            doc_indexes: vec![None; cap],
            custom_tables: RwLock::new(custom_tables),
            content_hashes: content_hashes.into_iter().collect(),
        })
    }

    /// Load only the catalog metadata from a `.mq-db` file — no block data.
    ///
    /// Documents have `block_count` populated from the catalog but `blocks`
    /// is empty. Useful for commands that only need zone-map metadata (e.g.
    /// `list`), avoiding the cost of deserialising all block data.
    pub fn load_catalog_only(path: impl AsRef<Path>) -> Result<Self, MqdbError> {
        let mut storage = Storage::open(path.as_ref())?;
        let (entries, _custom_table_entries, content_hashes) = storage.load_catalog()?;
        let cap = entries.len();
        let mut documents = Vec::with_capacity(cap);
        let mut max_doc_id = None;

        for entry in entries {
            let zone_maps = decode_zone_map(&entry.zone_map_bytes)?;
            let document_id = entry.document_id;
            let path = entry.path.map(PathBuf::from);
            documents.push(Document::from_catalog(
                document_id,
                path,
                entry.num_blocks,
                zone_maps,
            ));
            max_doc_id =
                Some(max_doc_id.map_or(document_id, |cur: DocumentId| cur.max(document_id)));
        }

        Ok(Self {
            documents,
            next_doc_id: max_doc_id.map_or(0, |id| id.saturating_add(1)),
            store_spans: true,
            storage: Mutex::new(None),
            doc_indexes: vec![None; cap],
            custom_tables: RwLock::new(HashMap::new()),
            content_hashes: content_hashes.into_iter().collect(),
        })
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
