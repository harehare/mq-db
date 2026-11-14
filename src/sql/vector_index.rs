//! `CREATE INDEX ... USING hnsw`, `DROP INDEX` and the `vector_search()`
//! table function.

use super::engine::SqlEngine;
use super::output::QueryOutput;
use super::plan::{expr_int_val, expr_str_val};
use super::rows::{qualify_row, require_unqualified};
use super::value::{Row, Value};
use crate::MqdbError;
use crate::vector::{HnswParams, normalized, parse_vector_text};
use sqlparser::ast::{
    BinaryOperator, CreateIndex, Expr, FunctionArg, FunctionArgExpr, IndexType, ObjectName,
    TableFunctionArgs,
};

/// Most rows `vector_search` will return.
const MAX_K: i64 = 100_000;

fn err(msg: impl Into<String>) -> MqdbError {
    MqdbError::SqlExec(msg.into())
}

/// Reads `WITH (m = 16, ef_construction = 200)`.
fn index_params(with: &[Expr]) -> Result<HnswParams, MqdbError> {
    let mut params = HnswParams::default();
    for option in with {
        let Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } = option
        else {
            return Err(err("index options must look like `m = 16`"));
        };
        let Expr::Identifier(name) = left.as_ref() else {
            return Err(err("index option name must be an identifier"));
        };
        let value = expr_int_val(right)
            .filter(|v| (1..=10_000).contains(v))
            .ok_or_else(|| {
                err(format!(
                    "index option '{}' must be an integer between 1 and 10000",
                    name.value
                ))
            })? as usize;
        match name.value.to_lowercase().as_str() {
            "m" if value <= 128 => params.m = value,
            "m" => return Err(err("index option 'm' must be at most 128")),
            "ef_construction" => params.ef_construction = value,
            other => {
                return Err(err(format!(
                    "unknown index option '{other}' (supported: m, ef_construction)"
                )));
            }
        }
    }
    Ok(params)
}

impl SqlEngine<'_> {
    pub(super) fn exec_create_index(&self, ci: &CreateIndex) -> Result<QueryOutput, MqdbError> {
        let is_hnsw = matches!(
            &ci.using,
            Some(IndexType::Custom(ident)) if ident.value.eq_ignore_ascii_case("hnsw")
        );
        if !is_hnsw {
            return Err(err(
                "only `CREATE INDEX ... USING hnsw (column)` is supported (a vector index on a custom table column)",
            ));
        }
        if ci.unique || ci.predicate.is_some() || !ci.include.is_empty() {
            return Err(err(
                "UNIQUE, WHERE and INCLUDE are not supported for hnsw indexes",
            ));
        }
        let [column] = ci.columns.as_slice() else {
            return Err(err("an hnsw index covers exactly one column"));
        };
        if let Some(class) = &column.operator_class
            && class.to_string().to_lowercase() != "vector_cosine_ops"
        {
            return Err(err(
                "only cosine similarity is supported (operator class vector_cosine_ops)",
            ));
        }
        let Expr::Identifier(column) = &column.column.expr else {
            return Err(err("an hnsw index column must be a plain column name"));
        };
        let column = column.value.clone();
        let table = require_unqualified(&ci.table_name)?;
        if matches!(table.as_str(), "blocks" | "documents" | "links") {
            return Err(err(format!(
                "cannot index built-in table '{table}'; vector indexes are for custom tables (see `mq-db embed`)"
            )));
        }
        let name = match &ci.name {
            Some(n) => require_unqualified(n)?,
            None => format!("{table}_{}_hnsw", column.to_lowercase()),
        };
        let params = index_params(&ci.with)?;

        let _catalog_guard = self.store.catalog_commit.lock().unwrap();
        if self.store.has_vector_index(&name) {
            if ci.if_not_exists {
                return Ok(QueryOutput {
                    columns: vec!["result".to_string()],
                    rows: vec![vec!["already exists".to_string()]],
                });
            }
            return Err(err(format!("index '{name}' already exists")));
        }
        let (index, skipped, table_rows) =
            self.store.build_vector_index(&table, &column, params)?;
        let summary = format!(
            "ok ({} vectors, {} dimensions{})",
            index.len(),
            index.dim(),
            if skipped > 0 {
                format!(", {skipped} rows skipped: not a vector of that length")
            } else {
                String::new()
            }
        );
        self.store
            .put_vector_index(name.clone(), table, column, table_rows, index);
        if let Err(e) = self.store.flush_catalog_to_storage() {
            self.store.drop_vector_index(&name);
            return Err(e);
        }
        self.store.note_catalog_mutation(self.session_id);
        Ok(QueryOutput {
            columns: vec!["result".to_string()],
            rows: vec![vec![summary]],
        })
    }

    pub(super) fn exec_drop_indexes(
        &self,
        names: &[ObjectName],
        if_exists: bool,
    ) -> Result<QueryOutput, MqdbError> {
        let names: Vec<String> = names
            .iter()
            .map(require_unqualified)
            .collect::<Result<_, _>>()?;
        let _catalog_guard = self.store.catalog_commit.lock().unwrap();
        if !if_exists && let Some(missing) = names.iter().find(|n| !self.store.has_vector_index(n))
        {
            return Err(err(format!("index '{missing}' does not exist")));
        }
        let removed = self.store.take_vector_indexes(&names);
        let dropped = removed.len();
        if dropped > 0 {
            if let Err(e) = self.store.flush_catalog_to_storage() {
                self.store.restore_vector_indexes(removed);
                return Err(e);
            }
            self.store.note_catalog_mutation(self.session_id);
        }
        Ok(QueryOutput {
            columns: vec!["result".to_string()],
            rows: vec![vec![format!("{dropped} index(es) dropped")]],
        })
    }

    /// `vector_search(table, column, '[query vector]', k [, ef])`: the `k`
    /// rows of `table` whose `column` is most cosine-similar to the query,
    /// with an extra `score` column, most similar first. Uses the table's
    /// hnsw index when it has a current one (approximate; `ef` trades speed
    /// for accuracy), otherwise compares every row exactly.
    pub(super) fn vector_search_rows(
        &self,
        args: &TableFunctionArgs,
        prefix: &str,
    ) -> Result<Vec<Row>, MqdbError> {
        const USAGE: &str = "vector_search(table, column, '[query vector]', k [, ef])";
        let exprs: Vec<&Expr> = args
            .args
            .iter()
            .map(|a| match a {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Ok(e),
                _ => Err(err(format!("usage: {USAGE}"))),
            })
            .collect::<Result<_, _>>()?;
        if !(4..=5).contains(&exprs.len()) {
            return Err(err(format!("usage: {USAGE}")));
        }
        let text = |i: usize, what: &str| {
            match exprs[i] {
                Expr::Identifier(id) => Some(ident_value_of(id)),
                e => expr_str_val(e),
            }
            .ok_or_else(|| err(format!("vector_search: {what} must be a string literal")))
        };
        let table = text(0, "table")?.to_lowercase();
        let column = text(1, "column")?;
        let query = parse_vector_text(&text(2, "the query vector")?).ok_or_else(|| {
            err("vector_search: the query must be a JSON array of numbers, e.g. '[0.1, 0.2]'")
        })?;
        let k = expr_int_val(exprs[3])
            .filter(|k| (1..=MAX_K).contains(k))
            .ok_or_else(|| {
                err(format!(
                    "vector_search: k must be an integer from 1 to {MAX_K}"
                ))
            })? as usize;
        let ef = match exprs.get(4) {
            Some(e) => expr_int_val(e)
                .filter(|v| (1..=1_000_000).contains(v))
                .ok_or_else(|| err("vector_search: ef must be a positive integer"))?
                as usize,
            None => (k * 8).max(64),
        };
        if normalized(&query).is_none() {
            return Err(err(
                "vector_search: the query vector is empty, all zeros or not finite",
            ));
        }

        // Loading an index takes the storage lock, which `INSERT` holds while
        // waiting for the table lock, so do it before taking the table lock.
        let index = match self.store.find_vector_index(&table, &column) {
            Some(name) => self.store.get_vector_index(&name)?,
            None => None,
        };

        let tables = self.store.custom_tables.read().unwrap();
        let state = tables
            .get(&table)
            .ok_or_else(|| err(format!("vector_search: unknown table '{table}'")))?;
        if state
            .columns
            .iter()
            .any(|c| c.eq_ignore_ascii_case("score"))
        {
            return Err(err(format!(
                "vector_search: table '{table}' has a column named 'score', which would hide the similarity score"
            )));
        }
        let col = state
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(&column))
            .ok_or_else(|| {
                err(format!(
                    "vector_search: table '{table}' has no column '{column}'"
                ))
            })?;

        let hits: Vec<(usize, f32)> = match index {
            Some(index) if index.dim() == query.len() => index
                .search(&query, k, ef)
                .into_iter()
                .map(|(row, score)| (row as usize, score))
                .filter(|(row, _)| *row < state.rows.len())
                .collect(),
            // A query of another length can never match this index.
            Some(_) => Vec::new(),
            None => exact_search(&state.rows, col, &query, k),
        };

        let mut columns: Vec<String> = state.columns.clone();
        columns.push("score".to_string());
        let columns: std::sync::Arc<[String]> = columns.into();
        Ok(hits
            .into_iter()
            .map(|(row, score)| {
                let mut values: Vec<Value> = state.rows[row]
                    .iter()
                    .map(|c| Value::Str(c.clone()))
                    .collect();
                values.push(Value::Float(f64::from(score)));
                qualify_row(
                    Row {
                        columns: columns.clone(),
                        values,
                    },
                    prefix,
                )
            })
            .collect())
    }

    /// `SHOW INDEXES`: every vector index and whether it is current.
    pub(super) fn exec_show_indexes(&self) -> Result<QueryOutput, MqdbError> {
        Ok(QueryOutput {
            columns: ["name", "table", "column", "type", "status"]
                .map(String::from)
                .to_vec(),
            rows: self
                .store
                .vector_index_list()
                .into_iter()
                .map(|(name, table, column, current)| {
                    vec![
                        name,
                        table,
                        column,
                        "hnsw".to_string(),
                        if current { "ready" } else { "stale" }.to_string(),
                    ]
                })
                .collect(),
        })
    }
}

fn ident_value_of(id: &sqlparser::ast::Ident) -> String {
    id.value.clone()
}

/// Brute-force top-`k` by cosine similarity; rows whose value is not a
/// vector of the query's length are skipped.
fn exact_search(rows: &[Vec<String>], col: usize, query: &[f32], k: usize) -> Vec<(usize, f32)> {
    let Some(q) = normalized(query) else {
        return Vec::new();
    };
    let mut scored: Vec<(usize, f32)> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, row)| {
            let v = normalized(&parse_vector_text(row.get(col)?)?)?;
            (v.len() == q.len()).then(|| (i, crate::vector::dot(&v, &q)))
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.truncate(k);
    scored
}

#[cfg(test)]
mod tests {
    use crate::{DocumentStore, SqlEngine};

    fn vec_text(i: usize) -> String {
        let a = i as f32 * 0.37;
        format!(
            "[{:.5}, {:.5}, {:.5}]",
            a.cos(),
            a.sin(),
            (i % 7) as f32 / 7.0
        )
    }

    fn store_with_vectors(n: usize) -> DocumentStore {
        let store = DocumentStore::new();
        let e = SqlEngine::new(&store).unwrap();
        e.execute("CREATE TABLE emb (id TEXT, v TEXT)").unwrap();
        let values: Vec<String> = (0..n)
            .map(|i| format!("('{i}', '{}')", vec_text(i)))
            .collect();
        e.execute(&format!("INSERT INTO emb VALUES {}", values.join(", ")))
            .unwrap();
        drop(e);
        store
    }

    fn search(e: &SqlEngine, i: usize, k: usize) -> Vec<String> {
        e.execute(&format!(
            "SELECT id FROM vector_search('emb', 'v', '{}', {k}) ORDER BY score DESC",
            vec_text(i)
        ))
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r[0].clone())
        .collect()
    }

    fn indexes(e: &SqlEngine) -> Vec<Vec<String>> {
        e.execute("SHOW INDEXES").unwrap().rows
    }

    #[test]
    fn search_without_an_index_is_exact_with_a_score_column() {
        let store = store_with_vectors(50);
        let e = SqlEngine::new(&store).unwrap();
        let out = e
            .execute(&format!(
                "SELECT id, score FROM vector_search('emb', 'v', '{}', 3)",
                vec_text(12)
            ))
            .unwrap();
        assert_eq!(out.rows.len(), 3);
        assert_eq!(out.rows[0][0], "12");
        let scores: Vec<f64> = out.rows.iter().map(|r| r[1].parse().unwrap()).collect();
        assert!((scores[0] - 1.0).abs() < 1e-4);
        assert!(scores.windows(2).all(|w| w[0] >= w[1]));
    }

    #[test]
    fn indexed_search_agrees_with_exact_search() {
        let store = store_with_vectors(300);
        let e = SqlEngine::new(&store).unwrap();
        let exact: Vec<Vec<String>> = (0..300).step_by(23).map(|i| search(&e, i, 5)).collect();
        let out = e
            .execute("CREATE INDEX emb_hnsw ON emb USING hnsw (v)")
            .unwrap();
        assert!(out.rows[0][0].starts_with("ok (300 vectors, 3 dimensions"));
        let indexed: Vec<Vec<String>> = (0..300).step_by(23).map(|i| search(&e, i, 5)).collect();
        assert_eq!(indexed, exact);
        assert_eq!(
            indexes(&e),
            vec![vec!["emb_hnsw", "emb", "v", "hnsw", "ready"]]
        );
    }

    #[test]
    fn writes_to_the_table_drop_its_indexes() {
        for (label, sql) in [
            ("update", "UPDATE emb SET v = '[0,0,1]' WHERE id = '1'"),
            ("delete", "DELETE FROM emb WHERE id = '1'"),
            ("drop table", "DROP TABLE emb"),
            (
                "create or replace",
                "CREATE OR REPLACE TABLE emb AS SELECT * FROM emb",
            ),
        ] {
            let store = store_with_vectors(20);
            let e = SqlEngine::new(&store).unwrap();
            e.execute("CREATE INDEX emb_hnsw ON emb USING hnsw (v)")
                .unwrap();
            assert_eq!(indexes(&e).len(), 1, "{label}: created");
            e.execute(sql).unwrap();
            assert!(indexes(&e).is_empty(), "{label}: index should be gone");
        }
    }

    #[test]
    fn a_table_changed_behind_the_index_falls_back_to_exact_search() {
        let store = store_with_vectors(40);
        let e = SqlEngine::new(&store).unwrap();
        e.execute("CREATE INDEX emb_hnsw ON emb USING hnsw (v)")
            .unwrap();
        // Bypass SQL so no hook drops the index: only the row count differs.
        store
            .custom_tables
            .write()
            .unwrap()
            .get_mut("emb")
            .unwrap()
            .rows
            .push(vec!["extra".into(), "[0.3, 0.3, 0.3]".into()]);
        assert_eq!(indexes(&e)[0][4], "stale");
        let hits = e
            .execute("SELECT id FROM vector_search('emb', 'v', '[0.3, 0.3, 0.3]', 1)")
            .unwrap();
        assert_eq!(hits.rows[0][0], "extra", "the new row must be findable");
    }

    #[test]
    fn create_and_drop_index_validation() {
        let store = store_with_vectors(10);
        let e = SqlEngine::new(&store).unwrap();
        let fails = |sql: &str, needle: &str| {
            let err = e.execute(sql).unwrap_err().to_string();
            assert!(err.contains(needle), "{sql}: {err}");
        };
        fails(
            "CREATE INDEX i ON emb USING btree (v)",
            "only `CREATE INDEX",
        );
        fails("CREATE INDEX i ON emb (v)", "only `CREATE INDEX");
        fails(
            "CREATE INDEX i ON emb USING hnsw (id, v)",
            "exactly one column",
        );
        fails(
            "CREATE INDEX i ON blocks USING hnsw (content)",
            "built-in table",
        );
        fails("CREATE INDEX i ON nope USING hnsw (v)", "unknown table");
        fails("CREATE INDEX i ON emb USING hnsw (nope)", "no column");
        fails("CREATE INDEX i ON emb USING hnsw (id)", "no vectors");
        fails(
            "CREATE INDEX i ON emb USING hnsw (v) WITH (foo = 1)",
            "unknown index option",
        );
        fails(
            "CREATE INDEX i ON emb USING hnsw (v) WITH (m = 0)",
            "between 1 and 10000",
        );
        fails(
            "CREATE INDEX i ON emb USING hnsw (v) WITH (m = 500)",
            "at most 128",
        );
        fails(
            "CREATE UNIQUE INDEX i ON emb USING hnsw (v)",
            "not supported",
        );
        fails("DROP INDEX missing", "does not exist");

        e.execute("CREATE INDEX i ON emb USING hnsw (v) WITH (m = 8, ef_construction = 50)")
            .unwrap();
        fails("CREATE INDEX i ON emb USING hnsw (v)", "already exists");
        let again = e
            .execute("CREATE INDEX IF NOT EXISTS i ON emb USING hnsw (v)")
            .unwrap();
        assert_eq!(again.rows[0][0], "already exists");
        e.execute("DROP INDEX IF EXISTS nothing").unwrap();
        assert_eq!(
            e.execute("DROP INDEX i").unwrap().rows[0][0],
            "1 index(es) dropped"
        );
        assert!(indexes(&e).is_empty());
    }

    #[test]
    fn unnamed_index_gets_a_default_name_and_bad_rows_are_reported() {
        let store = store_with_vectors(5);
        let e = SqlEngine::new(&store).unwrap();
        e.execute("INSERT INTO emb VALUES ('bad', 'oops'), ('short', '[1,2]')")
            .unwrap();
        let out = e.execute("CREATE INDEX ON emb USING hnsw (v)").unwrap();
        assert!(out.rows[0][0].contains("2 rows skipped"), "{:?}", out.rows);
        assert_eq!(indexes(&e)[0][0], "emb_v_hnsw");
    }

    #[test]
    fn vector_search_argument_validation() {
        let store = store_with_vectors(10);
        let e = SqlEngine::new(&store).unwrap();
        let fails = |args: &str, needle: &str| {
            let err = e
                .execute(&format!("SELECT * FROM vector_search({args})"))
                .unwrap_err()
                .to_string();
            assert!(err.contains(needle), "({args}): {err}");
        };
        fails("'emb', 'v'", "usage");
        fails("'emb', 'v', '[1,0,0]', 0", "k must be");
        fails("'emb', 'v', '[1,0,0]', 1000000", "k must be");
        fails("'emb', 'v', '[1,0,0]', 3, 0", "ef must be");
        fails("'emb', 'v', 'not json', 3", "JSON array");
        fails("'emb', 'v', '[0,0,0]', 3", "all zeros");
        fails("'nope', 'v', '[1,0,0]', 3", "unknown table");
        fails("'emb', 'nope', '[1,0,0]', 3", "no column");
        // A query of another length matches nothing rather than erroring.
        let none = e
            .execute("SELECT * FROM vector_search('emb', 'v', '[1,0]', 3)")
            .unwrap();
        assert!(none.rows.is_empty());
        e.execute("CREATE INDEX ON emb USING hnsw (v)").unwrap();
        let none = e
            .execute("SELECT * FROM vector_search('emb', 'v', '[1,0]', 3)")
            .unwrap();
        assert!(none.rows.is_empty());
    }

    #[test]
    fn search_joins_back_to_blocks_through_content_hash() {
        let mut store = DocumentStore::new();
        store
            .add_str("# Cats\n\nAbout cats\n\n# Dogs\n\nAbout dogs\n")
            .unwrap();
        let e = SqlEngine::new(&store).unwrap();
        e.execute("CREATE TABLE emb (content_hash TEXT, embedding TEXT)")
            .unwrap();
        // Hand-made 2-D "embeddings": cats point up, dogs point right.
        for (text, v) in [
            ("Cats", "[0,1]"),
            ("About cats", "[0.1,1]"),
            ("Dogs", "[1,0]"),
            ("About dogs", "[1,0.1]"),
        ] {
            e.execute(&format!(
                "INSERT INTO emb SELECT DISTINCT content_hash, '{v}' FROM blocks WHERE content = '{text}'"
            ))
            .unwrap();
        }
        e.execute("CREATE INDEX ON emb USING hnsw (embedding)")
            .unwrap();
        let out = e
            .execute(
                "SELECT b.content FROM vector_search('emb', 'embedding', '[0, 1]', 2) v \
                 JOIN blocks b ON b.content_hash = v.content_hash ORDER BY v.score DESC",
            )
            .unwrap();
        let got: Vec<&str> = out.rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(got, vec!["Cats", "About cats"]);
    }

    #[test]
    fn insert_extends_the_index_instead_of_dropping_it() {
        let store = store_with_vectors(60);
        let e = SqlEngine::new(&store).unwrap();
        e.execute("CREATE INDEX emb_hnsw ON emb USING hnsw (v)")
            .unwrap();
        e.execute("INSERT INTO emb VALUES ('new', '[0.9, 0.1, 0.9]'), ('bad', 'oops'), ('short', '[1,2]')")
            .unwrap();
        assert_eq!(indexes(&e)[0][4], "ready", "still current after an INSERT");
        let idx = store.get_vector_index("emb_hnsw").unwrap().unwrap();
        assert_eq!(idx.len(), 61, "only the valid vector was added");

        // The new row is found through the index.
        let hit = e
            .execute("SELECT id FROM vector_search('emb', 'v', '[0.9, 0.1, 0.9]', 1)")
            .unwrap();
        assert_eq!(hit.rows[0][0], "new");

        // And a later INSERT keeps extending it.
        e.execute("INSERT INTO emb VALUES ('newer', '[-0.5, 0.7, 0.2]')")
            .unwrap();
        assert_eq!(
            store.get_vector_index("emb_hnsw").unwrap().unwrap().len(),
            62
        );
        let hit = e
            .execute("SELECT id FROM vector_search('emb', 'v', '[-0.5, 0.7, 0.2]', 1)")
            .unwrap();
        assert_eq!(hit.rows[0][0], "newer");
    }

    #[test]
    fn extended_index_agrees_with_exact_search() {
        let store = store_with_vectors(150);
        let e = SqlEngine::new(&store).unwrap();
        e.execute("CREATE INDEX emb_hnsw ON emb USING hnsw (v)")
            .unwrap();
        // Grow the table by half again, a few rows per INSERT.
        for start in (150..225).step_by(5) {
            let values: Vec<String> = (start..start + 5)
                .map(|i| format!("('{i}', '{}')", vec_text(i)))
                .collect();
            e.execute(&format!("INSERT INTO emb VALUES {}", values.join(", ")))
                .unwrap();
        }
        assert_eq!(indexes(&e)[0][4], "ready");
        let indexed: Vec<Vec<String>> = (0..225).step_by(17).map(|i| search(&e, i, 5)).collect();
        e.execute("DROP INDEX emb_hnsw").unwrap();
        let exact: Vec<Vec<String>> = (0..225).step_by(17).map(|i| search(&e, i, 5)).collect();
        assert_eq!(indexed, exact);
    }

    #[test]
    fn a_stale_index_is_dropped_by_the_next_insert_not_extended() {
        let store = store_with_vectors(20);
        let e = SqlEngine::new(&store).unwrap();
        e.execute("CREATE INDEX emb_hnsw ON emb USING hnsw (v)")
            .unwrap();
        // Changed behind the index's back, so its row numbers are off.
        store
            .custom_tables
            .write()
            .unwrap()
            .get_mut("emb")
            .unwrap()
            .rows
            .push(vec!["sneaky".into(), "[1,1,1]".into()]);
        e.execute("INSERT INTO emb VALUES ('x', '[0,1,0]')")
            .unwrap();
        assert!(indexes(&e).is_empty());
    }

    #[test]
    fn index_is_used_after_reopen_and_stays_valid_across_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.mq-db");
        let mut base = DocumentStore::new();
        base.add_str("# T\n").unwrap();
        base.save(&path).unwrap();
        {
            let s = DocumentStore::open(&path).unwrap();
            let e = SqlEngine::new(&s).unwrap();
            e.execute("CREATE TABLE emb (id TEXT, v TEXT)").unwrap();
            let values: Vec<String> = (0..80)
                .map(|i| format!("('{i}', '{}')", vec_text(i)))
                .collect();
            e.execute(&format!("INSERT INTO emb VALUES {}", values.join(", ")))
                .unwrap();
            e.execute("CREATE INDEX emb_hnsw ON emb USING hnsw (v)")
                .unwrap();
        }
        let s = DocumentStore::open(&path).unwrap();
        let e = SqlEngine::new(&s).unwrap();
        assert_eq!(indexes(&e)[0][4], "ready");
        assert!(s.vector_indexes.read().unwrap()["emb_hnsw"].index.is_none());
        assert_eq!(search(&e, 33, 1), vec!["33"]);
        assert!(
            s.vector_indexes.read().unwrap()["emb_hnsw"].index.is_some(),
            "the persisted index was loaded and used"
        );

        // An INSERT extends it, and the extended index is what gets saved.
        e.execute("INSERT INTO emb VALUES ('z', '[1, 1, 1]')")
            .unwrap();
        drop(e);
        drop(s);
        let s = DocumentStore::open(&path).unwrap();
        let e = SqlEngine::new(&s).unwrap();
        assert_eq!(indexes(&e)[0][4], "ready", "the extension was persisted");
        assert_eq!(s.get_vector_index("emb_hnsw").unwrap().unwrap().len(), 81);
        let hit = e
            .execute("SELECT id FROM vector_search('emb', 'v', '[1, 1, 1]', 1)")
            .unwrap();
        assert_eq!(hit.rows[0][0], "z");

        // An UPDATE cannot be applied in place, so it drops the index, and
        // that drop is persisted too.
        e.execute("UPDATE emb SET v = '[0, 0, 1]' WHERE id = 'z'")
            .unwrap();
        drop(e);
        drop(s);
        let s = DocumentStore::open(&path).unwrap();
        let e = SqlEngine::new(&s).unwrap();
        assert!(indexes(&e).is_empty(), "the drop was persisted");
        assert_eq!(
            search(&e, 33, 1),
            vec!["33"],
            "still searchable, now exactly"
        );
    }

    #[test]
    fn a_table_with_a_score_column_is_rejected() {
        let store = DocumentStore::new();
        let e = SqlEngine::new(&store).unwrap();
        e.execute("CREATE TABLE s (id TEXT, v TEXT, score TEXT)")
            .unwrap();
        e.execute("INSERT INTO s VALUES ('a', '[1,0]', '0')")
            .unwrap();
        let err = e
            .execute("SELECT * FROM vector_search('s', 'v', '[1,0]', 1)")
            .unwrap_err()
            .to_string();
        assert!(err.contains("column named 'score'"), "{err}");
    }

    #[test]
    fn a_failed_drop_index_keeps_the_index() {
        let store = store_with_vectors(10);
        let e = SqlEngine::new(&store).unwrap();
        e.execute("CREATE INDEX emb_hnsw ON emb USING hnsw (v)")
            .unwrap();
        let removed = store.take_vector_indexes(&["emb_hnsw".to_string()]);
        assert!(indexes(&e).is_empty());
        store.restore_vector_indexes(removed);
        assert_eq!(indexes(&e).len(), 1);
    }

    #[test]
    fn index_changes_roll_back_with_the_transaction() {
        let store = store_with_vectors(20);
        let e = SqlEngine::new(&store).unwrap();
        e.execute("BEGIN").unwrap();
        e.execute("CREATE INDEX emb_hnsw ON emb USING hnsw (v)")
            .unwrap();
        assert_eq!(indexes(&e).len(), 1);
        e.execute("ROLLBACK").unwrap();
        assert!(indexes(&e).is_empty());
    }
}
