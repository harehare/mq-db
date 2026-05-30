use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use mqdb::{DocumentStore, SqlEngine, block::BlockType};

// ─────────────────────────────────────────────────────────────────────────────
// Fixtures
// ─────────────────────────────────────────────────────────────────────────────

/// Generate a Markdown document with `n_sections` H2 sections, each containing
/// a paragraph and a fenced code block.
fn make_md(n_sections: usize) -> String {
    let mut s = String::from("---\ntitle: Benchmark Doc\ntags: [bench, rust]\n---\n\n# Document Title\n\n");
    for i in 0..n_sections {
        s.push_str(&format!(
            "## Section {i}\n\nThis is paragraph {i} with some content.\n\n```rust\nfn func_{i}() -> u32 {{ {i} }}\n```\n\n"
        ));
        if i % 3 == 0 {
            s.push_str(&format!("- item A in section {i}\n- item B\n- item C\n\n"));
        }
    }
    s
}

/// Build a store pre-loaded with `n_docs` documents of `sections_each` sections.
fn make_store(n_docs: usize, sections_each: usize) -> DocumentStore {
    let md = make_md(sections_each);
    let mut store = DocumentStore::new();
    for _ in 0..n_docs {
        store.add_str(&md).unwrap();
    }
    store
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. Parsing + index build
// ─────────────────────────────────────────────────────────────────────────────

fn bench_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_and_index");

    for sections in [5, 20, 50, 100] {
        let md = make_md(sections);
        group.bench_with_input(
            BenchmarkId::new("sections", sections),
            &md,
            |b, md| {
                b.iter(|| {
                    let mut store = DocumentStore::new();
                    store.add_str(md).unwrap();
                });
            },
        );
    }
    group.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. mq-style Query API
// ─────────────────────────────────────────────────────────────────────────────

fn bench_query(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_api");

    // heading_depth filter — scans all blocks, picks headings
    {
        let store = make_store(10, 20);
        group.bench_function("heading_depth_filter", |b| {
            b.iter(|| store.query().heading_depth(2).blocks());
        });
    }

    // code_lang filter
    {
        let store = make_store(10, 20);
        group.bench_function("code_lang_filter", |b| {
            b.iter(|| store.query().code_lang("rust").blocks());
        });
    }

    // under_heading — resolves section interval then scans
    {
        let store = make_store(1, 50);
        group.bench_function("under_heading", |b| {
            b.iter(|| {
                store
                    .query()
                    .under_heading("Section 10", Some(2))
                    .filter(|bl| {
                        matches!(bl.block_type, BlockType::Paragraph | BlockType::Code)
                    })
                    .blocks()
            });
        });
    }

    // zone-map skip: query for a lang present in only 1 of 10 docs
    {
        let md_rust = make_md(20); // has rust blocks
        let md_other = "# Other\n\n```python\nx = 1\n```\n".to_string();
        let mut store = DocumentStore::new();
        store.add_str(&md_rust).unwrap();
        for _ in 0..9 {
            store.add_str(&md_other).unwrap();
        }
        group.bench_function("zone_map_skip_9_of_10", |b| {
            b.iter(|| {
                store
                    .query()
                    .documents(|d| d.zone_maps.code_languages.contains("rust"))
                    .code_lang("rust")
                    .blocks()
            });
        });
    }

    // lint check: heading followed by forbidden type
    {
        let store = make_store(10, 20);
        group.bench_function("lint_heading_followed_by", |b| {
            b.iter(|| {
                let q = store.query();
                let violations = q.lint_heading_followed_by(2, &[BlockType::List]);
                violations.len()
            });
        });
    }

    // scaling: vary number of documents
    for n_docs in [1, 10, 50, 100] {
        let store = make_store(n_docs, 20);
        group.bench_with_input(
            BenchmarkId::new("heading_depth_ndocs", n_docs),
            &store,
            |b, s| {
                b.iter(|| s.query().heading_depth(2).count());
            },
        );
    }

    group.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. SQL Engine
// ─────────────────────────────────────────────────────────────────────────────

fn bench_sql(c: &mut Criterion) {
    let mut group = c.benchmark_group("sql_engine");

    // Loading cost: build SqlEngine from store
    for n_docs in [1, 10, 50] {
        let store = make_store(n_docs, 20);
        group.bench_with_input(
            BenchmarkId::new("engine_load_ndocs", n_docs),
            &store,
            |b, s| {
                b.iter(|| SqlEngine::new(s).unwrap());
            },
        );
    }

    // Simple SELECT (already loaded)
    {
        let store = make_store(10, 20);
        let engine = SqlEngine::new(&store).unwrap();
        group.bench_function("select_headings", |b| {
            b.iter(|| {
                engine
                    .execute("SELECT content FROM blocks WHERE block_type='heading' ORDER BY pre")
                    .unwrap()
            });
        });
    }

    // GROUP BY aggregate
    {
        let store = make_store(10, 20);
        let engine = SqlEngine::new(&store).unwrap();
        group.bench_function("group_by_block_type", |b| {
            b.iter(|| {
                engine
                    .execute("SELECT block_type, count(*) FROM blocks GROUP BY block_type")
                    .unwrap()
            });
        });
    }

    // UNDER() hierarchy query
    {
        let store = make_store(1, 50);
        let engine = SqlEngine::new(&store).unwrap();
        let sql = "SELECT b.content FROM blocks b \
                   WHERE under(b.pre, b.post, \
                     (SELECT pre FROM blocks WHERE block_type='heading' AND content='Section 10'), \
                     (SELECT post FROM blocks WHERE block_type='heading' AND content='Section 10'))";
        group.bench_function("under_udf", |b| {
            b.iter(|| engine.execute(sql).unwrap());
        });
    }

    // JOIN: linter query in pure SQL
    {
        let store = make_store(10, 20);
        let engine = SqlEngine::new(&store).unwrap();
        let sql = "SELECT h.content, nxt.block_type \
                   FROM blocks h \
                   JOIN blocks nxt ON nxt.document_id = h.document_id AND nxt.pre = h.pre + 1 \
                   WHERE h.block_type='heading' \
                     AND json_extract(h.properties,'$.depth')=2 \
                     AND nxt.block_type='list'";
        group.bench_function("sql_linter_join", |b| {
            b.iter(|| engine.execute(sql).unwrap());
        });
    }

    group.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. Storage round-trip
// ─────────────────────────────────────────────────────────────────────────────

fn bench_storage(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage");

    for n_docs in [1, 5, 20] {
        let store = make_store(n_docs, 20);

        group.bench_with_input(
            BenchmarkId::new("save_ndocs", n_docs),
            &store,
            |b, s| {
                b.iter(|| {
                    let dir = tempfile::tempdir().unwrap();
                    let path = dir.path().join("bench.mqdb");
                    s.save(&path).unwrap();
                });
            },
        );

        // save once, then bench repeated loads
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bench.mqdb");
        store.save(&path).unwrap();

        group.bench_with_input(
            BenchmarkId::new("load_ndocs", n_docs),
            &path,
            |b, p| {
                b.iter(|| DocumentStore::load(p).unwrap());
            },
        );
    }

    group.finish();
}

// ─────────────────────────────────────────────────────────────────────────────
// Registration
// ─────────────────────────────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_parse,
    bench_query,
    bench_sql,
    bench_storage,
);
criterion_main!(benches);
