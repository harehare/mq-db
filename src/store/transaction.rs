use super::vector_indexes::persist_unsaved_vector_indexes;
use super::*;

impl DocumentStore {
    /// Starts a transaction owned by `session`: snapshots the documents,
    /// custom tables, views and vector indexes so
    /// [`rollback_transaction`](DocumentStore::rollback_transaction) can
    /// restore them. Errors if one is already open.
    pub(crate) fn begin_transaction(&self, session: u64) -> Result<(), MqdbError> {
        // Snapshot must not observe a writer's half-applied mutation.
        let _catalog_guard = self.catalog_commit.lock().unwrap();
        let mut guard = self.tx_snapshot.lock().unwrap();
        if guard.is_some() {
            return Err(MqdbError::SqlExec(
                "a transaction is already in progress".into(),
            ));
        }
        *guard = Some(TxSnapshot {
            documents: self.documents.clone(),
            custom_tables: self.custom_tables.read().unwrap().clone(),
            views: self.views.read().unwrap().clone(),
            vector_indexes: self.vector_indexes.read().unwrap().clone(),
            content_hashes: self.content_hashes.clone(),
            owner: session,
            external_write: false,
        });
        Ok(())
    }

    pub(crate) fn commit_transaction(&self) -> Result<(), MqdbError> {
        if self.tx_snapshot.lock().unwrap().take().is_none() {
            return Err(MqdbError::SqlExec("no transaction is in progress".into()));
        }
        Ok(())
    }

    /// Called by a catalog-mutating executor after it has actually changed
    /// and durably flushed the catalog. Callers must still hold
    /// `catalog_commit`, so rollback can't slip in between the commit and
    /// this mark. Flags an open transaction owned by another session so its
    /// rollback refuses to overwrite this commit. A no-op for the owner's own
    /// mutations: its rollback is expected to undo those.
    pub(crate) fn note_catalog_mutation(&self, session: u64) {
        if let Some(tx) = self.tx_snapshot.lock().unwrap().as_mut()
            && tx.owner != session
        {
            tx.external_write = true;
        }
    }

    pub(crate) fn rollback_transaction_tables_only(&self) -> Result<(), MqdbError> {
        // Guard the whole snapshot-take-and-restore, or a concurrent commit
        // in the gap gets silently overwritten by this restore.
        let _catalog_guard = self.catalog_commit.lock().unwrap();
        let snapshot = self
            .tx_snapshot
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| MqdbError::SqlExec("no transaction is in progress".into()))?;
        if snapshot.external_write {
            return Err(MqdbError::SqlExec(
                "rollback aborted: another session committed catalog changes during this transaction; \
                 the transaction has been closed without reverting the catalog to avoid discarding them"
                    .into(),
            ));
        }
        *self.custom_tables.write().unwrap() = snapshot.custom_tables;
        *self.views.write().unwrap() = snapshot.views;
        *self.vector_indexes.write().unwrap() = snapshot.vector_indexes;
        self.flush_catalog_to_storage()?;
        Ok(())
    }

    pub(crate) fn rollback_transaction(&mut self) -> Result<Vec<String>, MqdbError> {
        // See `rollback_transaction_tables_only` for why the guard covers
        // the whole snapshot-take-and-restore.
        let _catalog_guard = self.catalog_commit.lock().unwrap();
        let snapshot = self
            .tx_snapshot
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| MqdbError::SqlExec("no transaction is in progress".into()))?;
        if snapshot.external_write {
            return Err(MqdbError::SqlExec(
                "rollback aborted: another session committed catalog changes during this transaction; \
                 the transaction has been closed without reverting to avoid discarding them"
                    .into(),
            ));
        }
        let unrevertable_paths = diff_written_paths(&snapshot.documents, &self.documents);
        self.documents = snapshot.documents;
        self.doc_indexes = vec![None; self.documents.len()];
        self.content_hashes = snapshot.content_hashes;
        *self.custom_tables.write().unwrap() = snapshot.custom_tables;
        *self.views.write().unwrap() = snapshot.views;
        *self.vector_indexes.write().unwrap() = snapshot.vector_indexes;
        self.flush_catalog_to_storage()?;
        Ok(unrevertable_paths)
    }

    /// Publish the complete in-memory metadata (including custom tables, views
    /// and vector indexes) as a durable COW catalog, if a backing storage file
    /// is open. Called after DDL such as `CREATE TABLE` and `DROP TABLE`.
    /// No-op for in-memory stores. Errors are deliberately propagated:
    /// reporting success after a failed commit is worse than leaving the
    /// caller to retry.
    ///
    /// Any table whose rows, or index whose blob, has never been persisted is
    /// written out in full here (a one-time cost). Tables already backed by a
    /// row-page chain keep their existing pages untouched; see
    /// [`append_table_rows_to_storage`](DocumentStore::append_table_rows_to_storage)
    /// for the incremental `INSERT` path.
    pub(crate) fn flush_catalog_to_storage(&self) -> Result<(), MqdbError> {
        let mut guard = self.storage.lock().unwrap();
        if let Some(storage) = guard.as_mut() {
            let entries = self.catalog_entries();
            let custom = persist_unsaved_table_rows(storage, &self.custom_tables)?;
            let vector = persist_unsaved_vector_indexes(storage, &self.vector_indexes)?;
            storage.flush_catalog(
                &entries,
                &custom,
                &self.content_hash_pairs(),
                &self.views_entries(),
                self.tokenizer,
                &vector,
            )?;
        }
        Ok(())
    }

    /// Append `new_rows` to `table_name`'s on-disk row chain and flush a
    /// lightweight catalog update, no full row rewrite. No-op for in-memory
    /// stores or unknown tables.
    ///
    /// This is what makes `INSERT INTO <table>` incremental: the cost is
    /// proportional to the rows being inserted, not to the table's total size.
    pub(crate) fn append_table_rows_to_storage(
        &self,
        table_name: &str,
        _new_rows: &[Vec<String>],
    ) -> Result<(), MqdbError> {
        let mut guard = self.storage.lock().unwrap();
        let storage = match guard.as_mut() {
            Some(s) => s,
            None => return Ok(()),
        };

        {
            let mut ct_guard = self.custom_tables.write().unwrap();
            if let Some(state) = ct_guard.get_mut(table_name) {
                // A table chain's tail link used to be patched in place here.
                // That makes an uncommitted append reachable after a crash.
                // Until table pages gain their own immutable segment map, make
                // the chain COW as well and publish it only with the catalog.
                let (first, last) = storage.write_table_rows(&state.rows)?;
                state.first_row_page = first;
                state.last_row_page = last;
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
        let vector = persist_unsaved_vector_indexes(storage, &self.vector_indexes)?;
        storage.flush_catalog(
            &entries,
            &custom,
            &self.content_hash_pairs(),
            &self.views_entries(),
            self.tokenizer,
            &vector,
        )?;
        Ok(())
    }

    pub(crate) fn rollback_appended_rows(&self, table_name: &str, count: usize) {
        if count == 0 {
            return;
        }
        let mut guard = self.custom_tables.write().unwrap();
        if let Some(state) = guard.get_mut(table_name) {
            let new_len = state.rows.len().saturating_sub(count);
            state.rows.truncate(new_len);
        }
    }

    // Persistence
}
