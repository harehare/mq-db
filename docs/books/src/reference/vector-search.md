# Vector Search

mq-db does not compute embeddings. Compute them with any model, keep them in a custom table as JSON arrays, and rank with `cosine_similarity`, `dot_product` or `l2_distance`.

## Key embeddings on `content_hash`

`blocks.id` shifts when an earlier document changes, so do not key embeddings on it. [`blocks.content_hash`](sql-schema.md) depends only on a block's type and content: an unchanged block keeps its embedding across re-indexing and moves, and an edited block has a new hash, so it shows up as missing.

## Workflow

```bash
# 1. Export the blocks that have no embedding yet (all of them the first time).
mq-db sql --db store.mq-db --format json \
  "SELECT DISTINCT content_hash, content FROM blocks WHERE block_type <> 'yaml'" > todo.json

# 2. Embed `content` with your model and write one JSON object per line:
#    {"content_hash": "1c8d281685b6b265", "embedding": [0.12, -0.03, ...]}
#    -> embeddings.jsonl

# 3. Load them.
mq-db sql --db store.mq-db \
  "CREATE TABLE emb AS SELECT * FROM read_json('embeddings.jsonl')"
```

After documents change and are re-indexed, list what needs embedding and append it:

```sql
SELECT DISTINCT content_hash, content FROM blocks
WHERE content_hash NOT IN (SELECT content_hash FROM emb);

INSERT INTO emb SELECT * FROM read_json('new-embeddings.jsonl');
```

Drop embeddings of blocks that no longer exist:

```sql
CREATE OR REPLACE TABLE emb AS
SELECT * FROM emb WHERE content_hash IN (SELECT content_hash FROM blocks);
```

## Searching

```sql
SELECT d.path, b.anchor, b.content
FROM emb e
JOIN blocks b ON b.content_hash = e.content_hash
JOIN documents d ON d.id = b.document_id
ORDER BY cosine_similarity(e.embedding, '[0.1, 0.2, 0.3]') DESC
LIMIT 5;
```

Identical text in several places shares one embedding and appears once per location. Use `(documents.path, anchor)` instead when each location needs its own row. Integers compare equal to their text form (`1` and `'1'`), so a text column can join an integer key.

Pass the query vector as a string literal. A row whose vector is invalid or has a different length scores `NULL`; `NULL` sorts lowest, so those rows come last under `DESC`.

## Limits

Every query scans the whole table, which is fast enough for tens of thousands of vectors (about one second for 20,000 vectors of 384 dimensions) but has no index.
