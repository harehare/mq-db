# Architecture

Internal design of mq-db.

## Block Model

Every Markdown element is parsed into a `Block`:

```rust
struct Block {
    id: u32,
    document_id: u32,
    block_type: BlockType,
    content: String,
    span: Option<Span>,
    pre: u32,
    post: u32,
    properties: Properties,
}
```

### Block Types

| Type | Properties |
|------|------------|
| `Heading` | `depth`, `slug` |
| `Code` | `lang`, `meta` |
| `List` | `ordered`, `level`, `checked` |
| `Yaml` / `Toml` | front-matter keys |

## Interval Index

Heading hierarchy is encoded as `(pre, post)` pairs using Pre-Post Order traversal.

An ancestor check becomes a single integer comparison:

```
A is_under B  ↔  B.pre < A.pre AND A.post < B.post
```

This runs in O(1) — no tree traversal at query time.

## Index Layers

Three complementary layers, cheapest-first:

### Layer 1 — Zone Maps

Built once per document and stored in the `.mq-db` file.
Checked before any block is read to skip irrelevant documents.

| Field | Purpose |
|-------|---------|
| `heading_contents` | Skip if heading text absent |
| `code_languages` | Skip if language tag absent |
| `max_heading_depth` | Skip if depth impossible |
| `tags` | Skip if tag filter cannot match |

### Layer 2 — Interval Index

Determines the candidate block range for a section query.
Combined with Zone Maps, sections of large multi-file stores resolve without reading blocks from irrelevant files.

### Layer 3 — Secondary Indexes

| Index | Columns | Structure | Complexity |
|-------|---------|-----------|------------|
| `BitmapIndex` | `block_type` | Inverted list | O(1) + O(k) |
| `BTreeIndex` | `pre`, `post` | `BTreeMap` | O(log n) |
| `HashIndex` | `content`, `lang`, `depth` | `HashMap` | O(1) avg |

SQL predicate pushdown selects the cheapest available index hint.

## SQL Engine

A custom `sqlparser`-based evaluator with no SQLite dependency.
Supports `SELECT`, `JOIN`, `WHERE`, `GROUP BY`, `ORDER BY`, `LIMIT`, and DDL.

### Predicate Pushdown

```
SQL WHERE predicate
  ├── block_type = '...'  →  BitmapIndex
  ├── pre = N             →  BTreeIndex (point)
  ├── pre BETWEEN N AND M →  BTreeIndex (range)
  ├── content = '...'     →  HashIndex
  ├── lang = '...'        →  HashIndex
  └── other               →  Full Scan
```

## Storage Format

Custom 8 KB page file with atomic writes.

```
Page 0   File Header   magic · version · page count
Page 1   Catalog       doc_id → first_block_page · ZoneMaps
Page 2+  Block Data    linked page chains · overflow pages
```

Writes go to `<path>.tmp`, then atomically renamed to `<path>`.

## Query Pipeline

```
SQL Query
  → Zone Map skip        (document level)
  → Interval Index       (section scope)
  → Secondary Index hint (block lookup)
  → Full Scan fallback
  → Result rows
```
