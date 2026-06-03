use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::RwLock,
};

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
        catalog::CatalogEntry,
        codec::{decode_zone_map, encode_zone_map},
    },
};

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
    pub(crate) storage: Option<Storage>,
    /// Per-document secondary index cache (same order as `documents`).
    /// `None` means the index has not been built/loaded for that document yet.
    pub(crate) doc_indexes: Vec<Option<DocumentIndex>>,
    /// User-registered virtual tables: name → (columns, rows).
    /// Uses `RwLock` for interior mutability so `SqlEngine` can execute DDL
    /// (`CREATE TABLE`, `INSERT INTO`, `DROP TABLE`) with only `&DocumentStore`.
    pub(crate) custom_tables: RwLock<HashMap<String, (Vec<String>, Vec<Vec<String>>)>>,
}

impl Default for DocumentStore {
    fn default() -> Self {
        Self {
            documents: Vec::new(),
            next_doc_id: 0,
            store_spans: true,
            storage: None,
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
        self.custom_tables
            .write()
            .unwrap()
            .insert(name.into(), (columns, rows));
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

    fn add_str_with_path(
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
        let storage = match self.storage.as_mut() {
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

        if index_start_page > 0
            && let Some(storage) = self.storage.as_mut()
        {
            let bytes = storage.read_index_bytes(index_start_page)?;
            return DocumentIndex::from_bytes(&bytes);
        }

        Ok(DocumentIndex::build(&self.documents[i].blocks))
    }

    /// Returns the cached `DocumentIndex` for the document at position `i`.
    pub(crate) fn get_doc_index(&self, i: usize) -> Option<&DocumentIndex> {
        self.doc_indexes.get(i).and_then(|o| o.as_ref())
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

            storage.flush_catalog(&entries)?;
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
        let entries = storage.load_catalog()?;
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

        Ok(Self {
            documents,
            next_doc_id: max_doc_id.map_or(0, |id| id.saturating_add(1)),
            store_spans: true,
            storage: Some(storage),
            doc_indexes: vec![None; cap],
            custom_tables: RwLock::new(HashMap::new()),
        })
    }

    /// Load a `.mq-db` file and reconstruct the in-memory `DocumentStore`.
    ///
    /// All block data is read from disk. Secondary indexes are **not** built
    /// here — [`crate::SqlEngine`] builds them lazily on construction.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, MqdbError> {
        let mut storage = Storage::open(path.as_ref())?;
        let entries = storage.load_catalog()?;
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

        Ok(Self {
            documents,
            next_doc_id: max_doc_id.map_or(0, |id| id.saturating_add(1)),
            store_spans: true,
            storage: None,
            doc_indexes: vec![None; cap],
            custom_tables: RwLock::new(HashMap::new()),
        })
    }

    /// Load only the catalog metadata from a `.mq-db` file — no block data.
    ///
    /// Documents have `block_count` populated from the catalog but `blocks`
    /// is empty. Useful for commands that only need zone-map metadata (e.g.
    /// `list`), avoiding the cost of deserialising all block data.
    pub fn load_catalog_only(path: impl AsRef<Path>) -> Result<Self, MqdbError> {
        let mut storage = Storage::open(path.as_ref())?;
        let entries = storage.load_catalog()?;
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
            storage: None,
            doc_indexes: vec![None; cap],
            custom_tables: RwLock::new(HashMap::new()),
        })
    }
}
