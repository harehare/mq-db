use std::path::{Path, PathBuf};

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
#[derive(Default)]
pub struct DocumentStore {
    documents: Vec<Document>,
    /// Secondary indexes parallel to `documents` — same index `i` covers `documents[i]`.
    indexes: Vec<DocumentIndex>,
    next_doc_id: DocumentId,
}

impl DocumentStore {
    /// Creates an empty document store.
    pub fn new() -> Self {
        Self::default()
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

        let blocks = index::build_blocks(doc_id, &md.nodes);
        let doc_index = DocumentIndex::build(&blocks);
        let doc = Document::new(doc_id, path, blocks);
        self.documents.push(doc);
        self.indexes.push(doc_index);

        Ok(doc_id)
    }

    /// Returns a slice of all documents in the store.
    pub fn documents(&self) -> &[Document] {
        &self.documents
    }

    /// Returns documents paired with their secondary indexes.
    pub fn documents_with_indexes(&self) -> impl Iterator<Item = (&Document, &DocumentIndex)> {
        self.documents.iter().zip(self.indexes.iter())
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
                    num_blocks: doc.blocks.len() as u32,
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

    /// Load a `.mqdb` file and reconstruct the in-memory DocumentStore.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, MqdbError> {
        let mut storage = Storage::open(path.as_ref())?;
        let entries = storage.load_catalog()?;
        let mut documents = Vec::with_capacity(entries.len());
        let mut indexes = Vec::with_capacity(entries.len());
        let mut max_doc_id = None;

        for entry in entries {
            let blocks = storage.read_blocks(entry.first_block_page, entry.num_blocks)?;
            let zone_maps = decode_zone_map(&entry.zone_map_bytes)?;
            let document_id = entry.document_id;
            let path = entry.path.map(PathBuf::from);
            let doc_index = DocumentIndex::build(&blocks);
            documents.push(Document::from_parts(document_id, path, blocks, zone_maps));
            indexes.push(doc_index);
            max_doc_id = Some(
                max_doc_id.map_or(document_id, |current: DocumentId| current.max(document_id)),
            );
        }

        Ok(Self {
            documents,
            indexes,
            next_doc_id: max_doc_id.map_or(0, |id| id.saturating_add(1)),
        })
    }
}
