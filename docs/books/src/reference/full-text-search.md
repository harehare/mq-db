# Full-Text Search

`match()`/`score()`/`bm25()` are SQL functions backed by a persisted per-document `TermIndex` (a tokenized inverted index), for index-accelerated relevance search over block content.

```sql
SELECT content, bm25(content, 'error handling') AS relevance
FROM blocks
WHERE match(content, 'error handling')
ORDER BY relevance DESC;
```

- `match(content, query)`: true iff every tokenized term in `query` appears in `content`. Index-accelerated when `content` is a bare column reference and `query` is a string literal.
- `score(content, query)`: a simple term-frequency relevance score for `query` against `content` (no IDF weighting; see [Storage Format](storage-format.md)).
- `bm25(content, query)`: Okapi BM25 relevance score, IDF-weighted across the whole store (`k1=1.2`, `b=0.75`, not configurable). Prefer this over `score()` when documents vary a lot in length or in how common a query term is.

## `find` (CLI shortcut)

`mq-db find` is a shortcut for `match()`/`score()`, no SQL needed. It falls back to a case-insensitive substring match too, so partial CJK queries still hit (see [`tokenize`](https://github.com/harehare/mq-db/blob/main/src/indexes.rs)'s known limitations). Results show a snippet centred on the match, with matched terms highlighted when stdout is a terminal (respects `NO_COLOR`):

```bash
mq-db find "error handling" --db store.mq-db
mq-db find "error handling" --db store.mq-db -n 5 -F json   # top 5, JSON
```

```text
docs/API.md  ¶   0.67  Error handling follows RFC 7807 problem details...
docs/API.md  #   0.25  Error Handling

2 matches
```
