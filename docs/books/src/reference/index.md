# Reference

Technical reference for the SQL surface and the on-disk/in-memory data model.

- [Virtual Schema](sql-schema.md): the `documents` and `blocks` tables
- [Built-in Functions](sql-functions.md): mq-db-specific, string, numeric, null-handling, and aggregate functions
- [DDL Statements](sql-ddl.md): `CREATE TABLE`, `INSERT INTO`, `DROP TABLE`, views, `ATTACH`/`DETACH`, and friends
- [CTEs (WITH / WITH RECURSIVE)](cte.md): named subqueries and recursive queries
- [Full-Text Search](full-text-search.md): `match()`/`score()`/`bm25()` and the `find` CLI shortcut
- [EXPLAIN / EXPLAIN ANALYZE](explain.md): inspect and profile a query's index/join plan
- [External Files](external-files.md): `read_csv()`/`read_json()` table functions
- [Write-Back](write-back.md): `INSERT`/`UPDATE`/`DELETE` that edit the source Markdown file
- [Example Queries](sql-examples.md): hierarchy extraction, structural lint, and mixed mq/SQL queries
- [Block Model](block-model.md): the `Block` struct and per-type properties
- [Index Layers](index-layers.md): zone maps, the interval index, and secondary indexes
- [Storage Format](storage-format.md): the on-disk page-file layout

For mq language syntax itself (selectors, control flow, pattern matching, …), see the [mq documentation](https://mqlang.org/book/).
