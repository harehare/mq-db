---
name: querying-markdown-db
description: Queries and searches a collection of Markdown documents with mq-db (SQL, full-text and semantic search). Use when the user wants to search across many Markdown files, query document structure with SQL, or fetch the contents of a section by heading. For transforming a single file, use mq instead.
---

# Querying Markdown with mq-db

mq-db indexes Markdown files into a `.mq-db` store, then queries them with SQL, mq, or full-text/semantic search. Every block (heading, paragraph, code, list item, table cell) is one row.

## Workflow

```bash
mq-db index docs/ --recursive --output store.mq-db   # incremental; --prune drops deleted files
mq-db list  --db store.mq-db                         # indexed documents
mq-db find  "error handling" --db store.mq-db -n 5 -F json
mq-db sql   "SELECT ..." --db store.mq-db -F json
mq-db show  <DOC_ID> --db store.mq-db                # all blocks of one document
```

- Re-run `index` after the files change; unchanged files are skipped.
- Use `-F json` when you will read the output yourself.
- Check `mq-db <command> --help` for all flags.

## Choosing a command

| Goal | Command |
| --- | --- |
| Find where a topic is mentioned | `find "<terms>"` |
| Find by meaning, not keywords | `find --semantic "<question>"` (needs `mq-db embed` first) |
| Fetch a section's content | `sql` with `under()` |
| Filter or aggregate (headings, code languages, links, front matter) | `sql` |
| Apply an mq expression to blocks | `mq '<code>'` |

## Schema

`documents(id, path, title, tags, block_count, code_languages, properties, ...)`
`blocks(id, document_id, block_type, content, pre, post, depth, lang, properties, anchor, ...)`
`links(document_id, block_id, kind, url, text, title)`

`block_type` is one of `heading`, `paragraph`, `code`, `list`, `blockquote`, `table_cell`, `table_row`, `yaml`, `toml`, `html`, `math`, `definition`, `footnote`. `depth` is the heading level and `lang` the code fence language. `blocks.id` shifts when earlier documents change, so key long-lived data on `(documents.path, anchor)`.

## Fetching a section with `under()`

`under(pre, post, anc_pre, anc_post)` is true for blocks nested under a heading. `pre`/`post` are numbered **per document**, so always filter on `document_id` too.

```sql
SELECT b.block_type, b.content
FROM blocks b
JOIN blocks h ON h.document_id = b.document_id
WHERE h.block_type = 'heading' AND h.content = 'Architecture'
  AND under(b.pre, b.post, h.pre, h.post)
ORDER BY b.pre;
```

## Search

```sql
SELECT d.path, b.content, bm25(b.content, 'error handling') AS score
FROM blocks b JOIN documents d ON d.id = b.document_id
WHERE match(b.content, 'error handling')
ORDER BY score DESC LIMIT 10;
```

`match()` requires every term to appear. Prefer `bm25()` over `score()` for ranking. The tokenizer is fixed when the store is created (`bigram` is chosen automatically for CJK text).

## Common queries

```sql
SELECT path FROM documents WHERE json_extract(properties, 'owner') = 'alice';   -- front matter
SELECT lang, count(*) FROM blocks WHERE block_type = 'code' GROUP BY lang;      -- code languages
SELECT url, count(*) n FROM links WHERE kind = 'link' GROUP BY url ORDER BY n DESC;
```

## Write-back modifies the source files

`INSERT`/`UPDATE`/`DELETE` on `blocks` are rejected unless `--write-back` is passed, in which case they **rewrite the original Markdown files**.

- Run the equivalent `SELECT` first and show the user which blocks match.
- Pass `--write-back` only after the user confirms.
- Statements on custom tables only touch the `.mq-db` file and do not need the flag.

## When NOT to use mq-db

- A single file or a one-off extraction: use `mq` directly.
- Non-Markdown content.
