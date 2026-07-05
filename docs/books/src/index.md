# Introduction

`mq-db` treats Markdown documents as **structured, hierarchical databases** rather than plain text.

It parses Markdown into a flat block list annotated with an **interval index** (Nested Set / Pre-Post Order), which turns heading-hierarchy questions — "is this paragraph inside that section?" — into a single `O(1)` integer comparison instead of a tree walk. Documents can be queried with **SQL** or **[mq](https://github.com/harehare/mq)**, and persisted to a compact custom page-file format with no SQLite dependency.

> This project is under active development and the API may change.

## Why Markdown-as-database?

Markdown files already have implicit structure — headings nest sections, code blocks carry a language, front matter carries metadata. `mq-db` makes that structure queryable directly:

```sql
SELECT block_type, count(*) FROM blocks GROUP BY block_type;
```

```mq
.h1
```

Both engines run against the same underlying block store, so you can pick whichever query language fits the task: SQL for joins, aggregates, and ad-hoc analysis; mq for Markdown-shaped transformations and selectors.

## How it fits together

```text
Markdown File(s)
      │  CST Parser (mq-markdown)
      ▼
Block Tree (heading · paragraph · code · list · …)
      │  Interval Index + Secondary Indexes
      ▼
Flat Block Vector (pre/post integers)
      │
      ├── BitmapIndex   (block_type)
      ├── BTreeIndex    (pre / post)
      ├── HashIndex     (content / lang / depth)
      ├── Zone Maps     (per-document stats)
      │
      ├── SQL Engine   (sqlparser — custom native evaluator)
      └── mq Engine    (mq-lang evaluator)
```

## Features

- **Flat block storage** — every Markdown element becomes a typed `Block` with row-polymorphic properties
- **O(1) hierarchy queries** — interval index (`pre`/`post`) makes ancestor/descendant checks a single integer comparison
- **Three-layer secondary indexes** — `BitmapIndex` (block type), `BTreeIndex` (pre/post), `HashIndex` (content/lang/depth) for fast SQL predicate pushdown
- **Zone Maps** — per-document statistics skip irrelevant files before scanning any blocks
- **Dual query engines** — SQL via a custom `sqlparser`-based evaluator, and `mq` via `mq-lang`
- **DDL support** — `CREATE TABLE`, `INSERT INTO`, `DROP TABLE` for in-memory custom tables
- **Comprehensive SQL function library** — string, numeric, null-handling, `CASE`, and aggregate functions comparable to a general-purpose RDBMS
- **`mq()` scalar function** — run an mq program against Markdown content inline in SQL
- **Custom page-file persistence** — 8 KB fixed pages, checksums, atomic writes
- **CLI + interactive REPL + TUI** — full terminal experience

Keep reading in [Getting Started](start/index.md), or jump straight to the [SQL Reference](reference/index.md) if you already have a `.mq-db` store.
