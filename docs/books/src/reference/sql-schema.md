# Virtual Schema

The SQL engine exposes two virtual tables backed directly by the in-memory store; there is no separate schema to migrate.

```sql
SELECT id, path, title, tags FROM documents;

SELECT id, document_id, block_type, content, pre, post,
       depth, lang, properties FROM blocks;
```

## `documents`

| Column | Type | Description |
| --- | --- | --- |
| `id` | integer | Document ID (matches `blocks.document_id`) |
| `path` | text | Source file path, or `NULL` for in-memory-only documents |
| `title` | text | Front-matter / first-heading title, if any |
| `tags` | text | Front-matter tags, as a JSON array |
| `block_count` | integer | Number of blocks in the document |
| `max_heading_depth` | integer | Deepest heading level |
| `code_languages` | text | Code fence languages, as a sorted JSON array |
| `frontmatter_keys` | text | Top-level front-matter keys, as a sorted JSON array |
| `properties` | text | Top-level front-matter as a JSON object, readable without loading blocks |

```sql
SELECT path FROM documents
WHERE json_extract(properties, 'owner') = 'alice';
```

## `blocks`

| Column | Type | Description |
| --- | --- | --- |
| `id` | integer | Block ID |
| `document_id` | integer | Owning document ID |
| `block_type` | text | `'heading'`, `'paragraph'`, `'code'`, `'list'`, `'blockquote'`, `'table_cell'`, `'table_row'`, `'table_align'`, `'yaml'`, `'toml'`, `'html'`, `'horizontal_rule'`, `'math'`, `'definition'`, `'footnote'` |
| `content` | text | Raw block content |
| `pre` | integer | Interval-index pre-order boundary |
| `post` | integer | Interval-index post-order boundary |
| `depth` | integer | Heading depth (`1`–`6`); `NULL`/`0` for non-headings |
| `lang` | text | Code fence language, when `block_type = 'code'` |
| `properties` | text | Remaining block-type-specific properties as JSON |

`pre`/`post` are the Nested-Set interval-index boundaries described in [Index Layers](index-layers.md); they encode heading hierarchy as a pure integer range, which is what the [`under()`](sql-functions.md) function operates on.
