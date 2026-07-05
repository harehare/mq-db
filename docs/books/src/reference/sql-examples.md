# Example Queries

```sql
-- All text/code under a specific section (RAG extraction)
SELECT b.block_type, b.content
FROM blocks b
WHERE under(b.pre, b.post,
  (SELECT pre FROM blocks WHERE block_type = 'heading' AND content = 'Architecture'),
  (SELECT post FROM blocks WHERE block_type = 'heading' AND content = 'Architecture'))
  AND b.block_type IN ('paragraph', 'code')
ORDER BY b.pre;

-- Extract H1 title from code block content via the mq() scalar function
SELECT mq('.h1 | to_text', content) AS title
FROM blocks
WHERE block_type = 'code' AND lang = 'markdown';

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

-- Bucket headings by depth and summarize with string/numeric functions
SELECT
  CASE WHEN depth <= 1 THEN 'top-level' ELSE 'nested' END AS bucket,
  count(*),
  group_concat(initcap(trim(content)), ', ') AS headings
FROM blocks
WHERE block_type = 'heading'
GROUP BY CASE WHEN depth <= 1 THEN 'top-level' ELSE 'nested' END;
```

## Mixing mq and SQL

The `mq()` scalar function lets a SQL query delegate per-row Markdown transformation to mq, which is convenient when a block's `content` is itself a Markdown snapshot (e.g. a fenced code block containing Markdown, as in the `to_text` example above).

From the CLI you can also move between the two engines freely, since both `mq-db sql` and `mq-db mq` support `--format markdown`/`--format json`, so the output of one can feed a pipeline built around the other.
