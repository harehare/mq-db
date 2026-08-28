# DDL Statements

`mq-db` supports a small set of DDL statements for defining **custom in-memory tables** alongside the built-in `documents`/`blocks` virtual tables. Custom tables live only for the process lifetime; they are not persisted to the `.mq-db` store file.

| Statement | Description |
| --- | --- |
| `CREATE TABLE name AS SELECT …` | Create a custom table from a query result |
| `CREATE TABLE name (col TYPE, …)` | Create an empty custom table with explicit schema |
| `INSERT INTO name VALUES (…)` | Insert a row into a custom table |
| `DROP TABLE name` | Drop a custom table |
| `SHOW TABLES` | List all custom tables and views |
| `DESC name` | Show the schema of a custom table or view |
| `CREATE VIEW name AS SELECT …` | Create a live (non-materialized) [view](#views) |
| `CREATE OR REPLACE VIEW name AS …` | Overwrite an existing view's definition |
| `DROP VIEW name` | Drop a view |
| `ATTACH DATABASE 'path' AS alias` | Mount another `.mq-db` store as `alias.<table>` |
| `DETACH alias` | Unmount a previously attached store |

## Examples

```bash
# Create from a SELECT result
mq-db sql "CREATE TABLE headings AS SELECT content, depth FROM blocks WHERE block_type = 'heading'" --db store.mq-db

# Create with explicit schema, then insert
mq-db sql "CREATE TABLE notes (id TEXT, body TEXT)" --db store.mq-db
mq-db sql "INSERT INTO notes VALUES ('1', 'Hello world')" --db store.mq-db

# Inspect
mq-db sql "SHOW TABLES" --db store.mq-db
mq-db sql "DESC notes"  --db store.mq-db

# Drop
mq-db sql "DROP TABLE notes" --db store.mq-db
```

Custom tables can be queried and joined exactly like `documents`/`blocks`:

```sql
SELECT h.content, n.body
FROM headings h
JOIN notes n ON n.id = h.content;
```

## ATTACH / DETACH

`ATTACH DATABASE '<path>' AS <alias>` mounts another `.mq-db` store for the session, queryable as `<alias>.blocks`, `<alias>.documents`, or any of its views/custom tables, usable in `SELECT`, `JOIN`, subqueries, and CTEs. `DETACH <alias>` unmounts it. Like SQLite, this is session-scoped only (not saved into either store's file); pass `--attach path.mq-db:alias` (repeatable) to `sql`, `repl`, or `serve` to attach automatically on startup.

```bash
mq-db repl --db project-a.mq-db --attach project-b.mq-db:b

sql> SELECT a.content, b.content FROM blocks a
     JOIN b.blocks b ON a.block_type = b.block_type
     WHERE a.block_type = 'heading';
```

Writes (`INSERT`/`UPDATE`/`DELETE`/`CREATE TABLE`) through an attached alias are rejected, only the local store can be written to.

## Views

Unlike `CREATE TABLE name AS SELECT …` (a frozen snapshot), a view is **not materialized**: its `SELECT` re-runs on every reference, so it always reflects current data, including [write-back](write-back.md) edits and re-indexing. View definitions persist to the `.mq-db` file, same as custom tables.

| Statement | Description |
| --- | --- |
| `CREATE VIEW name AS SELECT …` | Create a live (non-materialized) view |
| `CREATE OR REPLACE VIEW name AS …` | Overwrite an existing view's definition |
| `DROP VIEW name` | Drop a view |

```bash
mq-db sql "CREATE VIEW headings AS SELECT content, depth FROM blocks WHERE block_type = 'heading'" --db store.mq-db
mq-db sql "SELECT * FROM headings WHERE depth = 1" --db store.mq-db

mq-db sql "SHOW TABLES" --db store.mq-db   # views are listed with kind = "view"
mq-db sql "DESC headings" --db store.mq-db

mq-db sql "CREATE OR REPLACE VIEW headings AS SELECT content FROM blocks WHERE block_type = 'heading'" --db store.mq-db
mq-db sql "DROP VIEW headings" --db store.mq-db
```

Limitations in this version:

- `CREATE VIEW v (a, b) AS ...` (explicit column list) is not supported
- A view and a custom table can't share a name
- A view's output columns are recovered from their rendered text, same as `WITH` CTEs, see the [CTE](cte.md) page for the trade-off this implies
