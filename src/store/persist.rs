use super::vector_indexes::states_from_entries;
use super::*;

impl DocumentStore {
    /// Persist all in-memory documents to a `.mq-db` file, including secondary
    /// indexes. Writes atomically: writes to `path.tmp` then renames to `path`.
    ///
    /// Errors if `path` is the store's own open backing file (would keep
    /// writing through the old, unlinked inode); use
    /// [`vacuum`](Self::vacuum) instead.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), MqdbError> {
        let path = path.as_ref();
        if let Some(storage) = self.storage.lock().unwrap().as_ref()
            && storage.same_backing_file(path)?
        {
            return Err(MqdbError::Storage(
                "save cannot target the store's own open backing file — use vacuum instead".into(),
            ));
        }
        self.save_inner(path, false).map(|_| ())
    }

    /// `write_lock_held` is used by migration, whose snapshot and replacement
    /// must be one writer-critical section rather than two separately locked
    /// operations.
    ///
    /// Returns the temp storage the new file was written through, still
    /// holding its inode lock, so [`vacuum`](Self::vacuum) can carry that
    /// lock into the reopened storage without a re-acquire gap.
    fn save_inner(&self, path: &Path, write_lock_held: bool) -> Result<Storage, MqdbError> {
        // `save` publishes via rename, so lock a stable sidecar for the
        // whole build-and-publish window.  An already-open store holds that
        // same lock for its lifetime; do not attempt to lock it twice.
        let save_lock = {
            let guard = self.storage.lock().unwrap();
            let already_locked = write_lock_held
                || match guard.as_ref() {
                    Some(storage) => storage.same_backing_file(path)?,
                    None => false,
                };
            if already_locked {
                None
            } else {
                Some(Storage::acquire_write_lock(path)?)
            }
        };
        let tmp_path = PathBuf::from(format!("{}.tmp", path.to_string_lossy()));
        if tmp_path.exists() {
            std::fs::remove_file(&tmp_path)?;
        }

        let write_result = (|| -> Result<Storage, MqdbError> {
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
                    std::borrow::Cow::Owned(DocumentIndex::build(&doc.blocks, self.tokenizer))
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

            // Same reasoning for vector indexes: load each (it may still be
            // in the old file) and write it into this one.
            let mut vector_entries = Vec::new();
            let states: Vec<(String, VectorIndexState)> = self
                .vector_indexes
                .read()
                .unwrap()
                .iter()
                .map(|(n, s)| (n.clone(), s.clone()))
                .collect();
            for (name, state) in states {
                let Some(index) = self.get_vector_index(&name)? else {
                    continue;
                };
                vector_entries.push(crate::storage::catalog::VectorIndexEntry {
                    name,
                    table: state.table,
                    column: state.column,
                    first_page: storage.write_index(&index.to_bytes())?,
                    table_rows: state.table_rows,
                });
            }
            vector_entries.sort_by(|a, b| a.name.cmp(&b.name));

            storage.flush_catalog(
                &entries,
                &custom,
                &self.content_hash_pairs(),
                &self.views_entries(),
                self.tokenizer,
                &vector_entries,
            )?;
            Ok(storage)
        })();

        let temp_storage = match write_result {
            Ok(storage) => storage,
            Err(err) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(err);
            }
        };

        std::fs::rename(&tmp_path, path)?;
        // A rename is only power-loss durable after the containing directory
        // is synced as well.
        std::fs::File::open(containing_dir(path))?.sync_all()?;
        drop(save_lock);
        Ok(temp_storage)
    }

    /// Rewrites the backing `.mq-db` file at `path` from scratch (same
    /// compaction [`save`](Self::save) already does), reclaiming dead page
    /// chains left behind by [`replace_document`](Self::replace_document),
    /// `DROP TABLE`/`DROP VIEW`, and re-indexing changed files. Requires a
    /// store opened via [`open`](Self::open) (a live backing file).
    pub fn vacuum(&mut self, path: impl AsRef<Path>) -> Result<VacuumReport, MqdbError> {
        let path = path.as_ref();
        let pages_before = {
            let guard = self.storage.lock().unwrap();
            let Some(storage) = guard.as_ref() else {
                return Err(MqdbError::Storage(
                    "vacuum requires a store opened from a file (DocumentStore::open) — \
                     this store has no backing file"
                        .into(),
                ));
            };
            if !storage.same_backing_file(path)? {
                return Err(MqdbError::Storage(format!(
                    "vacuum target {} is not the file this store was opened from — \
                     vacuum only rewrites its own backing file in place",
                    path.display()
                )));
            }
            storage.num_pages()
        };

        let replacement = self.save_inner(path, false)?;

        let previous = self
            .storage
            .lock()
            .unwrap()
            .take()
            .expect("vacuum checked that storage was open");
        let reopened = Storage::reopen_after_replace(path, previous, replacement)?;
        let pages_after = reopened.num_pages();
        *self.storage.lock().unwrap() = Some(reopened);

        // The replacement file wrote each table's rows to new pages, but the
        // in-memory page numbers still describe the old file. Forget them so
        // the next catalog flush rewrites the rows instead of publishing
        // pointers into pages that no longer hold them.
        for state in self.custom_tables.write().unwrap().values_mut() {
            state.first_row_page = 0;
            state.last_row_page = 0;
        }
        // Same for vector indexes; `save_inner` loaded them all into memory.
        for state in self.vector_indexes.write().unwrap().values_mut() {
            state.first_page = 0;
        }

        Ok(VacuumReport {
            pages_before,
            pages_after,
        })
    }

    /// Open a `.mq-db` file in lazy mode: reads only catalog and zone maps.
    ///
    /// Block data is not loaded until you call
    /// [`load_all_blocks`](DocumentStore::load_all_blocks).  Secondary indexes
    /// are not built until you call
    /// [`load_all_indexes`](DocumentStore::load_all_indexes).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MqdbError> {
        let mut storage = Storage::open(path.as_ref())?;
        // Unlike `load`/`load_catalog_only` (read-only, always rebuild
        // indexes from blocks), this mode keeps `storage` open for later
        // in-place writes, which only emit current-format index bytes;
        // mixed with a legacy file's untouched ones, that would corrupt it.
        if storage.file_version() != FILE_VERSION {
            return Err(MqdbError::Storage(format!(
                "store file is version {} (expected {FILE_VERSION}); run `DocumentStore::migrate` \
                 (or open it once via an interactive `mq-db` command, which offers to migrate) before opening it for writes",
                storage.file_version()
            )));
        }
        let (
            entries,
            custom_table_entries,
            content_hashes,
            view_entries,
            tokenizer,
            vector_index_entries,
        ) = storage.load_catalog()?;
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

        let mut custom_tables = FxHashMap::default();
        for ct in custom_table_entries {
            let rows = storage.read_table_rows(ct.first_row_page, ct.num_rows, ct.columns.len())?;
            custom_tables.insert(
                ct.name,
                CustomTableState {
                    columns: ct.columns,
                    rows,
                    first_row_page: ct.first_row_page,
                    last_row_page: ct.last_row_page,
                    not_null: vec![],
                    unique: vec![],
                },
            );
        }
        let views: FxHashMap<String, String> =
            view_entries.into_iter().map(|v| (v.name, v.sql)).collect();

        Ok(Self {
            documents,
            next_doc_id: max_doc_id.map_or(0, |id| id.saturating_add(1)),
            store_spans: true,
            tokenizer,
            storage: Mutex::new(Some(storage)),
            doc_indexes: vec![None; cap],
            custom_tables: RwLock::new(custom_tables),
            views: RwLock::new(views),
            vector_indexes: RwLock::new(states_from_entries(vector_index_entries)),
            attached: RwLock::new(FxHashMap::default()),
            content_hashes: content_hashes.into_iter().collect(),
            tx_snapshot: Mutex::new(None),
            catalog_commit: Mutex::new(()),
            write_session: DocumentStore::new_session_id(),
        })
    }

    /// Load a `.mq-db` file and reconstruct the in-memory `DocumentStore`.
    ///
    /// All block data is read from disk. Secondary indexes are **not** built
    /// here: [`crate::SqlEngine`] builds them lazily on construction.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, MqdbError> {
        let mut storage = Storage::open_read_only(path.as_ref())?;
        let (
            entries,
            custom_table_entries,
            content_hashes,
            view_entries,
            tokenizer,
            vector_index_entries,
        ) = storage.load_catalog()?;
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

        let mut custom_tables = FxHashMap::default();
        for ct in custom_table_entries {
            let rows = storage.read_table_rows(ct.first_row_page, ct.num_rows, ct.columns.len())?;
            custom_tables.insert(
                ct.name,
                CustomTableState {
                    columns: ct.columns,
                    rows,
                    first_row_page: ct.first_row_page,
                    last_row_page: ct.last_row_page,
                    not_null: vec![],
                    unique: vec![],
                },
            );
        }
        let views: FxHashMap<String, String> =
            view_entries.into_iter().map(|v| (v.name, v.sql)).collect();
        // A read-only load drops the file handle, so read the indexes now.
        let mut vector_states = states_from_entries(vector_index_entries);
        for state in vector_states.values_mut() {
            state.index = Some(std::sync::Arc::new(crate::vector::HnswIndex::from_bytes(
                &storage.read_index_bytes(state.first_page)?,
            )?));
        }

        Ok(Self {
            documents,
            next_doc_id: max_doc_id.map_or(0, |id| id.saturating_add(1)),
            store_spans: true,
            tokenizer,
            storage: Mutex::new(None),
            doc_indexes: vec![None; cap],
            custom_tables: RwLock::new(custom_tables),
            views: RwLock::new(views),
            vector_indexes: RwLock::new(vector_states),
            attached: RwLock::new(FxHashMap::default()),
            content_hashes: content_hashes.into_iter().collect(),
            tx_snapshot: Mutex::new(None),
            catalog_commit: Mutex::new(()),
            write_session: DocumentStore::new_session_id(),
        })
    }

    /// Load only the catalog metadata from a `.mq-db` file, no block data.
    ///
    /// Documents have `block_count` populated from the catalog but `blocks`
    /// is empty. Useful for commands that only need zone-map metadata (e.g.
    /// `list`), avoiding the cost of deserialising all block data.
    pub fn load_catalog_only(path: impl AsRef<Path>) -> Result<Self, MqdbError> {
        let mut storage = Storage::open_read_only(path.as_ref())?;
        let (
            entries,
            _custom_table_entries,
            content_hashes,
            _view_entries,
            tokenizer,
            _vector_indexes,
        ) = storage.load_catalog()?;
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
            tokenizer,
            storage: Mutex::new(None),
            doc_indexes: vec![None; cap],
            custom_tables: RwLock::new(FxHashMap::default()),
            views: RwLock::new(FxHashMap::default()),
            vector_indexes: RwLock::new(FxHashMap::default()),
            attached: RwLock::new(FxHashMap::default()),
            content_hashes: content_hashes.into_iter().collect(),
            tx_snapshot: Mutex::new(None),
            catalog_commit: Mutex::new(()),
            write_session: DocumentStore::new_session_id(),
        })
    }

    /// The on-disk file-format version at `path`, without loading the
    /// catalog. Use this to decide whether [`DocumentStore::migrate`] is
    /// needed before calling [`DocumentStore::open`].
    pub fn file_version(path: impl AsRef<Path>) -> Result<u32, MqdbError> {
        Ok(Storage::open_read_only(path.as_ref())?.file_version())
    }

    /// Rewrites a store written by an older but still-recognised file
    /// format (see `storage::page`) in the current format, rebuilding every
    /// document's secondary index (including any index sections added since
    /// that version) from its stored blocks. Returns the version the file
    /// was in before migrating; a no-op if it was already current.
    ///
    /// This does not touch the original bytes in place, like [`Self::save`],
    /// it writes a new file to `<path>.tmp` and renames it over `path`, so a
    /// failure or interruption midway leaves the original file untouched.
    /// Callers that want a backup of the pre-migration file should copy it
    /// before calling this.
    pub fn migrate(path: impl AsRef<Path>) -> Result<u32, MqdbError> {
        let path = path.as_ref();
        // Keep the writer lock from reading the old snapshot through the
        // final rename, otherwise another writer could be silently replaced.
        let _migration_lock = Storage::acquire_write_lock(path)?;
        let old_version = Storage::open_read_only(path)?.file_version();
        if old_version == FILE_VERSION {
            return Ok(old_version);
        }
        let store = Self::load(path)?;
        store.save_inner(path, true)?;
        Ok(old_version)
    }
}
