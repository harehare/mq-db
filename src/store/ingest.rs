use super::*;

impl DocumentStore {
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
        self.do_append(content, None, true)
    }

    /// Append a Markdown file to the existing `.mq-db` file (in-place).
    ///
    /// See [`append_str`](DocumentStore::append_str) for full semantics.
    pub fn append_file(&mut self, path: impl AsRef<Path>) -> Result<DocumentId, MqdbError> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)?;
        self.do_append(&content, Some(path.to_path_buf()), true)
    }

    /// Core of [`append_str`](Self::append_str)/[`append_file`](Self::append_file).
    ///
    /// `flush` rewrites the on-disk catalog immediately if `true`. Bulk
    /// callers like [`reindex_paths`](Self::reindex_paths) pass `false` and
    /// flush once after the whole batch instead.
    fn do_append(
        &mut self,
        content: &str,
        md_path: Option<PathBuf>,
        flush: bool,
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
                let first_block_page = storage.write_document(&doc)?;
                doc.first_block_page = first_block_page;

                let idx = DocumentIndex::build(&doc.blocks, self.tokenizer);
                let index_start_page = storage.write_index(&idx.to_bytes())?;
                doc.index_start_page = index_start_page;

                Some(idx)
            } else {
                None
            }
        };
        self.doc_indexes.push(idx_opt);
        self.documents.push(doc);

        if flush && let Err(err) = self.flush_catalog_to_storage() {
            self.documents.pop();
            self.doc_indexes.pop();
            self.next_doc_id -= 1;
            return Err(err);
        }
        Ok(doc_id)
    }

    /// Replace the content of an existing document in place, keeping its
    /// `DocumentId` stable (so any external references to it, e.g. `mq()`
    /// join columns, application code holding the id, stay valid).
    ///
    /// Re-parses `content`, writes fresh block/index page chains to the
    /// backing storage file (if one is open) and overwrites that document's
    /// catalog entry; the old page chains become orphaned dead space (no
    /// compaction yet; a future `vacuum` command could reclaim them).
    ///
    /// For in-memory-only stores (no backing file open), this only updates
    /// in-memory state; call [`save`](DocumentStore::save) afterward to
    /// persist.
    ///
    /// Returns an error if no document with `doc_id` exists.
    pub fn replace_document(
        &mut self,
        doc_id: DocumentId,
        content: &str,
        path: Option<PathBuf>,
    ) -> Result<(), MqdbError> {
        self.do_replace(doc_id, content, path, true)
    }

    /// Core of [`replace_document`](Self::replace_document). See
    /// [`do_append`](Self::do_append) for what `flush` controls and why.
    fn do_replace(
        &mut self,
        doc_id: DocumentId,
        content: &str,
        path: Option<PathBuf>,
        flush: bool,
    ) -> Result<(), MqdbError> {
        let pos = self
            .documents
            .iter()
            .position(|d| d.id == doc_id)
            .ok_or_else(|| MqdbError::Storage(format!("no such document: {doc_id}")))?;
        self.do_replace_at(pos, doc_id, content, path, flush)
    }

    /// Like [`do_replace`](Self::do_replace) but takes the document's index
    /// in `self.documents` directly instead of scanning for it.
    fn do_replace_at(
        &mut self,
        pos: usize,
        doc_id: DocumentId,
        content: &str,
        path: Option<PathBuf>,
        flush: bool,
    ) -> Result<(), MqdbError> {
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
                let first_block_page = storage.write_document(&doc)?;
                doc.first_block_page = first_block_page;

                let idx = DocumentIndex::build(&doc.blocks, self.tokenizer);
                let index_start_page = storage.write_index(&idx.to_bytes())?;
                doc.index_start_page = index_start_page;

                Some(idx)
            } else {
                None
            }
        };

        let previous_doc = std::mem::replace(&mut self.documents[pos], doc);
        let previous_idx = std::mem::replace(&mut self.doc_indexes[pos], idx_opt);

        if flush && let Err(err) = self.flush_catalog_to_storage() {
            self.documents[pos] = previous_doc;
            self.doc_indexes[pos] = previous_idx;
            return Err(err);
        }
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
        let documents_snapshot = self.documents.clone();
        let content_hashes_snapshot = self.content_hashes.clone();

        // Path -> (DocumentId, position), for O(1) lookup instead of an
        // O(N) scan per file below.
        let mut by_path: FxHashMap<PathBuf, (DocumentId, usize)> = self
            .documents
            .iter()
            .enumerate()
            .filter_map(|(i, d)| d.path.clone().map(|p| (p, (d.id, i))))
            .collect();

        for (path, content) in files.iter().zip(contents) {
            seen.insert(path.clone());
            let result = (|| -> Result<(), MqdbError> {
                let content = content?;
                let hash = hash_bytes(content.as_bytes());

                let existing = by_path.get(path).copied();

                match existing {
                    Some((doc_id, _)) if self.content_hashes.get(&doc_id) == Some(&hash) => {
                        report.unchanged += 1;
                    }
                    Some((doc_id, pos)) => {
                        self.do_replace_at(pos, doc_id, &content, Some(path.clone()), false)?;
                        self.content_hashes.insert(doc_id, hash);
                        report.updated.push(path.clone());
                    }
                    None => {
                        let doc_id = if self.storage.lock().unwrap().is_some() {
                            self.do_append(&content, Some(path.clone()), false)?
                        } else {
                            self.add_str_with_path(&content, Some(path.clone()))?
                        };
                        self.content_hashes.insert(doc_id, hash);
                        by_path.insert(path.clone(), (doc_id, self.documents.len() - 1));
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

        if let Err(err) = self.flush_catalog_to_storage() {
            self.documents = documents_snapshot;
            self.doc_indexes = vec![None; self.documents.len()];
            self.content_hashes = content_hashes_snapshot;
            return Err(err);
        }
        Ok(report)
    }
}
