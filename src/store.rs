use std::path::{Path, PathBuf};

use mq_markdown::Markdown;

use crate::{
    block::DocumentId,
    document::Document,
    error::MqdbError,
    index,
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
/// Secondary indexes (`DocumentIndex`) are no longer kept here — they are
/// built on demand inside [`crate::SqlEngine`] so that commands that don't
/// use SQL (mq, list, show, stats …) pay no index-construction cost.
///
/// # Example
///
/// ```rust
/// use mqdb::DocumentStore;
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
}

impl Default for DocumentStore {
    fn default() -> Self {
        Self {
            documents: Vec::new(),
            next_doc_id: 0,
            store_spans: true,
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
    /// Has no effect on blocks already in the store.
    pub fn set_store_spans(&mut self, val: bool) {
        self.store_spans = val;
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

    /// Persist all in-memory documents to a `.mqdb` file.
    /// Writes atomically: writes to `path.tmp` then renames to `path`.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), MqdbError> {
        let path = path.as_ref();
        let tmp_path = PathBuf::from(format!("{}.tmp", path.to_string_lossy()));
        if tmp_path.exists() {
            std::fs::remove_file(&tmp_path)?;
        }

        let write_result = (|| -> Result<(), MqdbError> {
            let mut storage = Storage::create(&tmp_path)?;
            let mut entries = Vec::with_capacity(self.documents.len());

            for doc in &self.documents {
                let first_block_page = storage.write_document(doc)?;
                entries.push(CatalogEntry {
                    document_id: doc.id,
                    path: doc.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    first_block_page,
                    num_blocks: doc.block_count,
                    zone_map_bytes: encode_zone_map(&doc.zone_maps),
                });
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

    /// Load a `.mqdb` file and reconstruct the in-memory `DocumentStore`.
    ///
    /// All block data is read from disk. Secondary indexes are **not** built
    /// here — [`crate::SqlEngine`] builds them lazily on construction.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, MqdbError> {
        let mut storage = Storage::open(path.as_ref())?;
        let entries = storage.load_catalog()?;
        let mut documents = Vec::with_capacity(entries.len());
        let mut max_doc_id = None;

        for entry in entries {
            let blocks = storage.read_blocks(entry.first_block_page, entry.num_blocks)?;
            let zone_maps = decode_zone_map(&entry.zone_map_bytes)?;
            let document_id = entry.document_id;
            let path = entry.path.map(PathBuf::from);
            documents.push(Document::from_parts(document_id, path, blocks, zone_maps));
            max_doc_id = Some(
                max_doc_id.map_or(document_id, |current: DocumentId| current.max(document_id)),
            );
        }

        Ok(Self {
            documents,
            next_doc_id: max_doc_id.map_or(0, |id| id.saturating_add(1)),
            store_spans: true,
        })
    }

    /// Load only the catalog metadata from a `.mqdb` file — no block data.
    ///
    /// Documents have `block_count` populated from the catalog but `blocks`
    /// is empty. Useful for commands that only need zone-map metadata (e.g.
    /// `list`), avoiding the cost of deserialising all block data.
    pub fn load_catalog_only(path: impl AsRef<Path>) -> Result<Self, MqdbError> {
        let mut storage = Storage::open(path.as_ref())?;
        let entries = storage.load_catalog()?;
        let mut documents = Vec::with_capacity(entries.len());
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
            max_doc_id = Some(
                max_doc_id.map_or(document_id, |current: DocumentId| current.max(document_id)),
            );
        }

        Ok(Self {
            documents,
            next_doc_id: max_doc_id.map_or(0, |id| id.saturating_add(1)),
            store_spans: true,
        })
    }
}
