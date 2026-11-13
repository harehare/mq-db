//! Regression benchmarks on a deterministic synthetic corpus.
//! Run with `cargo bench`; `cargo bench --no-run` just checks they build.

use criterion::{Criterion, criterion_group, criterion_main};
use mq_db::{DocumentStore, SqlEngine};
use std::hint::black_box;

const DOCS: usize = 2_000;
const WORDS: [&str; 20] = [
    "error", "handling", "parser", "index", "query", "markdown", "section", "database", "vector",
    "search", "code", "block", "heading", "table", "rust", "async", "tokio", "lock", "page",
    "storage",
];

/// Deterministic pseudo-random word picker (no `rand` dependency).
fn word(seed: &mut u64) -> &'static str {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    WORDS[(*seed >> 33) as usize % WORDS.len()]
}

fn doc(i: usize) -> String {
    let mut seed = i as u64 + 1;
    let mut out = format!(
        "---\ntitle: Doc {i}\ntags: [t{}]\n---\n\n# Document {i}\n",
        i % 20
    );
    for s in 0..6 {
        out.push_str(&format!("\n## Section {s} {}\n\n", word(&mut seed)));
        for _ in 0..3 {
            let para: Vec<&str> = (0..40).map(|_| word(&mut seed)).collect();
            out.push_str(&para.join(" "));
            out.push_str("\n\n");
        }
        if s % 2 == 0 {
            out.push_str("```rust\nfn f() {}\n```\n");
        }
    }
    out
}

fn build_store() -> DocumentStore {
    let mut store = DocumentStore::new();
    for i in 0..DOCS {
        store.add_str(&doc(i)).unwrap();
    }
    store
}

fn bench(c: &mut Criterion) {
    let docs: Vec<String> = (0..DOCS).map(doc).collect();
    c.bench_function("index_2k_docs", |b| {
        b.iter(|| {
            let mut store = DocumentStore::new();
            for d in &docs {
                store.add_str(d).unwrap();
            }
            black_box(store)
        })
    });

    let store = build_store();
    let engine = SqlEngine::new(&store).unwrap();
    let queries = [
        ("sql_count", "SELECT count(*) FROM blocks"),
        (
            "sql_group_by",
            "SELECT block_type, count(*) FROM blocks GROUP BY block_type",
        ),
        (
            "sql_fts_match",
            "SELECT count(*) FROM blocks WHERE match(content, 'parser tokio')",
        ),
        (
            "sql_bm25_top5",
            "SELECT content FROM blocks WHERE match(content, 'parser tokio') \
             ORDER BY bm25(content, 'parser tokio') DESC LIMIT 5",
        ),
        (
            "sql_under_subquery",
            "SELECT count(*) FROM blocks b \
             WHERE b.document_id = (SELECT document_id FROM blocks WHERE block_type = 'heading' AND content = 'Document 1000') \
             AND under(b.pre, b.post, \
               (SELECT pre FROM blocks WHERE block_type = 'heading' AND content = 'Document 1000'), \
               (SELECT post FROM blocks WHERE block_type = 'heading' AND content = 'Document 1000'))",
        ),
        (
            "sql_in_subquery",
            "SELECT count(*) FROM blocks \
             WHERE document_id IN (SELECT document_id FROM blocks WHERE content = 'Section 3 parser')",
        ),
        (
            "sql_join_documents",
            "SELECT d.path, count(*) FROM blocks b JOIN documents d ON d.id = b.document_id \
             GROUP BY d.path LIMIT 3",
        ),
    ];
    for (name, sql) in queries {
        c.bench_function(name, |b| b.iter(|| black_box(engine.execute(sql).unwrap())));
    }
}

/// 10k clustered 128-d vectors (like real text embeddings, unlike uniform
/// noise), as a table of JSON arrays.
fn vector_store(indexed: bool) -> (DocumentStore, String) {
    const N: usize = 10_000;
    const D: usize = 128;
    const CENTERS: usize = 100;
    let mut seed = 42u64;
    let mut gauss = move || {
        // Sum of uniforms: close enough to a bell curve for a benchmark.
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 40) as f32 / (1u64 << 24) as f32 - 0.5
        };
        (0..6).map(|_| next()).sum::<f32>()
    };
    let centers: Vec<Vec<f32>> = (0..CENTERS)
        .map(|_| (0..D).map(|_| gauss()).collect())
        .collect();
    let store = DocumentStore::new();
    let engine = SqlEngine::new(&store).unwrap();
    engine
        .execute("CREATE TABLE emb (id TEXT, v TEXT)")
        .unwrap();
    let mut query = String::new();
    for chunk in (0..N).collect::<Vec<_>>().chunks(1000) {
        let rows: Vec<String> = chunk
            .iter()
            .map(|&i| {
                let c = &centers[i % CENTERS];
                let v: Vec<String> = c
                    .iter()
                    .map(|x| format!("{:.4}", x + 0.1 * gauss()))
                    .collect();
                if i == 5000 {
                    query = format!("[{}]", v.join(","));
                }
                format!("('{i}', '[{}]')", v.join(","))
            })
            .collect();
        engine
            .execute(&format!("INSERT INTO emb VALUES {}", rows.join(",")))
            .unwrap();
    }
    if indexed {
        engine
            .execute("CREATE INDEX emb_hnsw ON emb USING hnsw (v)")
            .unwrap();
    }
    drop(engine);
    (store, query)
}

fn bench_vectors(c: &mut Criterion) {
    let (exact_store, query) = vector_store(false);
    let (indexed_store, _) = vector_store(true);
    let sql = format!("SELECT id FROM vector_search('emb', 'v', '{query}', 10)");
    let exact = SqlEngine::new(&exact_store).unwrap();
    let indexed = SqlEngine::new(&indexed_store).unwrap();
    assert_eq!(
        exact.execute(&sql).unwrap().rows[0],
        indexed.execute(&sql).unwrap().rows[0]
    );
    c.bench_function("vector_search_exact_10k", |b| {
        b.iter(|| black_box(exact.execute(&sql).unwrap()))
    });
    c.bench_function("vector_search_hnsw_10k", |b| {
        b.iter(|| black_box(indexed.execute(&sql).unwrap()))
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20);
    targets = bench, bench_vectors
}
criterion_main!(benches);
