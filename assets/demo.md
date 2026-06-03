# mq-db Demo

Markdown-specialized embedded database with SQL and mq query support.

## Architecture

The database treats every Markdown element as a typed Block stored in a flat
vector with an interval index for O(1) hierarchy queries.

### Block Model

Each block carries:

- `block_type` — heading, paragraph, code, list, …
- `pre` / `post` — interval index for hierarchy
- `content` — raw Markdown text
- `properties` — row-polymorphic extras (depth, lang, …)

### Index Layers

Three complementary layers, cheapest first:

1. **Zone Maps** — document-level skip (built once per file)
2. **Interval Index** — section hierarchy via Pre-Post Order
3. **Secondary Indexes** — BitmapIndex, BTreeIndex, HashIndex

## Query Engines

### SQL Engine

Custom `sqlparser`-based evaluator — no SQLite dependency.

```sql
SELECT block_type, count(*) FROM blocks GROUP BY block_type;
```

Built-in functions:

- `under(pre, post, anc_pre, anc_post)` — O(1) ancestor check
- `mq(program, content)` — inline mq expression
- `json_extract(json, path)` — JSON path extraction

### mq Engine

```mq
.h1
select(.code_lang == "rust")
.h | select(.depth == 2)
```

## Storage

Custom 8 KB page file with atomic writes (`<path>.tmp` → rename).

```
Page 0  File Header   magic · version · page count
Page 1  Catalog       doc_id → first_block_page · ZoneMaps
Page 2+ Block Data    linked page chains · overflow pages
```

## Installation

```bash
curl -fsSL https://raw.githubusercontent.com/harehare/mq-db/main/bin/install.sh | bash
```

## License

MIT
