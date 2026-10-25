# Library API (Rust)

`mq-db` is usable directly as a Rust library, without shelling out to the CLI.

```toml
[dependencies]
mq-db = "0.1"
```

```rust
use mq_db::{DocumentStore, SqlEngine, MqEngine};

// Build in memory
let mut store = DocumentStore::new();
store.add_file("docs/DESIGN.md")?;
store.add_str("# Hello\n\n## Architecture\n\nDetails\n")?;

// SQL engine (custom sqlparser-based evaluator, no SQLite dependency)
let engine = SqlEngine::new(&store)?;
let out = engine.execute(
    "SELECT content FROM blocks WHERE block_type = 'heading' ORDER BY pre"
)?;
print!("{}", out.to_table());

// mq engine
let results = MqEngine::eval_store(".h1", &store)?;

// UPDATE/DELETE with write-back: rewrites the affected block's source file
// (heading/paragraph content only), then re-parses it in place
store.execute_sql_mut(
    "UPDATE blocks SET content = 'New Title' WHERE block_type = 'heading' AND content = 'Old Title'"
)?;

// Persist / load
store.save("store.mq-db")?;

// Full load: all blocks read into memory, indexes built on first SqlEngine use
let store = DocumentStore::load("store.mq-db")?;

// Lazy open: catalog only, call load_all_blocks() + load_all_indexes() before SQL
let mut store = DocumentStore::open("store.mq-db")?;
store.load_all_blocks()?;
store.load_all_indexes()?;

// Catalog-only: for metadata commands (list, stats) that don't need block data
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
