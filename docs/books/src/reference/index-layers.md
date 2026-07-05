# Index Layers

`mq-db` applies three complementary index layers, cheapest-first:

```text
SQL Query
   │
   ▼
Layer 1 — Zone Maps (document skip) ────skip───▶ ✗ irrelevant docs
   │ relevant docs
   ▼
Layer 2 — Interval Index (section scope)
   │ candidate blocks
   ▼
Layer 3 — Secondary Indexes (block lookup)
   │  BitmapIndex · BTreeIndex · HashIndex
   │  (no hint ──▶ Full Scan)
   ▼
Result Rows
```

## Layer 1 — Zone Maps (document-level skip)

Built once per document and stored in the `.mq-db` file. Checked before any block is read.

**Via SQL** — `SqlEngine` derives a skip automatically from the `WHERE` clause, for a single, non-`JOIN`ed `SELECT ... FROM blocks`:

| `WHERE` conjunct | Skips documents where… |
| --- | --- |
| `lang = 'X'` | `code_languages` doesn't contain `X` |
| `depth = N` (`N > 0`) | `N` exceeds `max_heading_depth` |
| `block_type = 'heading' AND content = 'X'` | `heading_contents` has no case-insensitive match for `X` |

**Via the Rust API** — `store.query().documents(|doc| ...)` lets you filter on *any* zone-map field yourself (`heading_slugs`, `frontmatter_keys`, `title`, `tags`, …), not just the patterns `SqlEngine` recognizes automatically.

## Layer 2 — Interval Index (section hierarchy)

Heading hierarchy is encoded as `(pre, post)` pairs via Pre-Post Order (Nested Set) traversal:

```text
# Doc                 pre=0  · post=11
├── ## Section A      pre=2  · post=7
│   ├── Paragraph     pre=3  · post=4
│   └── Code          pre=5  · post=6
└── ## Section B      pre=8  · post=11
    └── Paragraph     pre=9  · post=10
```

`A is_under B` ↔ `B.pre < A.pre AND A.post < B.post` — `O(1)`, no tree traversal. This is exactly what the SQL [`under()`](sql-functions.md) function and the Rust `.under_heading()` query-builder method check.

## Layer 3 — Secondary Indexes (block-level fast lookup)

| Index | Column(s) | Structure | Complexity |
| --- | --- | --- | --- |
| `BitmapIndex` | `block_type` | Inverted list per type | `O(1)` key + `O(k)` iterate |
| `BTreeIndex` | `pre`, `post` | `BTreeMap` | `O(log n)` point, `O(log n + k)` range |
| `HashIndex` | `content`, `lang`, `depth` | `HashMap` | `O(1)` average |

SQL predicate pushdown picks an `IndexHint` based on the shape of the `WHERE` predicate:

| `WHERE` predicate | Index used |
| --- | --- |
| `block_type = '...'` | `BitmapIndex` |
| `pre = N` | `BTreeIndex` (point lookup) |
| `pre BETWEEN N AND M` | `BTreeIndex` (range scan) |
| `content = '...'` | `HashIndex` |
| `lang = '...'` | `HashIndex` |
| `depth = N` | `HashIndex` |
| anything else | Full scan |
