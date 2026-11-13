# Vector Search

`mq-db embed` computes embeddings through any OpenAI-compatible `/embeddings` API and keeps them in a custom table as JSON arrays; rank them with `cosine_similarity`, `dot_product` or `l2_distance`. You can also compute them yourself and load them, see [Bring your own embeddings](#bring-your-own-embeddings).

## Key embeddings on `content_hash`

`blocks.id` shifts when an earlier document changes, so do not key embeddings on it. [`blocks.content_hash`](sql-schema.md) depends only on a block's type and content: an unchanged block keeps its embedding across re-indexing and moves, and an edited block has a new hash, so it shows up as missing.

## `mq-db embed`

```bash
# Ollama (no key needed)
MQ_DB_EMBED_ENDPOINT=http://localhost:11434/v1 MQ_DB_EMBED_MODEL=nomic-embed-text \
  mq-db embed --db store.mq-db

# OpenAI
export OPENAI_API_KEY=...
mq-db embed --db store.mq-db --model text-embedding-3-small
```

It embeds the blocks that have no row in the table yet, in batches, and deletes rows whose block no longer exists, so re-running after `mq-db index` only pays for what changed. Afterwards it makes sure the table has a current HNSW index named `<table>_hnsw`, so `vector_search()` stays fast: new embeddings are added to the existing index, and it is rebuilt in full only when rows were removed. Rows are written in a few large batches rather than one per API request, and a run that fails partway keeps what it already embedded, so running it again resumes. Front matter and empty blocks are skipped, and blocks with identical type and content share one embedding.

| Option | Default | |
|--------|---------|---|
| `--endpoint` (`MQ_DB_EMBED_ENDPOINT`) | `https://api.openai.com/v1` | Base URL; `/embeddings` is appended |
| `--model` (`MQ_DB_EMBED_MODEL`) | `text-embedding-3-small` | Model name sent to the API |
| `--api-key-env` | `OPENAI_API_KEY` | Name of the environment variable holding the key. The key is never read from the command line. |
| `--table` | `embeddings` | Table with columns `content_hash`, `embedding`, `model` |
| `--batch-size` | `64` | Texts per request |
| `--dimensions` | none | Forwarded to models that can shorten their vectors |
| `--rebuild` | off | Replace every stored embedding |
| `--no-index` | off | Do not build or refresh the [HNSW index](#hnsw-index) |

The table remembers which model produced its vectors. Running with a different `--model` is refused, because vectors from different models cannot be compared; pass `--rebuild` or use another `--table`.

Search it with the query from [Searching](#searching), or with an [HNSW index](#hnsw-index) for large tables. Failed requests are retried twice; text is sent to the configured endpoint, so use a local server for private documents.

## Bring your own embeddings

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
DELETE FROM emb WHERE content_hash NOT IN (SELECT content_hash FROM blocks);
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

## HNSW index

Scanning every row is fine for tens of thousands of vectors. Past that, build an approximate nearest-neighbour index (HNSW) on the vector column and search with `vector_search()`:

```sql
CREATE INDEX embeddings_hnsw ON embeddings USING hnsw (embedding);

SELECT d.path, b.anchor, b.content, v.score
FROM vector_search('embeddings', 'embedding', '[0.1, 0.2, 0.3]', 10) v
JOIN blocks b ON b.content_hash = v.content_hash
JOIN documents d ON d.id = b.document_id
ORDER BY v.score DESC;
```

`vector_search(table, column, '[query vector]', k [, ef])` returns the `k` rows whose `column` is most cosine-similar to the query, with all of the table's columns plus `score` (the cosine similarity). It is a table function, so join and filter its result like any table.

- **Without an index it still works**: it compares every row exactly. An index only makes it faster, so queries never fail because one is missing or out of date.
- **`ef`** is the number of candidates examined per search (default `max(8 * k, 64)`). Raise it for better recall at the cost of speed.
- **Build options**: `CREATE INDEX … USING hnsw (col) WITH (m = 16, ef_construction = 200)`. Larger values give better recall and a bigger, slower build. Leaving the name out names the index `<table>_<column>_hnsw`; `IF NOT EXISTS` and `DROP INDEX [IF EXISTS] name` work as usual, and `SHOW INDEXES` lists them.
- **Cosine similarity only.** Vectors are normalised when indexed, so the index ignores their length. Rows whose value is not a JSON array of numbers, or whose length differs from the first vector's, are left out of the index (the `CREATE INDEX` result says how many).
- **`INSERT` extends the index; other writes drop it.** Rows appended to the table are added to its index in place. An `UPDATE`, `DELETE`, `DROP TABLE` or `CREATE OR REPLACE TABLE` cannot be applied that way, so it drops the table's indexes; re-run `CREATE INDEX` afterwards (`mq-db embed` does this for you).
- The index is saved in the `.mq-db` file and read the first time it is needed. Older versions of `mq-db` open such a file normally; they just don't see the index.

### Accuracy and speed

HNSW is approximate: it can miss a true neighbour. How often depends on the data. Embeddings from real models cluster, which is the easy case; on measured data (20,000 clustered 384-dimensional vectors) the default `ef` found all of the true top 10 for every query. Uniform random noise is the hard case, where the same default found about a third and `ef = 1000` found 96%, so if recall matters, measure it on your own data by comparing against the exact query above.

Warm, in a long-running process such as `mq-db serve` or the library, searching 10,000 clustered 128-dimensional vectors takes about 30 µs through the index against 20 ms exactly. A one-off `mq-db sql` call also pays to open the store, which reads every custom table, so it improves less (roughly 2x on 20,000 × 384). Building took about 6 seconds for 20,000 × 384.

## Semantic search from the CLI

After `mq-db embed`, search by meaning without writing SQL:

```bash
mq-db find "how do I handle errors?" --semantic
```

It embeds the query through the same endpoint, using the model recorded in the table, and prints blocks in the same format as plain `find`, ranked by similarity. `--table`, `--endpoint`, `--api-key-env` and `--dimensions` work as they do for `embed`. If the query's vector size differs from the stored ones, pass the `--dimensions` you used when embedding.
