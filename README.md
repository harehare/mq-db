<div align="center">
  <img src="assets/logo.svg" width="96" height="96" />

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
mqdb index docs/ --recursive --output store.mqdb
mqdb index README.md DESIGN.md
```

```
  ✓ docs/DESIGN.md
  ✓ docs/API.md

Indexed 2 files → store.mqdb
```

### List indexed documents

```bash
mqdb list --db store.mqdb
```

```
┌──────┬────────────────────────────────────────────────────┬────────┬──────────┐
│   ID │ Path / Title                                       │ Blocks │ Tags     │
├──────┼────────────────────────────────────────────────────┼────────┼──────────┤
│    0 │ docs/DESIGN.md                                     │    142 │          │
│    1 │ docs/API.md                                        │     87 │ api, v2  │
└──────┴────────────────────────────────────────────────────┴────────┴──────────┘
2 documents
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
mqdb mq ".h1" --db store.mqdb
mqdb mq 'select(.code_lang == "rust")' --db store.mqdb
```

### Interactive REPL

```bash
mqdb repl --db store.mqdb --mode sql
```

```
mqdb  (.help for commands  .quit to exit)
mode: sql  (.mode mq | .mode sql)

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
→ mq mode
mq> .h2
## Architecture
## Query Engine
```

### Structural linting

```bash
mqdb lint --db store.mqdb --depth 2
```

```
✗  1 violation  (H2 immediately followed by list)

  file                                      heading
  ────────────────────────────────────────  ──────────────────────────────
  docs/DESIGN.md                            "Quick Start"
```

### Statistics

```bash
mqdb stats --db store.mqdb
```

```
  Documents  5
  Blocks     632

  Block types
  ────────────────────────────────────────────────────────
   ¶  paragraph    ████████████████████░░░░   241  (38%)
   #  heading      ████████░░░░░░░░░░░░░░░░    89  (14%)
  {}  code         ███████░░░░░░░░░░░░░░░░░    73  (12%)
   •  list         ██████░░░░░░░░░░░░░░░░░░    58   (9%)

  Code languages
  ────────────────────────────────────────────────────────
  {}  rust         ████████████████████████    41  (57%)
  {}  python       ██████████░░░░░░░░░░░░░░    18  (25%)
  {}  bash         ███████░░░░░░░░░░░░░░░░░    14  (19%)
```

### Show document structure

```bash
mqdb show 0 --db store.mqdb
```

```
  docs/DESIGN.md
  title   Design Document
  blocks  142

  pre   post  type               content
  ────  ────  ────────────────   ──────────────────────────────────────────
     0   141  heading H1         Design Document
     2    55  heading H2         Architecture
     4    21  paragraph          The system is built on…
    22    37  heading H3           Query Engine
    24    36  code                   fn main() { … }
```

### TUI

```bash
mqdb tui --db store.mqdb
```

```
 mqdb  SQL  Tab:switch  i:input  j/k:nav  d/u:scroll  q:quit
┌─ Documents ──────────┬─ SQL ────────────────────────────────────────────────┐
│ DESIGN.md            │ SELECT block_type, count(*) FROM blocks GROUP BY b_  │
│   142 blocks         ├─ Results ────────────────────────────────────────────┤
│ API.md               │ ┌─────────────┬──────────┐                           │
│   87 blocks  API     │ │ block_type  │ count(*) │                           │
│ README.md            │ ├─────────────┼──────────┤                           │
│   34 blocks          │ │ paragraph   │ 48       │                           │
└──────────────────────┴──────────────────────────────────────────────────────┘
 5 docs  632 blocks  3 rows
```

**Keys:**

| Key | Action |
|---|---|
| `i` | Focus query input |
| `Esc` | Blur input |
| `Enter` | Run query |
| `Tab` | Toggle mq / SQL mode |
| `j` / `k` | Navigate document list |
| `d` / `u` | Scroll results down / up |
| `g` / `G` | Jump to top / bottom |
| `q` / `Ctrl+C` | Quit |

## Library API

```rust
use mqdb::{DocumentStore, SqlEngine, MqEngine, block::BlockType};

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
store.add_file("README.md")?;
let results = MqEngine::eval_store(".h1", &store)?;

// Structural lint
let violations = store.query().lint_heading_followed_by(2, &[BlockType::List]);

// Persist / load
store.save("store.mqdb")?;
let store = DocumentStore::load("store.mqdb")?;
```

## SQL Reference

### Virtual schema

```sql
SELECT id, path, title, tags FROM documents;

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
-- All text/code under a specific section (RAG extraction)
SELECT b.block_type, b.content
FROM blocks b
WHERE under(b.pre, b.post,
  (SELECT pre FROM blocks WHERE block_type = 'heading' AND content = 'Architecture'),
  (SELECT post FROM blocks WHERE block_type = 'heading' AND content = 'Architecture'))
  AND b.block_type IN ('paragraph', 'code')
ORDER BY b.pre;

-- H2 headings immediately followed by a list (structural lint)
SELECT d.path, h.content AS heading
FROM blocks h
JOIN blocks nxt ON nxt.document_id = h.document_id AND nxt.pre = h.pre + 1
JOIN documents d ON d.id = h.document_id
WHERE h.block_type = 'heading' AND depth = 2 AND nxt.block_type = 'list';

-- Documents containing Python code
SELECT DISTINCT d.path
FROM documents d JOIN blocks b ON b.document_id = d.id
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

| Block type | Properties |
|---|---|
| `Heading` | `{ "depth": 2, "slug": "architecture" }` |
| `Code` | `{ "lang": "rust", "meta": "no_run" }` |
| `List` | `{ "ordered": false, "level": 1, "checked": null }` |
| `Yaml` / `Toml` | parsed front-matter keys (`"title"`, `"tags"`, …) |

### Index layers

mqdb applies three complementary index layers, cheapest-first.

#### Layer 1 — Zone Maps (document-level skip)

Built once per document and stored in the `.mqdb` file. Checked before any block is read:

| Field | Skips documents where… |
|---|---|
| `heading_contents` | The requested heading text is absent |
| `code_languages` | The requested language tag is absent |
| `max_heading_depth` | The requested depth cannot exist |
| `tags` | The tag filter cannot match |

#### Layer 2 — Interval Index (section hierarchy)

Heading hierarchy encoded as `(pre, post)` pairs via Pre-Post Order (Nested Set) traversal:

```
# Doc          pre=0, post=11
## Section A   pre=2, post=7
  Paragraph    pre=3, post=4
  Code         pre=5, post=6
## Section B   pre=8, post=11
  Paragraph    pre=9, post=10
```

`A is_under B` ↔ `B.pre < A.pre AND A.post < B.post` — O(1), no tree traversal.

#### Layer 3 — Secondary Indexes (block-level fast lookup)

| Index | Column(s) | Structure | Complexity |
|---|---|---|---|
| `BitmapIndex` | `block_type` | Inverted list per type | O(1) key + O(k) iterate |
| `BTreeIndex` | `pre`, `post` | `BTreeMap` | O(log n) point, O(log n + k) range |
| `HashIndex` | `content`, `lang`, `depth` | `HashMap` | O(1) average |

SQL predicate pushdown picks an `IndexHint`:

```
WHERE block_type = 'heading'    → BitmapIndex
WHERE pre = 42                  → BTreeIndex  (point)
WHERE pre BETWEEN 10 AND 50     → BTreeIndex  (range)
WHERE content = 'Architecture'  → HashIndex
WHERE lang = 'rust'             → HashIndex
WHERE depth = 2                 → HashIndex
(other)                         → FullScan
```

### Storage format

Custom 8 KB page file:

```
Page 0  │ File header  (magic 0x4D514442, version, page count)
Page 1  │ Catalog      (doc_id → first_block_page, num_blocks, ZoneMaps)
Page 2+ │ Block data   (linked page chains, overflow pages)
```

Writes are atomic: data goes to `<path>.tmp` then renamed to `<path>` on success.

## License

[MIT](LICENSE)
