# INSERT / UPDATE / DELETE with Write-Back

Statements on [custom tables](sql-ddl.md) change only the `.mq-db` file and never need the flag. The rest of this page is about `blocks`.

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

mq-db sql "INSERT INTO blocks (document_id, block_type, content, lang) VALUES (0, 'code', 'let x = 1;', 'rust')" \
  --db store.mq-db --write-back
```

## Limitations in this version

- `UPDATE ... SET content` and `INSERT INTO blocks` support `heading`, `paragraph`, `code`, `blockquote`, `math`, `html` and `list` blocks. `UPDATE` also supports `table_cell`; tables themselves cannot be inserted or deleted
- `UPDATE` on a `code` block keeps its language, info string and fence style; a longer fence is used if the new content contains backticks
- `INSERT INTO blocks` requires an explicit column list drawn from `document_id`, `block_type`, `content`, `depth` (required for `heading`, 1-6), `lang` (`code` only), `after_pre` (optional); `INSERT ... SELECT` is not supported, only `VALUES`
- Only documents indexed **with spans** (the default; not `--no-spans`) and from a real file (not added via the library's `add_str`) are eligible
- Not available over `serve`'s HTTP endpoint or from `mq-mcp`: CLI (`sql`/`repl` with `--write-back`) and the library (`DocumentStore::execute_sql_mut`) only

## Lists and tables

A list item is one block per item, a table is one `table_cell` block per cell, so these are edited in place rather than as whole blocks.

```sql
-- Change an item's text; its indent, marker (`-`, `2.`) and task checkbox stay
UPDATE blocks SET content = 'Ship it' WHERE block_type = 'list' AND content = 'Draft'

-- Add a sibling after an item (and after that item's nested items)
INSERT INTO blocks (document_id, block_type, content, after_pre)
VALUES (0, 'list', 'New item', 12)

-- Change one table cell; every other character of the row is kept
UPDATE blocks SET content = '42'
WHERE block_type = 'table_cell' AND content = '41'
```

- **List items** are a single line: `content` cannot contain a line break. The text you write is Markdown source, so `` `code` `` and `**bold**` work, while the `content` column you read back has the markup removed. `UPDATE` on an item that wraps onto several lines replaces all of it with the one line.
- **`INSERT` of a list item** copies the indent and marker of the item it follows (without a task checkbox) and adds no blank lines, so it joins that list. It goes after the item's whole subtree. With no `after_pre`, or after something that is not a list item, it starts a new `- ` list set off by a blank line. Ordered items keep the anchor's number; renderers renumber them.
- **`DELETE` of a list item** removes just its lines. An item that has nested items is refused unless the same statement deletes those too, so nested items are never left behind.
- **Table cells**: `UPDATE` replaces one cell, escaping `|` for you; the new content cannot contain a line break. The row's other cells keep their exact spacing, so a table that was column-aligned may no longer be. `DELETE` of a cell, row or alignment row, and `INSERT` of a cell, are refused: change a cell's content instead.
- Removing a block never joins the text around it: blank lines are only collapsed where two would end up side by side.
