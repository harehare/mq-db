use std::{
    collections::HashMap,
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
    block::DocumentId,
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
        }
    }
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
                storage.flush_catalog(&entries, &custom)?;
                Some(idx)
            } else {
                None
            }
        };
        self.doc_indexes.push(idx_opt);

        self.documents.push(doc);
        Ok(doc_id)
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
                let _ = storage.flush_catalog(&entries, &custom);
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
        let _ = storage.flush_catalog(&entries, &custom);
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

            storage.flush_catalog(&entries, &custom)?;
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
        let (entries, custom_table_entries) = storage.load_catalog()?;
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
        })
    }

    /// Load a `.mq-db` file and reconstruct the in-memory `DocumentStore`.
    ///
    /// All block data is read from disk. Secondary indexes are **not** built
    /// here — [`crate::SqlEngine`] builds them lazily on construction.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, MqdbError> {
        let mut storage = Storage::open(path.as_ref())?;
        let (entries, custom_table_entries) = storage.load_catalog()?;
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
        })
    }

    /// Load only the catalog metadata from a `.mq-db` file — no block data.
    ///
    /// Documents have `block_count` populated from the catalog but `blocks`
    /// is empty. Useful for commands that only need zone-map metadata (e.g.
    /// `list`), avoiding the cost of deserialising all block data.
    pub fn load_catalog_only(path: impl AsRef<Path>) -> Result<Self, MqdbError> {
        let mut storage = Storage::open(path.as_ref())?;
        let (entries, _custom_table_entries) = storage.load_catalog()?;
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
        })
    }
}
