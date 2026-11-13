//! Vector (HNSW) indexes over custom-table columns: in-memory state, lazy
//! loading from the backing file, and what is persisted in the catalog.

use super::*;
use crate::storage::catalog::VectorIndexEntry;
use crate::vector::{HnswIndex, HnswParams, parse_vector_text};
use std::sync::Arc;

/// One named index. `index` is `None` until first used when the store was
/// opened lazily from a file; `first_page` is 0 until the blob has been
/// written to the backing file.
#[derive(Clone)]
pub(crate) struct VectorIndexState {
    pub table: String,
    pub column: String,
    /// Row count of the table when the index was built.
    pub table_rows: u32,
    pub first_page: u32,
    pub index: Option<Arc<HnswIndex>>,
}

/// Writes every index blob not yet in `storage`, then returns the catalog
/// entries for all indexes.
pub(super) fn persist_unsaved_vector_indexes(
    storage: &mut Storage,
    indexes: &RwLock<FxHashMap<String, VectorIndexState>>,
) -> Result<Vec<VectorIndexEntry>, MqdbError> {
    let mut guard = indexes.write().unwrap();
    for state in guard.values_mut() {
        if state.first_page == 0
            && let Some(index) = &state.index
        {
            state.first_page = storage.write_index(&index.to_bytes())?;
        }
    }
    Ok(entries_of(&guard))
}

fn entries_of(map: &FxHashMap<String, VectorIndexState>) -> Vec<VectorIndexEntry> {
    let mut entries: Vec<VectorIndexEntry> = map
        .iter()
        .filter(|(_, s)| s.first_page != 0)
        .map(|(name, s)| VectorIndexEntry {
            name: name.clone(),
            table: s.table.clone(),
            column: s.column.clone(),
            first_page: s.first_page,
            table_rows: s.table_rows,
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// States for catalog entries read from a file, to be loaded on first use.
pub(super) fn states_from_entries(
    entries: Vec<VectorIndexEntry>,
) -> FxHashMap<String, VectorIndexState> {
    entries
        .into_iter()
        .map(|e| {
            (
                e.name,
                VectorIndexState {
                    table: e.table,
                    column: e.column,
                    table_rows: e.table_rows,
                    first_page: e.first_page,
                    index: None,
                },
            )
        })
        .collect()
}

impl DocumentStore {
    /// Loads the index named `name`, reading it from the backing file the
    /// first time. `Ok(None)` if there is no such index.
    pub(crate) fn get_vector_index(&self, name: &str) -> Result<Option<Arc<HnswIndex>>, MqdbError> {
        let first_page = {
            let guard = self.vector_indexes.read().unwrap();
            let Some(state) = guard.get(name) else {
                return Ok(None);
            };
            if let Some(index) = &state.index {
                return Ok(Some(index.clone()));
            }
            state.first_page
        };
        let mut storage_guard = self.storage.lock().unwrap();
        let storage = storage_guard.as_mut().ok_or_else(|| {
            MqdbError::Storage(format!(
                "vector index '{name}' is not loaded and the store has no backing file"
            ))
        })?;
        let index = Arc::new(HnswIndex::from_bytes(
            &storage.read_index_bytes(first_page)?,
        )?);
        if let Some(state) = self.vector_indexes.write().unwrap().get_mut(name) {
            state.index = Some(index.clone());
        }
        Ok(Some(index))
    }

    /// Name of a current index on `table.column`, if any. An index built when
    /// the table had a different number of rows is stale and is ignored.
    pub(crate) fn find_vector_index(&self, table: &str, column: &str) -> Option<String> {
        let rows = self.custom_tables.read().unwrap().get(table)?.rows.len();
        self.vector_indexes
            .read()
            .unwrap()
            .iter()
            .filter(|(_, s)| {
                s.table == table
                    && s.column.eq_ignore_ascii_case(column)
                    && s.table_rows as usize == rows
            })
            .map(|(name, _)| name.clone())
            .min()
    }

    /// Whether an index called `name` exists, stale or not.
    pub(crate) fn has_vector_index(&self, name: &str) -> bool {
        self.vector_indexes.read().unwrap().contains_key(name)
    }

    /// Builds an HNSW index over `table.column`, whose values are JSON arrays
    /// of numbers. The first valid vector fixes the dimension; rows whose
    /// value is not a vector of that length are left out (and counted).
    /// Returns the index, the number of rows skipped, and the table's row
    /// count.
    pub(crate) fn build_vector_index(
        &self,
        table: &str,
        column: &str,
        params: HnswParams,
    ) -> Result<(HnswIndex, usize, u32), MqdbError> {
        let tables = self.custom_tables.read().unwrap();
        let state = tables
            .get(table)
            .ok_or_else(|| MqdbError::SqlExec(format!("unknown table: {table}")))?;
        let col = state
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(column))
            .ok_or_else(|| {
                MqdbError::SqlExec(format!("table '{table}' has no column '{column}'"))
            })?;
        let mut index: Option<HnswIndex> = None;
        let mut skipped = 0usize;
        for (row_no, row) in state.rows.iter().enumerate() {
            let Some(vec) = row.get(col).and_then(|v| parse_vector_text(v)) else {
                skipped += 1;
                continue;
            };
            let idx = index.get_or_insert_with(|| HnswIndex::new(vec.len(), params));
            if !idx.insert(row_no as u32, &vec) {
                skipped += 1;
            }
        }
        let index = index.filter(|i| !i.is_empty()).ok_or_else(|| {
            MqdbError::SqlExec(format!(
                "'{table}.{column}' holds no vectors (expected JSON arrays such as [0.1, 0.2])"
            ))
        })?;
        Ok((index, skipped, state.rows.len() as u32))
    }

    /// Registers a built index. It is written to the backing file, if any,
    /// by the next catalog flush.
    pub(crate) fn put_vector_index(
        &self,
        name: String,
        table: String,
        column: String,
        table_rows: u32,
        index: HnswIndex,
    ) {
        self.vector_indexes.write().unwrap().insert(
            name,
            VectorIndexState {
                table,
                column,
                table_rows,
                first_page: 0,
                index: Some(Arc::new(index)),
            },
        );
    }

    /// Removes the index `name`. Returns whether it existed.
    pub(crate) fn drop_vector_index(&self, name: &str) -> bool {
        self.vector_indexes.write().unwrap().remove(name).is_some()
    }

    /// Removes every index on `table` (its rows changed or it was dropped).
    /// Returns whether any existed.
    pub(crate) fn drop_vector_indexes_for_table(&self, table: &str) -> bool {
        let mut guard = self.vector_indexes.write().unwrap();
        let before = guard.len();
        guard.retain(|_, s| s.table != table);
        guard.len() != before
    }

    /// `(name, table, column, current)` of every index, sorted by name.
    /// `current` is false once the table's row count no longer matches the
    /// count the index was built from.
    pub(crate) fn vector_index_list(&self) -> Vec<(String, String, String, bool)> {
        let tables = self.custom_tables.read().unwrap();
        let mut list: Vec<_> = self
            .vector_indexes
            .read()
            .unwrap()
            .iter()
            .map(|(n, s)| {
                let current = tables
                    .get(&s.table)
                    .is_some_and(|t| t.rows.len() == s.table_rows as usize);
                (n.clone(), s.table.clone(), s.column.clone(), current)
            })
            .collect();
        list.sort();
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqlEngine;

    fn vector_text(i: usize) -> String {
        // Distinct directions on a circle in 2-D, padded to 4-D.
        let a = i as f32 * 0.37;
        format!("[{}, {}, 0.5, 0.25]", a.cos(), a.sin())
    }

    /// A file-backed store whose table `emb(id, v)` has `n` vectors, opened
    /// lazily like the CLI does.
    fn store_with_table(n: usize) -> (tempfile::TempDir, std::path::PathBuf, DocumentStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.mq-db");
        let mut s = DocumentStore::new();
        s.add_str("# T\n\nbody\n").unwrap();
        s.save(&path).unwrap();
        let s = DocumentStore::open(&path).unwrap();
        let e = SqlEngine::new(&s).unwrap();
        e.execute("CREATE TABLE emb (id TEXT, v TEXT)").unwrap();
        if n > 0 {
            let values: Vec<String> = (0..n)
                .map(|i| format!("('{i}', '{}')", vector_text(i)))
                .collect();
            e.execute(&format!("INSERT INTO emb VALUES {}", values.join(", ")))
                .unwrap();
        }
        drop(e);
        (dir, path, s)
    }

    fn index_it(s: &DocumentStore, name: &str) {
        let (index, skipped, rows) = s
            .build_vector_index("emb", "v", HnswParams::default())
            .unwrap();
        assert_eq!(skipped, 0);
        s.put_vector_index(name.into(), "emb".into(), "v".into(), rows, index);
        s.flush_catalog_to_storage().unwrap();
    }

    fn nearest(s: &DocumentStore, name: &str, i: usize) -> u32 {
        let idx = s.get_vector_index(name).unwrap().unwrap();
        let q = crate::vector::parse_vector_text(&vector_text(i)).unwrap();
        idx.search(&q, 1, 32)[0].0
    }

    #[test]
    fn index_survives_reopen_and_loads_lazily() {
        let (_dir, path, s) = store_with_table(60);
        index_it(&s, "emb_hnsw");
        drop(s);

        let r = DocumentStore::open(&path).unwrap();
        assert_eq!(
            r.vector_index_list(),
            vec![("emb_hnsw".into(), "emb".into(), "v".into(), true)]
        );
        assert!(
            r.vector_indexes.read().unwrap()["emb_hnsw"].index.is_none(),
            "not read until used"
        );
        assert_eq!(nearest(&r, "emb_hnsw", 17), 17);
        assert!(r.vector_indexes.read().unwrap()["emb_hnsw"].index.is_some());
        assert_eq!(r.find_vector_index("emb", "V").as_deref(), Some("emb_hnsw"));
    }

    #[test]
    fn read_only_load_reads_the_index_eagerly() {
        let (_dir, path, s) = store_with_table(30);
        index_it(&s, "emb_hnsw");
        drop(s);
        let r = DocumentStore::load(&path).unwrap();
        assert_eq!(nearest(&r, "emb_hnsw", 5), 5);
    }

    #[test]
    fn index_is_stale_once_the_row_count_differs() {
        let (_dir, _path, s) = store_with_table(20);
        index_it(&s, "emb_hnsw");
        assert!(s.find_vector_index("emb", "v").is_some());
        // Changed behind the store's back (no SQL hook drops the index).
        s.custom_tables
            .write()
            .unwrap()
            .get_mut("emb")
            .unwrap()
            .rows
            .push(vec!["x".into(), "[1,0,0,0]".into()]);
        assert_eq!(s.find_vector_index("emb", "v"), None);
        assert!(s.has_vector_index("emb_hnsw"), "still listed until dropped");
        assert!(!s.vector_index_list()[0].3, "reported as not current");
    }

    #[test]
    fn sql_writes_drop_the_index_outright() {
        let (_dir, _path, s) = store_with_table(20);
        index_it(&s, "emb_hnsw");
        SqlEngine::new(&s)
            .unwrap()
            .execute("INSERT INTO emb VALUES ('x', '[1,0,0,0]')")
            .unwrap();
        assert!(!s.has_vector_index("emb_hnsw"));
    }

    #[test]
    fn build_skips_invalid_rows_and_rejects_a_column_without_vectors() {
        let (_dir, _path, s) = store_with_table(0);
        let e = SqlEngine::new(&s).unwrap();
        e.execute("INSERT INTO emb VALUES ('a', '[1,0]'), ('b', 'oops'), ('c', '[1,0,0]'), ('d', '[0,0]'), ('e', '[0,1]')")
            .unwrap();
        let (index, skipped, rows) = s
            .build_vector_index("emb", "v", HnswParams::default())
            .unwrap();
        assert_eq!((index.len(), skipped, rows), (2, 3, 5));
        assert_eq!(index.dim(), 2);

        let err = s
            .build_vector_index("emb", "id", HnswParams::default())
            .err()
            .unwrap();
        assert!(err.to_string().contains("no vectors"));
        assert!(
            s.build_vector_index("emb", "nope", HnswParams::default())
                .is_err()
        );
        assert!(
            s.build_vector_index("nope", "v", HnswParams::default())
                .is_err()
        );
    }

    #[test]
    fn index_is_carried_into_a_saved_copy() {
        let (dir, _path, s) = store_with_table(40);
        index_it(&s, "emb_hnsw");
        let copy = dir.path().join("copy.mq-db");
        s.save(&copy).unwrap();
        let r = DocumentStore::open(&copy).unwrap();
        assert_eq!(nearest(&r, "emb_hnsw", 9), 9);
    }

    #[test]
    fn index_survives_vacuum_and_a_following_write() {
        let (_dir, path, s) = store_with_table(40);
        index_it(&s, "emb_hnsw");
        drop(s);
        let mut s = DocumentStore::open(&path).unwrap();
        s.load_all_blocks().unwrap();
        s.load_all_indexes().unwrap();
        s.vacuum(&path).unwrap();
        // A catalog-only write after vacuum must not publish stale pages.
        SqlEngine::new(&s)
            .unwrap()
            .execute("CREATE VIEW v1 AS SELECT 1")
            .unwrap();
        drop(s);
        let r = DocumentStore::open(&path).unwrap();
        assert_eq!(nearest(&r, "emb_hnsw", 21), 21);
        assert!(r.find_vector_index("emb", "v").is_some());
    }

    #[test]
    fn rollback_restores_the_index_set() {
        let (_dir, _path, mut s) = store_with_table(20);
        index_it(&s, "keep");
        s.begin_transaction(s.write_session).unwrap();
        s.drop_vector_index("keep");
        let (index, _, rows) = s
            .build_vector_index("emb", "v", HnswParams::default())
            .unwrap();
        s.put_vector_index("temp".into(), "emb".into(), "v".into(), rows, index);
        s.rollback_transaction().unwrap();
        let names: Vec<String> = s.vector_index_list().into_iter().map(|t| t.0).collect();
        assert_eq!(names, vec!["keep".to_string()]);
        assert_eq!(nearest(&s, "keep", 3), 3);
    }

    #[test]
    fn dropping_a_table_removes_its_indexes_only() {
        let (_dir, _path, s) = store_with_table(10);
        index_it(&s, "emb_hnsw");
        assert!(!s.drop_vector_indexes_for_table("other"));
        assert!(s.drop_vector_indexes_for_table("emb"));
        assert!(s.vector_index_list().is_empty());
        assert!(!s.drop_vector_index("emb_hnsw"));
    }

    #[test]
    fn catalog_without_vector_indexes_is_unchanged_and_with_them_round_trips() {
        use crate::storage::catalog::VectorIndexEntry;
        let (_dir, path, s) = store_with_table(10);
        // No indexes: nothing is written for the section.
        s.flush_catalog_to_storage().unwrap();
        drop(s);
        assert!(
            DocumentStore::open(&path)
                .unwrap()
                .vector_index_list()
                .is_empty()
        );

        let entry = VectorIndexEntry {
            name: "n".into(),
            table: "t".into(),
            column: "c".into(),
            first_page: 7,
            table_rows: 3,
        };
        let mut states = FxHashMap::default();
        states.extend(states_from_entries(vec![entry.clone()]));
        assert_eq!(entries_of(&states), vec![entry]);
    }
}
