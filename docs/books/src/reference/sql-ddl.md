# DDL Statements

`mq-db` supports a small set of DDL statements for defining **custom in-memory tables** alongside the built-in `documents`/`blocks` virtual tables. Custom tables live only for the process lifetime — they are not persisted to the `.mq-db` store file.

| Statement | Description |
| --- | --- |
| `CREATE TABLE name AS SELECT …` | Create a custom table from a query result |
| `CREATE TABLE name (col TYPE, …)` | Create an empty custom table with explicit schema |
| `INSERT INTO name VALUES (…)` | Insert a row into a custom table |
| `DROP TABLE name` | Drop a custom table |
| `SHOW TABLES` | List all custom tables |
| `DESC name` | Show the schema of a custom table |

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
