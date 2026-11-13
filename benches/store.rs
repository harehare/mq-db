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

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20);
    targets = bench
}
criterion_main!(benches);
