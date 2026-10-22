# Vector Search

mq-db does not compute embeddings. Compute them with any model, store them in a custom table as JSON arrays, and rank with `cosine_similarity`, `dot_product` or `l2_distance`.

Key each embedding on `blocks.content_hash`, not `blocks.id`. The hash depends only on the block's type and content, so an embedding keeps matching its block when the block moves or other documents change.

```sql
-- Hand these to your embedding model.
SELECT DISTINCT content_hash, content FROM blocks;

-- One JSON object per line: {"content_hash": "9f86d0...", "embedding": [0.12, -0.03, ...]}
CREATE TABLE emb AS SELECT * FROM read_json('embeddings.jsonl');

SELECT b.content
FROM emb e JOIN blocks b ON b.content_hash = e.content_hash
ORDER BY cosine_similarity(e.embedding, '[0.1, 0.2, 0.3]') DESC
LIMIT 5;
```

A block whose content changed gets a new hash, so it simply stops matching. Find the blocks that still need an embedding:

```sql
SELECT DISTINCT b.content_hash, b.content
FROM blocks b LEFT JOIN emb e ON b.content_hash = e.content_hash
WHERE e.content_hash IS NULL;
```

Equal content shares one hash, so one embedding covers every copy. Use `(documents.path, anchor)` instead when each location needs its own row. `blocks.id` also works for a throwaway table, but it shifts whenever an earlier document gains or loses blocks. Integers compare equal to their text form (`1` and `'1'`), so a text column can join an integer key.

Every query scans the whole table, which is fast enough for tens of thousands of vectors (about one second for 20,000 vectors of 384 dimensions) but has no index.
