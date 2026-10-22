# Virtual Schema

The SQL engine exposes three virtual tables backed directly by the in-memory store; there is no separate schema to migrate.

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

## `links`

One row per link or image, including those inside lists, tables and block quotes.

| Column | Type | Description |
| --- | --- | --- |
| `document_id` | integer | Owning document ID |
| `block_id` | integer | `blocks.id` of the block containing the link |
| `kind` | text | `'link'`, `'image'`, `'ref'` (reference link) or `'image_ref'` |
| `url` | text | Destination URL, or the reference label for `ref`/`image_ref` |
| `text` | text | Link text, or image alt text |
| `title` | text | Link title, or empty |

```sql
-- Most linked-to URLs
SELECT url, COUNT(*) AS n FROM links WHERE kind = 'link' GROUP BY url ORDER BY n DESC;
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
| `content_hash` | text | 64 hex digits of SHA-256 over the block type, a NUL byte and the content. Equal for equal content wherever it appears, so it is a stable key for derived data such as embeddings |
| `anchor` | text | Location that survives edits elsewhere in the document: a heading is its slug path (`guide/install`, a repeated sibling gets `-2`), any other block is `<section path>::<type>:<n>` (`guide/install::code:1`). Unique within a document |

`id` is the block's position across the whole store, so it shifts when an earlier document gains or loses blocks. Key long-lived data on `(documents.path, anchor)` or on `content_hash` instead.

`pre`/`post` are the Nested-Set interval-index boundaries described in [Index Layers](index-layers.md); they encode heading hierarchy as a pure integer range, which is what the [`under()`](sql-functions.md) function operates on.
