# EXPLAIN / EXPLAIN ANALYZE

See which index (zone map, `BitmapIndex`, `BTreeIndex`, `HashIndex`, `TermIndex`, or full scan) a query's `WHERE`/`JOIN` resolves to, without running it. When more than one index is viable for the same `WHERE` clause, the choice is **cost-based**: each candidate's real matching-block count is read from its index, and the cheapest wins; `EXPLAIN` shows every candidate considered.

```bash
mq-db sql "EXPLAIN SELECT content FROM blocks WHERE block_type = 'code' AND lang = 'json'" --db store.mq-db
```

```text
┌──────────────────────┬────────────────────────────────────────────────────────────────────────┐
│ step                 │ detail                                                                 │
├──────────────────────┼────────────────────────────────────────────────────────────────────────┤
│ query:from           │ blocks (blocks)                                                        │
│ query:where          │ HashIndex(lang = 'json') used (est. 2 row(s); also considered:          │
│                       │   BitmapIndex(block_type IN (code)) [est. 289])                        │
│ query:zone-map       │ eligible via lang                                                       │
│ query:where-recheck  │ row-by-row (full predicate re-evaluated after scan)                     │
└──────────────────────┴────────────────────────────────────────────────────────────────────────┘
```

Add `ANALYZE` to also run the query and report actual row counts, document-skip counts, and timing:

```bash
mq-db sql "EXPLAIN ANALYZE SELECT * FROM blocks WHERE match(content, 'error handling')" --db store.mq-db
```

`WITH` CTEs are described separately (`cte:<name>:...` steps) before the outer query; `JOIN`s report whether they resolve to a hash join (equi-join `ON`) or a nested loop. Only `SELECT` queries are supported; `EXPLAIN` on other statements is rejected.
