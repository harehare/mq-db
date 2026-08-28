# External Files (`read_csv` / `read_json`)

`read_csv(path)` and `read_json(path)` are table functions, use them anywhere a table name goes (`FROM`, `JOIN`, `CREATE TABLE ... AS SELECT`) to query an external file alongside `blocks`, no separate import step:

```sql
SELECT name, age FROM read_csv('people.csv') WHERE age > 26;
SELECT name FROM read_json('people.jsonl') WHERE active = true;

-- Persist as a custom table
CREATE TABLE people AS SELECT * FROM read_csv('people.csv');
```

`read_csv` parses RFC 4180 (quoted fields, embedded commas/quotes/newlines); the first row is the header. `read_json` expects one JSON object per line (JSON Lines): the column set is the union of every line's keys, in first-seen order. In both, numeric-looking values support arithmetic/comparison (`WHERE age > 26`), and a short/missing cell becomes `NULL` rather than erroring. Parquet is not supported (would need a large `parquet`/Arrow dependency); an unrecognized table function name (including `read_parquet`) is rejected with a clear error rather than silently misparsed.
