# Query Guide

A practical guide to querying Markdown with mq-db.

## SQL Queries

mq-db supports a subset of SQL executed by a custom evaluator — no SQLite required.

### Basic SELECT

```sql
SELECT id, block_type, content FROM blocks LIMIT 10;
```

### Filtering by Type

```sql
SELECT content, depth FROM blocks
WHERE block_type = 'heading'
ORDER BY pre;
```

### Aggregation

```sql
SELECT block_type, count(*) AS total
FROM blocks
GROUP BY block_type
ORDER BY total DESC;
```

### Joining Documents

```sql
SELECT d.path, b.content
FROM blocks b
JOIN documents d ON d.id = b.document_id
WHERE b.block_type = 'heading' AND b.depth = 1;
```

### Hierarchy with under()

The `under()` function performs an O(1) ancestor check using the interval index.

```sql
SELECT b.block_type, b.content
FROM blocks b
WHERE under(
  b.pre, b.post,
  (SELECT pre  FROM blocks WHERE block_type = 'heading' AND content = 'Architecture'),
  (SELECT post FROM blocks WHERE block_type = 'heading' AND content = 'Architecture')
)
ORDER BY b.pre;
```

### Inline mq with mq()

Run an mq program against Markdown content directly inside SQL.

```sql
SELECT mq('.h1 | to_text', content) AS title
FROM blocks
WHERE block_type = 'code' AND lang = 'markdown';
```

## mq Queries

mq-lang is a jq-inspired query language for Markdown blocks.

### Select Headings

```mq
.h1
.h2
.h
```

### Filter by Property

```mq
select(.code_lang == "rust")
select(.depth == 2)
.code | select(.lang != null)
```

### Combine Filters

```mq
.h | select(.depth >= 2 and .depth <= 3)
```

## DDL — Custom Tables

### Create from Query

```sql
CREATE TABLE headings AS
SELECT content, depth FROM blocks WHERE block_type = 'heading';
```

### Explicit Schema

```sql
CREATE TABLE notes (id TEXT, body TEXT);
INSERT INTO notes VALUES ('1', 'First note');
INSERT INTO notes VALUES ('2', 'Second note');
```

### Inspect and Drop

```sql
SHOW TABLES;
DESC notes;
DROP TABLE notes;
```

## Output Formats

Every query command supports `--format`:

| Format | Flag |
|--------|------|
| Table (default) | `--format table` |
| JSON | `--format json` |
| CSV | `--format csv` |
| TSV | `--format tsv` |
| Markdown | `--format markdown` |
| HTML | `--format html` |

```bash
mq-db sql "SELECT * FROM blocks LIMIT 5" --db store.mq-db --format json
mq-db mq ".h1" --db store.mq-db --format markdown
```
