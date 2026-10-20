# Vector Search

mq-db does not compute embeddings. Compute them with any model, store them in a custom table as JSON arrays, and rank with `cosine_similarity`, `dot_product` or `l2_distance`.

```sql
-- One JSON object per line: {"id": 1, "embedding": [0.12, -0.03, ...]}
CREATE TABLE emb AS SELECT * FROM read_json('embeddings.jsonl');

SELECT id FROM emb
ORDER BY cosine_similarity(embedding, '[0.1, 0.2, 0.3]') DESC
LIMIT 5;
```

To get the matching text, join on `blocks.id`. Integers compare equal to their text form (`1` and `'1'`), so a text column can join an integer key:

```sql
SELECT b.content
FROM emb e JOIN blocks b ON b.id = e.block_id
ORDER BY cosine_similarity(e.embedding, '[0.1, 0.2, 0.3]') DESC
LIMIT 5;
```

Every query scans the whole table, which is fast enough for tens of thousands of vectors (about one second for 20,000 vectors of 384 dimensions) but has no index.
