<div align="center">

<h1>mqdb</h1>

**Markdown-specialized embedded database with interval-indexed block storage and hierarchical query support.**

[![ci](https://img.shields.io/github/actions/workflow/status/harehare/mqdb/ci.yml?logo=github-actions&label=ci)](https://github.com/harehare/mqdb/actions)
[![crates.io](https://img.shields.io/crates/v/mqdb?logo=rust)](https://crates.io/crates/mqdb)
[![license](https://img.shields.io/crates/l/mqdb)](LICENSE)

</div>

`mqdb` treats Markdown documents as **structured, hierarchical databases** rather than plain text. It parses Markdown into a flat block list with an **interval index** (Nested Set / Pre-Post Order), enabling O(1) section hierarchy queries. Documents can be queried with **SQL** or **[mq](https://github.com/harehare/mq)** and persisted to a compact custom page-file format.

```
[Markdown File]
      │
      ▼  CST Parser (mq-markdown)
[Block Tree]  ─── (heading, paragraph, code, list, …)
      │
      ▼  Interval Index + Secondary Indexes
[Flat Block Vector]  pre/post integers + BitmapIndex / BTreeIndex / HashIndex
      │
      ├── SQL Engine  (sqlparser — custom native evaluator, no SQLite)
      └── mq Engine   (mq-lang evaluator)
```

> [!IMPORTANT]
> This project is under active development and the API may change.

## Features

- **Flat block storage** — every Markdown element becomes a typed `Block` with row-polymorphic properties
- **O(1) hierarchy queries** — interval index (`pre`/`post`) makes ancestor/descendant checks a single integer comparison
- **Three-layer secondary indexes** — `BitmapIndex` (block type), `BTreeIndex` (pre/post), `HashIndex` (content/lang/depth) for fast SQL predicate pushdown
- **Zone Maps** — per-document statistics skip irrelevant files before scanning any blocks
- **Dual query engines** — SQL via a custom `sqlparser`-based evaluator, and `mq` via `mq-lang`
- **Custom page-file persistence** — 8 KB fixed pages, checksums, atomic writes
- **CLI + interactive REPL + TUI** — full terminal experience

## Installation

```bash
git clone https://github.com/harehare/mqdb
cd mqdb
cargo build --release
# binary: target/release/mqdb
```

> **Requires:** the [`mq`](https://github.com/harehare/mq) repository checked out as a sibling directory (`../mq`).

## CLI Usage

### Index Markdown files

```bash
# Index a directory recursively → store.mqdb
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
block_type    count(*)
────────────────────────
paragraph     48
heading       21
code          15
(3 rows)
```

**Hierarchy query with `under()`** — find all content inside a specific section:

```bash
mqdb sql "
  SELECT b.block_type, b.content
  FROM blocks b
  WHERE under(b.pre, b.post,
    (SELECT pre FROM blocks WHERE block_type = 'heading' AND content = 'Architecture'),
    (SELECT post FROM blocks WHERE block_type = 'heading' AND content = 'Architecture'))
  ORDER BY b.pre
" --db store.mqdb
```

### mq queries

```bash
# Extract all H1 headings
mqdb mq ".h1" --db store.mqdb

# Extract all Rust code blocks
mqdb mq 'select(.code_lang == "rust")' --db store.mqdb
```

### Interactive REPL

```bash
mqdb repl --db store.mqdb --mode sql
```

```
mqdb REPL  (type .help for commands, .quit to exit)
Mode: sql  (.mode mq | .mode sql to switch)

sql> SELECT content FROM blocks WHERE block_type = 'heading' LIMIT 3;
content
──────────────────
Overview
Architecture
Query Engine
(3 rows)

sql> .mode mq
Switched to mq mode.
mq> .h2
## Architecture
## Query Engine
```

### Structural linting

Check that H2 headings are not immediately followed by a list (no intro paragraph):

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
│                  │  block_type    count(*)                       │
│                  │  paragraph     48                             │
│                  │  heading       21                             │
└──────────────────┴──────────────────────────────────────────────┘
  [j/k: navigate]  [i: focus input]  [Esc: blur]  [Enter: run]
```

**Keys:**

| Key | Action |
|---|---|
| `i` | Focus query input |
| `Esc` | Blur input |
| `Enter` | Run query |
| `Tab` | Toggle mq / SQL mode |
| `j` / `k` | Navigate document list |
| `PgDn` / `PgUp` | Scroll results |
| `q` / `Ctrl+C` | Quit |

## Library API

```rust
use mqdb::{DocumentStore, SqlEngine, MqEngine, block::BlockType};

// Build an in-memory store
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

// mq engine (operates on file-backed documents)
store.add_file("README.md")?;
let results = MqEngine::eval_store(".h1", &store)?;

// Structural lint
let q = store.query();
let violations = q.lint_heading_followed_by(2, &[BlockType::List]);

// Persist to disk (atomic write via .tmp rename)
store.save("store.mqdb")?;

// Load from disk
let store = DocumentStore::load("store.mqdb")?;
```

## SQL Reference

### Virtual schema

The SQL engine exposes two virtual tables over the in-memory store:

```sql
-- Indexed documents
SELECT id, path, title, tags FROM documents;

-- All blocks from all documents
SELECT id, document_id, block_type, content, pre, post,
       depth, lang, properties FROM blocks;
```

### Built-in functions

| Function | Description |
|---|---|
| `under(pre, post, anc_pre, anc_post)` | O(1) interval ancestor check |
| `json_extract(json, path)` | Extract a value from a JSON string |
| `count(*) / min / max / sum / avg` | Aggregate functions |
| `lower / upper / length / coalesce` | Scalar utilities |

### Example queries

```sql
-- RAG: extract all text/code under a specific section
SELECT b.block_type, b.content
FROM blocks b
WHERE under(b.pre, b.post,
  (SELECT pre FROM blocks WHERE block_type = 'heading' AND content = 'Architecture'),
  (SELECT post FROM blocks WHERE block_type = 'heading' AND content = 'Architecture'))
  AND b.block_type IN ('paragraph', 'code')
ORDER BY b.pre;

-- Linter: H2 headings immediately followed by a list
SELECT d.path, h.content AS heading, nxt.block_type AS next_type
FROM blocks h
JOIN blocks nxt ON nxt.document_id = h.document_id AND nxt.pre = h.pre + 1
JOIN documents d ON d.id = h.document_id
WHERE h.block_type = 'heading'
  AND depth = 2
  AND nxt.block_type = 'list';

-- Documents containing Python code
SELECT DISTINCT d.path
FROM documents d
JOIN blocks b ON b.document_id = d.id
WHERE b.block_type = 'code' AND lang = 'python';
```

## Architecture

### Block model

Every Markdown element becomes a `Block`:

```rust
struct Block {
    id: u32,
    document_id: u32,
    block_type: BlockType,  // Heading, Paragraph, Code, List, …
    content: String,
    span: Option<Span>,     // line/column for editor sync
    pre: u32,               // interval index pre-order
    post: u32,              // interval index post-order
    properties: Properties, // row-polymorphic extra attributes
}
```

Row-polymorphic property examples:

| Block type | Properties |
|---|---|
| `Heading` | `{ "depth": 2, "slug": "architecture" }` |
| `Code` | `{ "lang": "rust", "meta": "no_run" }` |
| `List` | `{ "ordered": false, "level": 1, "checked": null }` |
| `Yaml` | parsed front-matter keys (`"title"`, `"tags"`, …) |

### Index layers

mqdb applies three complementary index layers, cheapest-first, to avoid unnecessary work.

#### Layer 1 — Zone Maps (document-level skip)

Built once per document at parse time and stored in the `.mqdb` file. Checked before any block in a document is read:

| Field | Skips documents where… |
|---|---|
| `heading_contents` | The requested heading text is absent |
| `heading_slugs` | The requested heading slug is absent |
| `code_languages` | The requested language tag is absent |
| `max_heading_depth` | The requested depth cannot exist |
| `tags` | The tag filter cannot match |

```rust
// This skips any document that has no Rust code before scanning a single block
store.query()
    .documents(|doc| doc.zone_maps.code_languages.contains("rust"))
    .code_lang("rust")
    .blocks();
```

#### Layer 2 — Interval Index (section hierarchy)

The heading hierarchy is encoded as `(pre, post)` integer pairs via a Pre-Post Order (Nested Set) traversal assigned when blocks are built from the AST:

```
# Doc          pre=0, post=11
## Section A   pre=2, post=7
  Paragraph    pre=3, post=4
  Code         pre=5, post=6
## Section B   pre=8, post=11
  Paragraph    pre=9, post=10
```

**Ancestor check:** `A is_under B` ↔ `B.pre < A.pre AND A.post < B.post` — O(1), no tree traversal.

This powers both the `under_heading` / `under_interval` query API and the `under()` SQL function.

#### Layer 3 — Secondary Indexes (block-level fast lookup)

Three per-document indexes are built at parse time and consulted by the SQL engine's predicate pushdown to avoid full block scans:

| Index | Column(s) | Structure | Complexity |
|---|---|---|---|
| `BitmapIndex` | `block_type` | Inverted list per type | O(1) key + O(k) iterate |
| `BTreeIndex` | `pre`, `post` | `BTreeMap` sorted by value | O(log n) point, O(log n + k) range |
| `HashIndex` | `content`, `lang`, `depth` | `HashMap` | O(1) average |

The SQL planner picks an `IndexHint` for each query:

```
WHERE block_type = 'heading'    → BitmapIndex  (inverted list lookup)
WHERE block_type IN (…)         → BitmapIndex  (union of lists)
WHERE pre = 42                  → BTreeIndex   (point lookup)
WHERE pre BETWEEN 10 AND 50     → BTreeIndex   (range scan)
WHERE content = 'Architecture'  → HashIndex    (exact match, lowercased)
WHERE lang = 'rust'             → HashIndex    (lang lookup)
WHERE depth = 2                 → HashIndex    (depth lookup)
(other predicates)              → FullScan     (linear scan fallback)
```

All three beat an O(n) full scan when the number of matching blocks `k` is much smaller than the total `n`.

### Storage format

Custom 8 KB page file with linked page chains:

```
Page 0  │ File header  (magic 0x4D514442, version, page count)
Page 1  │ Catalog      (doc_id → first_block_page, num_blocks, ZoneMaps)
Page 2+ │ Block data   (PAGE_TYPE_BLOCK_DATA head + PAGE_TYPE_OVERFLOW continuations)
```

Writes are atomic: data goes to `<path>.tmp` then renamed to `<path>` on success.

## Project structure

```
mqdb/
├── src/
│   ├── lib.rs          # Public API surface
│   ├── block.rs        # Block, BlockType, Properties, PropertyValue
│   ├── document.rs     # Document, ZoneMaps
│   ├── index.rs        # build_blocks(): mq-markdown AST → interval-indexed blocks
│   ├── indexes.rs      # BitmapIndex, BTreeIndex, HashIndex, IndexHint
│   ├── query.rs        # Chainable query builder
│   ├── sql.rs          # SqlEngine: custom sqlparser-based evaluator
│   ├── mq_engine.rs    # MqEngine: mq-lang wrapper
│   ├── tui.rs          # ratatui TUI
│   ├── store.rs        # DocumentStore: add, query, save, load
│   ├── error.rs        # MqdbError
│   └── storage/        # Custom page-file persistence
│       ├── mod.rs
│       ├── codec.rs    # Block ↔ binary serialisation
│       ├── page.rs     # 8 KB page I/O + checksum
│       └── catalog.rs  # Document catalog page chain
└── src/bin/
    └── mqdb.rs         # CLI (clap)
```

## License

MIT
