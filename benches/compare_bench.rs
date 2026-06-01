/// Comparison benchmarks: mq-db custom SQL engine vs DuckDB
///
/// Both engines receive identical pre-parsed block data loaded into their
/// respective in-memory databases.  Parse time (Markdown → blocks) is
/// excluded so we measure **pure query engine performance**.
///
/// Run:  cargo bench --bench compare_bench
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use duckdb::Connection as DuckConn;
use mq_db::{DocumentStore, SqlEngine, block::BlockType};

// ─────────────────────────────────────────────────────────────────────────────
// Shared fixture
// ─────────────────────────────────────────────────────────────────────────────

fn make_md(n_sections: usize) -> String {
    let mut s = String::from(
        "---\ntitle: Benchmark Doc\ntags: [bench, rust]\n---\n\n# Document Title\n\n",
    );
    for i in 0..n_sections {
        s.push_str(&format!(
            "## Section {i}\n\nParagraph content for section {i}.\n\n\
             ```rust\nfn func_{i}() -> u32 {{ {i} }}\n```\n\n"
        ));
        if i % 3 == 0 {
            s.push_str(&format!("- item A in section {i}\n- item B\n- item C\n\n"));
        }
    }
    s
}

fn make_store(n_docs: usize, sections: usize) -> DocumentStore {
    let md = make_md(sections);
    let mut store = DocumentStore::new();
    for _ in 0..n_docs { store.add_str(&md).unwrap(); }
    store
}

// ─────────────────────────────────────────────────────────────────────────────
// DuckDB helpers  (for comparison against the custom SqlEngine)
// ─────────────────────────────────────────────────────────────────────────────

fn duckdb_load(store: &DocumentStore) -> DuckConn {
    let conn = DuckConn::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE documents (id INTEGER, path TEXT, title TEXT, tags TEXT);
         CREATE TABLE blocks (
             id INTEGER, document_id INTEGER,
             block_type TEXT, content TEXT,
             pre INTEGER, post INTEGER, properties TEXT
         );",
    )
    .unwrap();

    {
        let mut appender = conn.appender("blocks").unwrap();
        let mut block_id: u32 = 0;
        for doc in store.documents() {
            for block in &doc.blocks {
                appender
                    .append_row(duckdb::appender_params_from_iter([
                        block_id.to_string(),
                        doc.id.to_string(),
                        block.block_type.as_str().to_string(),
                        block.content.clone(),
                        block.pre.to_string(),
                        block.post.to_string(),
                        "{}".to_string(),
                    ]))
                    .unwrap();
                block_id += 1;
            }
        }
        appender.flush().unwrap();
    } // appender dropped here, borrow released
    conn
}

fn duckdb_exec(conn: &DuckConn, sql: &str) -> usize {
    let mut stmt = conn.prepare(sql).unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut count = 0;
    while rows.next().unwrap().is_some() { count += 1; }
    count
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. Engine load cost
// ─────────────────────────────────────────────────────────────────────────────

fn bench_load(c: &mut Criterion) {
    let mut group = c.benchmark_group("load_engine");
    group.sample_size(50);

    for n_docs in [1, 10, 50] {
        let store = make_store(n_docs, 20);

        group.bench_with_input(
            BenchmarkId::new("mq-db_custom", n_docs),
            &store,
            |b, s| b.iter(|| SqlEngine::new(s).unwrap()),
        );

        group.bench_with_input(
            BenchmarkId::new("duckdb", n_docs),
            &store,
            |b, s| b.iter(|| duckdb_load(s)),
        );
    }
    group.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. SELECT with WHERE (simple filter)
// ─────────────────────────────────────────────────────────────────────────────

fn bench_select_filter(c: &mut Criterion) {
    let mut group = c.benchmark_group("select_filter");

    let store = make_store(10, 20);
    let engine = SqlEngine::new(&store).unwrap();
    let duck_conn = duckdb_load(&store);

    let sql = "SELECT content FROM blocks WHERE block_type = 'heading' ORDER BY pre";

    group.bench_function("mq-db_custom", |b| {
        b.iter(|| engine.execute(sql).unwrap())
    });
    group.bench_function("duckdb", |b| {
        b.iter(|| duckdb_exec(&duck_conn, sql))
    });

    // Also compare against native mq-db query API (no SQL overhead)
    group.bench_function("mq-db_query_api", |b| {
        b.iter(|| store.query().heading_depth(2).blocks())
    });

    group.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. GROUP BY aggregate
// ─────────────────────────────────────────────────────────────────────────────

fn bench_aggregate(c: &mut Criterion) {
    let mut group = c.benchmark_group("aggregate_group_by");

    let store = make_store(10, 20);
    let engine = SqlEngine::new(&store).unwrap();
    let duck_conn = duckdb_load(&store);

    let sql = "SELECT block_type, count(*) AS n FROM blocks GROUP BY block_type ORDER BY n DESC";

    group.bench_function("mq-db_custom", |b| {
        b.iter(|| engine.execute(sql).unwrap())
    });
    group.bench_function("duckdb", |b| {
        b.iter(|| duckdb_exec(&duck_conn, sql))
    });

    group.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. Hierarchy query: UNDER() (mq-db) vs subquery correlated scan (DuckDB)
// ─────────────────────────────────────────────────────────────────────────────

fn bench_hierarchy(c: &mut Criterion) {
    let mut group = c.benchmark_group("hierarchy_query");

    let store = make_store(1, 50);
    let engine = SqlEngine::new(&store).unwrap();
    let duck_conn = duckdb_load(&store);

    // mq-db uses the native UNDER() interval-index predicate
    let sql_mq-db = "SELECT b.content FROM blocks b \
                    WHERE under(b.pre, b.post, \
                      (SELECT pre FROM blocks WHERE block_type='heading' AND content='Section 10'), \
                      (SELECT post FROM blocks WHERE block_type='heading' AND content='Section 10'))";

    // DuckDB: equivalent using a correlated subquery / JOIN
    // (No interval-index UDF; DuckDB must use its own optimiser)
    let sql_duck = "WITH anc AS (
                        SELECT pre, post FROM blocks
                        WHERE block_type='heading' AND content='Section 10'
                        LIMIT 1
                    )
                    SELECT b.content FROM blocks b, anc
                    WHERE b.pre > anc.pre AND b.post < anc.post";

    group.bench_function("mq-db_custom_under", |b| {
        b.iter(|| engine.execute(sql_mq-db).unwrap())
    });
    group.bench_function("duckdb_cte_range", |b| {
        b.iter(|| duckdb_exec(&duck_conn, sql_duck))
    });
    // native mq-db API for fairest comparison
    group.bench_function("mq-db_query_api_under_heading", |b| {
        b.iter(|| {
            store
                .query()
                .under_heading("Section 10", Some(2))
                .blocks()
        })
    });

    group.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// 5. JOIN linter query
// ─────────────────────────────────────────────────────────────────────────────

fn bench_linter_join(c: &mut Criterion) {
    let mut group = c.benchmark_group("linter_join");

    let store = make_store(10, 20);
    let engine = SqlEngine::new(&store).unwrap();
    let duck_conn = duckdb_load(&store);

    let sql = "SELECT h.content, nxt.block_type \
               FROM blocks h \
               JOIN blocks nxt ON nxt.document_id = h.document_id AND nxt.pre = h.pre + 1 \
               WHERE h.block_type = 'heading' \
                 AND CAST(json_extract_string(h.properties, '$.depth') AS INTEGER) = 2 \
                 AND nxt.block_type = 'list'";

    // mq-db uses json_extract(); DuckDB uses json_extract_string()
    let sql_sqlite = "SELECT h.content, nxt.block_type \
                      FROM blocks h \
                      JOIN blocks nxt ON nxt.document_id = h.document_id AND nxt.pre = h.pre + 1 \
                      WHERE h.block_type = 'heading' \
                        AND json_extract(h.properties, '$.depth') = 2 \
                        AND nxt.block_type = 'list'";

    group.bench_function("mq-db_custom", |b| {
        b.iter(|| engine.execute(sql_sqlite).unwrap())
    });
    group.bench_function("duckdb", |b| {
        b.iter(|| duckdb_exec(&duck_conn, sql))
    });
    group.bench_function("mq-db_query_api_lint", |b| {
        b.iter(|| {
            let q = store.query();
            let v = q.lint_heading_followed_by(2, &[BlockType::List]);
            v.len()
        })
    });

    group.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// 6. Scale: vary block count (both engines pre-loaded)
// ─────────────────────────────────────────────────────────────────────────────

fn bench_scale(c: &mut Criterion) {
    let mut group = c.benchmark_group("scale_ndocs");
    group.sample_size(30);

    let sql = "SELECT count(*) FROM blocks WHERE block_type = 'heading'";

    for n_docs in [1, 10, 50, 100] {
        let store = make_store(n_docs, 20);
        let engine = SqlEngine::new(&store).unwrap();
        let duck_conn = duckdb_load(&store);

        group.bench_with_input(
            BenchmarkId::new("mq-db_custom", n_docs),
            &engine,
            |b, e| b.iter(|| e.execute(sql).unwrap()),
        );
        group.bench_with_input(
            BenchmarkId::new("duckdb", n_docs),
            &duck_conn,
            |b, c| b.iter(|| duckdb_exec(c, sql)),
        );
        group.bench_with_input(
            BenchmarkId::new("mq-db_query_api", n_docs),
            &store,
            |b, s| b.iter(|| s.query().heading_depth(2).count()),
        );
    }
    group.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// Registration
// ─────────────────────────────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_load,
    bench_select_filter,
    bench_aggregate,
    bench_hierarchy,
    bench_linter_join,
    bench_scale,
);
criterion_main!(benches);
