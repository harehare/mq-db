# INSERT / UPDATE / DELETE with Write-Back

`INSERT`/`UPDATE`/`DELETE` on `blocks` write the change back to the document's *source Markdown file* (re-parsed in place, same `DocumentId`), pass `--write-back` to allow it; without the flag the statement is rejected:

```bash
mq-db sql "UPDATE blocks SET content = 'New Title' WHERE block_type = 'heading' AND content = 'Old Title'" \
  --db store.mq-db --write-back

mq-db sql "DELETE FROM blocks WHERE content = 'Outdated paragraph'" \
  --db store.mq-db --write-back

# after_pre anchors the new block right after an existing block's `pre`;
# omit it to append at the end of the document.
mq-db sql "INSERT INTO blocks (document_id, block_type, content, depth, after_pre) VALUES (0, 'heading', 'New Section', 2, 4)" \
  --db store.mq-db --write-back

mq-db sql "INSERT INTO blocks (document_id, block_type, content) VALUES (0, 'paragraph', 'Appended at the end')" \
  --db store.mq-db --write-back
```

## Limitations in this version

- `UPDATE ... SET content` and `INSERT INTO blocks` only support `heading`/`paragraph` blocks (not tables, code, lists, ...)
- `INSERT INTO blocks` requires an explicit column list drawn from `document_id`, `block_type`, `content`, `depth` (required for `heading`, 1-6), `after_pre` (optional); `INSERT ... SELECT` is not supported, only `VALUES`
- Only documents indexed **with spans** (the default; not `--no-spans`) and from a real file (not added via the library's `add_str`) are eligible
- Not available over `serve`'s HTTP endpoint or from `mq-mcp`: CLI (`sql`/`repl` with `--write-back`) and the library (`DocumentStore::execute_sql_mut`) only
