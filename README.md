# mqdb

A Markdown-specialized embedded database that treats Markdown documents as **structured, hierarchical databases** rather than plain text.

## Overview

`mqdb` parses Markdown into a flat block list with **interval indexing** (Nested Set / Pre-Post Order), enabling O(1) section hierarchy queries. It supports both **SQL** and **mq** query languages.

```
[Markdown File]
      │
      ▼  CST Parser (mq-markdown)
[Block Tree]  ─── (heading, paragraph, code, list, …)
      │
      ▼  Interval Index
[Flat Block Vector]  pre/post integers per block
      │
      ├── SQL Engine  (rusqlite in-memory + UNDER() UDF)
      └── mq Engine   (mq-lang evaluator)
```

## Features

- **Flat block storage** – every Markdown element becomes a typed `Block` row with row-polymorphic properties
- **O(1) hierarchy queries** – `UNDER()` SQL function uses the interval index (`pre > anc_pre AND post < anc_post`)
- **Zone Maps** – per-document statistics for query pruning (skip files that can't match)
- **Dual query engines** – SQL via `rusqlite` and `mq` via `mq-lang`
- **Custom page-file persistence** – 8 KB fixed pages, checksum, atomic writes
- **CLI + interactive REPL + TUI** – all operations from the terminal

## Installation

```bash
git clone https://github.com/yourname/mqdb
cd mqdb
cargo build --release
# binary at: target/release/mqdb
```

> **Requires:** the [`mq`](https://github.com/harehare/mq) repository checked out as a sibling directory (`../mq`).

## CLI Usage

### Index Markdown files

```bash
# Index a directory (recursively) → store.mqdb
mqdb index docs/ --recursive --output store.mqdb

# Index individual files
mqdb index README.md DESIGN.md
```

### List indexed documents

```bash
mqdb list --db store.mqdb
```

```
ID      Path / Title                                        Blocks    Tags
────────────────────────────────────────────────────────────────────────────
0       docs/DESIGN.md                                      142
1       docs/API.md                                         87        api, v2
```

### SQL queries

```bash
mqdb sql "SELECT block_type, count(*) FROM blocks GROUP BY block_type" --db store.mqdb
```

```
┌─────────────┬──────────┐
│ block_type  │ count(*) │
├─────────────┼──────────┤
│ paragraph   │ 48       │
│ heading     │ 21       │
│ code        │ 15       │
└─────────────┴──────────┘
(3 rows)
```

**Hierarchy query with `UNDER()`** – find all content inside a specific section:

```bash
mqdb sql "
  SELECT b.block_type, b.content
  FROM blocks b
  WHERE under(b.pre, b.post,
    (SELECT pre FROM blocks WHERE block_type='heading' AND content='Architecture'),
    (SELECT post FROM blocks WHERE block_type='heading' AND content='Architecture')
  )
  ORDER BY b.pre
" --db store.mqdb
```

### mq queries

```bash
# Extract all H1 headings
mqdb mq ".h1" --db store.mqdb

# Extract all Rust code blocks
mqdb mq "select(.code_lang == \"rust\")" --db store.mqdb
```

### Interactive REPL

```bash
mqdb repl --db store.mqdb --mode sql
```

```
mqdb REPL  (type .help for commands, .quit to exit)
Mode: sql  (.mode mq | .mode sql to switch)

sql> SELECT content FROM blocks WHERE block_type = 'heading' LIMIT 3;
┌──────────────────┐
│ content          │
├──────────────────┤
│ Overview         │
│ Architecture     │
│ Query Engine     │
└──────────────────┘
(3 rows)

sql> .mode mq
Switched to mq mode.
mq> .h2
## Architecture
## Query Engine
```

### Structural linting

Check that H2 headings are not immediately followed by a list (must have an intro paragraph):

```bash
mqdb lint --db store.mqdb --depth 2
```

```
Found 1 violation:
  docs/DESIGN.md  H2 "Quick Start" immediately followed by list
```

### Statistics

```bash
mqdb stats --db store.mqdb
```

```
Documents : 5
Blocks    : 632

Block type breakdown:
  paragraph            241
  heading              89
  code                 73
  list                 58
  table_cell           52
  ...

Code block languages:
  rust                 41
  python               18
  bash                 14
```

### Show document structure

```bash
mqdb show 0 --db store.mqdb
```

```
Document: docs/DESIGN.md  (id=0)
Blocks: 142

pre     post    depth   type              content
──────────────────────────────────────────────────────────────────────
0       141             heading           Design Document
2       55      H2      heading           Architecture
4       21              paragraph         The system is built on…
22      37      H3      heading           Query Engine
24      36              code              fn main() { … }
```

### TUI

```bash
mqdb tui --db store.mqdb
```

```
 mqdb  Mode: SQL  [Tab: switch mode]  [Ctrl+C: quit]
┌──────────────────┬──────────────────────────────────────────────┐
│ Documents        │ SQL Query                                     │
│ ▶ DESIGN.md(142) │ > SELECT block_type, count(*) FROM blocks…_  │
│   API.md (87)    ├──────────────────────────────────────────────┤
│   README.md (34) │ Results                                       │
│                  │ ┌─────────────┬──────────┐                   │
│                  │ │ block_type  │ count(*) │                   │
│                  │ ├─────────────┼──────────┤                   │
│                  │ │ paragraph   │ 48       │                   │
└──────────────────┴──────────────────────────────────────────────┘
  [j/k: navigate]  [i: focus input]  [Esc: blur]  [Enter: run]
```

**Keys:**
| Key | Action |
|-----|--------|
| `i` | Focus query input |
| `Esc` | Blur input |
| `Enter` | Run query |
| `Tab` | Toggle mq / SQL mode |
| `j` / `k` | Navigate document list |
| `PgDn` / `PgUp` | Scroll results |
| `q` | Quit |
| `Ctrl+C` | Force quit |

## Library API

```rust
use mqdb::{DocumentStore, SqlEngine, MqEngine, block::BlockType};

// Build an in-memory store
let mut store = DocumentStore::new();
store.add_file("docs/DESIGN.md")?;
store.add_str("# Hello\n\n## Architecture\n\nDetails\n")?;

// mq-style query API
let chunks = store.query()
    .under_heading("Architecture", Some(2))
    .filter(|b| matches!(b.block_type, BlockType::Paragraph | BlockType::Code))
    .blocks();

// SQL engine
let engine = SqlEngine::new(&store)?;
let out = engine.execute(
    "SELECT content FROM blocks WHERE block_type = 'heading' ORDER BY pre"
)?;
println!("{}", out.to_table());

// mq engine (file-backed documents)
store.add_file("README.md")?;
let results = MqEngine::eval_store(".h1", &store)?;

// Structural lint
let violations = store.query()
    .lint_heading_followed_by(2, &[BlockType::List]);

// Persist to disk
store.save("store.mqdb")?;

// Load from disk
let store = DocumentStore::load("store.mqdb")?;
```

## SQL Schema

```sql
-- Indexed documents
CREATE TABLE documents (
    id      INTEGER PRIMARY KEY,
    path    TEXT,
    title   TEXT,
    tags    TEXT    -- JSON array, e.g. '["rust","api"]'
);

-- All blocks from all documents
CREATE TABLE blocks (
    id          INTEGER PRIMARY KEY,
    document_id INTEGER NOT NULL,
    block_type  TEXT NOT NULL,   -- 'heading','paragraph','code','list',…
    content     TEXT NOT NULL,
    pre         INTEGER NOT NULL, -- interval index pre-order
    post        INTEGER NOT NULL, -- interval index post-order
    properties  TEXT NOT NULL     -- JSON: {"depth":2}, {"lang":"rust"}, …
);
```

### Custom SQL function

```sql
-- under(pre, post, anc_pre, anc_post) → BOOL
-- True if block (pre,post) is a descendant of (anc_pre, anc_post)
SELECT * FROM blocks b
WHERE under(b.pre, b.post,
  (SELECT pre FROM blocks WHERE content = 'Architecture'),
  (SELECT post FROM blocks WHERE content = 'Architecture'));
```

### Useful queries

```sql
-- Linter: H2 headings immediately followed by a list
SELECT d.path, h.content AS heading, nxt.block_type AS next_type
FROM blocks h
JOIN blocks nxt ON nxt.document_id = h.document_id AND nxt.pre = h.pre + 1
JOIN documents d ON d.id = h.document_id
WHERE h.block_type = 'heading'
  AND json_extract(h.properties, '$.depth') = 2
  AND nxt.block_type = 'list';

-- RAG: extract all text/code under a specific section
SELECT b.block_type, b.content
FROM blocks b
WHERE under(b.pre, b.post,
  (SELECT pre FROM blocks WHERE block_type='heading' AND content='Architecture'),
  (SELECT post FROM blocks WHERE block_type='heading' AND content='Architecture'))
  AND b.block_type IN ('paragraph', 'code')
ORDER BY b.pre;

-- Documents containing Python code
SELECT DISTINCT d.path
FROM documents d
JOIN blocks b ON b.document_id = d.id
WHERE b.block_type = 'code'
  AND json_extract(b.properties, '$.lang') = 'python';
```

## Architecture

### Block model

Each Markdown element becomes a `Block`:

```rust
struct Block {
    id: u32,
    document_id: u32,
    block_type: BlockType,  // Heading, Paragraph, Code, List, …
    content: String,
    span: Span,             // byte offset for editor sync
    pre: u32,               // interval index (pre-order)
    post: u32,              // interval index (post-order)
    properties: HashMap<String, PropertyValue>,  // row-polymorphic
}
```

Property examples:
- `Heading` → `{ "depth": 2, "slug": "architecture" }`
- `Code` → `{ "lang": "rust", "meta": "no_run" }`
- `Yaml` front-matter → `{ "title": "Design", "tags": ["api"] }`

### Interval index

The heading hierarchy is encoded as `(pre, post)` integer pairs via a Pre-Post Order (Nested Set) traversal:

```
# Doc          pre=0, post=11
## Section A   pre=2, post=7
  Paragraph    pre=3, post=4
  Code         pre=5, post=6
## Section B   pre=8, post=11
  Paragraph    pre=9, post=10
```

**Ancestor check:** `block_A is_under block_B` ↔ `A.pre > B.pre AND A.post < B.post` — **O(1), no tree traversal**.

### Storage format

Custom 8 KB page file (`docs/STORAGE_FORMAT.md`):

```
Page 0  │ File header  (magic 0x4D514442, version, page count)
Page 1  │ Catalog      (doc_id → first_block_page, ZoneMaps)
Page 2+ │ Block data   (linked page chains per document)
```

### Zone Maps

Per-document statistics for query pruning:

| Statistic | Used to skip |
|-----------|-------------|
| `heading_contents` | `UNDER "Architecture"` on docs without that heading |
| `code_languages` | `WHERE lang='python'` on docs with no Python code |
| `max_heading_depth` | depth filters |
| `tags` | tag-based document filters |

## Project structure

```
mqdb/
├── src/
│   ├── lib.rs          # Public API
│   ├── block.rs        # Block, BlockType, PropertyValue
│   ├── document.rs     # Document, ZoneMaps
│   ├── index.rs        # build_blocks(): mq-markdown → interval-indexed blocks
│   ├── query.rs        # Chainable query builder (mq-style API)
│   ├── sql.rs          # SqlEngine: rusqlite + UNDER() UDF
│   ├── mq_engine.rs    # MqEngine: mq-lang wrapper
│   ├── tui.rs          # ratatui TUI
│   ├── store.rs        # DocumentStore: add, query, save, load
│   ├── error.rs        # MqdbError
│   └── storage/        # Custom page-file persistence
│       ├── mod.rs
│       ├── codec.rs    # Block ↔ binary serialisation
│       ├── page.rs     # 8 KB page I/O + checksum
│       └── catalog.rs  # Document catalog page chain
├── src/bin/
│   └── mqdb.rs         # CLI (clap)
└── docs/
    └── STORAGE_FORMAT.md
```

## License

MIT
