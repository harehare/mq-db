# Library API (Rust)

`mq-db` is usable directly as a Rust library, without shelling out to the CLI.

```toml
[dependencies]
mq-db = "0.1"
```

```rust
use mq_db::{DocumentStore, SqlEngine, MqEngine, block::BlockType};

// ── Build in memory ──────────────────────────────────────────────────────────
let mut store = DocumentStore::new();
store.add_file("docs/DESIGN.md")?;
store.add_str("# Hello\n\n## Architecture\n\nDetails\n")?;

// Chainable query API — zone-map skip + interval scope + block predicates
let chunks = store.query()
    .documents(|doc| doc.zone_maps.heading_contents.contains("Architecture"))
    .under_heading("Architecture", Some(2))
    .filter(|b| matches!(b.block_type, BlockType::Paragraph | BlockType::Code))
    .blocks();

// SQL engine (custom sqlparser-based evaluator — no SQLite dependency)
let engine = SqlEngine::new(&store)?;
let out = engine.execute(
    "SELECT content FROM blocks WHERE block_type = 'heading' ORDER BY pre"
)?;
print!("{}", out.to_table());

// mq engine
let results = MqEngine::eval_store(".h1", &store)?;

// Structural lint
let violations = store.query().lint_heading_followed_by(2, &[BlockType::List]);

// ── Persist / load ───────────────────────────────────────────────────────────
store.save("store.mq-db")?;

// Full load — all blocks read into memory, indexes built on first SqlEngine use
let store = DocumentStore::load("store.mq-db")?;

// Lazy open — catalog only; call load_all_blocks() + load_all_indexes() before SQL
let mut store = DocumentStore::open("store.mq-db")?;
store.load_all_blocks()?;
store.load_all_indexes()?;

// Catalog-only — for metadata commands (list, stats) that don't need block data
let store = DocumentStore::load_catalog_only("store.mq-db")?;
```

## Loading strategies

| Function | Loads | Use for |
| --- | --- | --- |
| `DocumentStore::new()` | Nothing (empty, in-memory) | Building a store from scratch |
| `DocumentStore::load()` | Catalog + all blocks + indexes | One-shot CLI queries |
| `DocumentStore::open()` | Catalog only, lazily | Long-lived processes that defer block/index loading |
| `DocumentStore::load_catalog_only()` | Catalog only | `list` / `stats`-style metadata commands |

When using `open()`, call `load_all_blocks()` and `load_all_indexes()` before running any `SqlEngine` query.

## Query builder

`store.query()` returns a chainable builder that applies the same three index layers used by the SQL engine, in order:

1. `.documents(|doc| ...)` — zone-map predicate, skips whole documents
2. `.under_heading(title, depth)` / interval-scope helpers — narrows to a `(pre, post)` range
3. `.filter(|block| ...)` — per-block predicate over the remaining candidates
4. `.blocks()` — materializes the final `Vec<&Block>`

See [Index Layers](../reference/index-layers.md) for how each layer works.
