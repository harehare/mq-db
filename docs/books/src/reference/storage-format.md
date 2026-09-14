# MQDB Storage Format

## Overview

`mq-db` persists documents in a fixed-size page file. Every file is split into 8192-byte pages. Page 0 is the file header, page 1 is the catalog root, and all remaining pages hold document blocks, secondary indexes, custom-table rows, or overflow chunks.

```text
+-----------+-----------+-----------+-----------+-----------+
| Page 0    | Page 1    | Page N    | Page N+1  | Page ...  |
| FileHeader| Catalog   | BlockData | Overflow  | Free/Future|
+-----------+-----------+-----------+-----------+-----------+
```

Multi-page values are stored as singly linked page chains using the `next_page` field in the page header.

## Page Layout

Each page is exactly 8192 bytes.

### Page header (16 bytes)

| Offset | Size | Field       | Type     | Description                                                         |
| ------ | ---- | ----------- | -------- | -------------------------------------------------------------------- |
| 0      | 4    | `page_type` | `u32 LE` | `0=Free`, `1=FileHeader`, `2=Catalog`, `3=BlockData`, `4=Overflow`, `5=Index`, `6=TableData` |
| 4      | 4    | `checksum`  | `u32 LE` | Wrapping sum of all page bytes except bytes `4..8`                  |
| 8      | 4    | `page_id`   | `u32 LE` | Zero-based page index                                               |
| 12     | 4    | `next_page` | `u32 LE` | `0` means end of chain; otherwise next page index                   |

### Page body (8176 bytes)

| Offset | Size | Description           |
| ------ | ---- | ---------------------- |
| 16     | 8176 | Type-specific payload |

## File Header Page (page 0)

Page 0 always has `page_type = 1` and `page_id = 0`.

### File header body

| Offset in body | Size | Field                | Type         | Value                          |
| -------------- | ---- | --------------------- | ------------ | ------------------------------- |
| 0               | 4    | `magic`               | `u32 LE`     | `0x4D514442` (`"MQDB"`)         |
| 4               | 4    | `version`             | `u32 LE`     | `7` (readers also accept legacy `4`, `5`, `6`) |
| 8               | 4    | `page_size`           | `u32 LE`     | `8192`                          |
| 12              | 4    | `num_pages`           | `u32 LE`     | Total pages currently in file   |
| 16              | 4    | `catalog_start_page`  | `u32 LE`     | Always `1`                      |
| 20              | 8156 | `reserved`            | `[u8; 8156]` | All zero bytes                  |

## Catalog Pages

The catalog always starts at page 1. If the serialized catalog exceeds one page body, additional catalog pages are linked by `next_page`.

```text
Page 1 (Catalog) --> Page 12 (Catalog) --> Page 18 (Catalog) --> 0
```

### Catalog body format

The catalog holds document entries, custom tables, content hashes, view definitions, and the store's tokenizer, in this order:

| Section         | Fields                                                                                          |
| ---------------- | ------------------------------------------------------------------------------------------------ |
| Document entries | `num_entries: u32`, then per entry: `document_id: u32`, `path_present: u8`, `path_len: u16` + `path` (only if present), `first_block_page: u32`, `num_blocks: u32`, `zone_map_len: u32` + `zone_map` bytes, `index_start_page: u32` |
| Custom tables    | `num_tables: u32`, then per table: `name_len: u16` + `name`, `num_columns: u16`, then per column `col_len: u16` + `col`, `first_row_page: u32`, `last_row_page: u32`, `num_rows: u32` |
| Content hashes   | `num_hashes: u32`, then per entry: `document_id: u32`, `hash: u64`                                |
| Views            | `num_views: u32`, then per view: `name_len: u16` + `name`, `sql_len: u16` + `sql`                |
| Tokenizer        | `tokenizer: u8` (`0=Word`, `1=Bigram`, `2=Trigram`)                                               |

`index_start_page` is `0` when no secondary index is persisted for that document. Every trailing section (custom tables onward) is optional: a catalog blob written by an older `mq-db` version simply ends early, and readers default the missing sections to empty (tokenizer defaults to `Word`).

## Block Data Pages

A document is serialized as concatenated encoded blocks. The byte stream is cut into 8176-byte chunks.

- The first chunk is stored in a page with `page_type = 3` (`BlockData`).
- Continuation chunks are stored in pages with `page_type = 4` (`Overflow`).
- The last page in the chain has `next_page = 0`.

```text
first_block_page
      |
      v
+-------------------+    +-------------------+    +-------------------+
| type=BlockData    | -> | type=Overflow     | -> | type=Overflow     |
| body bytes 0..8175|    | next chunk        |    | final chunk       |
+-------------------+    +-------------------+    +-------------------+
```

Unused bytes at the end of the final page body are zero-filled.

## Block Wire Format

Each block is encoded independently and concatenated without separators.

| Order | Field          | Type                 | Description                    |
| ----- | -------------- | --------------------- | -------------------------------- |
| 1     | `id`           | `u32 LE`              | Block ID                        |
| 2     | `document_id`  | `u32 LE`              | Owning document ID              |
| 3     | `block_type`   | `u8`                  | See mapping below                |
| 4     | `pre`          | `u32 LE`              | Interval-index left boundary     |
| 5     | `post`         | `u32 LE`              | Interval-index right boundary    |
| 6     | `span_present` | `u8`                  | `0` absent, `1` present          |
| 7     | `start_line`   | `u32 LE`              | Present only when span exists    |
| 8     | `start_col`    | `u32 LE`              | Present only when span exists    |
| 9     | `end_line`     | `u32 LE`              | Present only when span exists    |
| 10    | `end_col`      | `u32 LE`              | Present only when span exists    |
| 11    | `content_len`  | `u32 LE`              | UTF-8 byte length                |
| 12    | `content`      | `[u8 * content_len]`  | UTF-8 bytes                      |
| 13    | `num_props`    | `u16 LE`              | Number of properties             |
| 14    | `key_len`      | `u8`                  | Repeated per property            |
| 15    | `key`          | `[u8 * key_len]`      | UTF-8 property name              |
| 16    | `value`        | `PropertyValue`       | Encoded property value           |

### Block type mapping

| `u8` | BlockType        |
| ---- | ------------------ |
| 0    | `Heading`        |
| 1    | `Paragraph`      |
| 2    | `Code`           |
| 3    | `List`           |
| 4    | `TableCell`      |
| 5    | `TableRow`       |
| 6    | `TableAlign`     |
| 7    | `Blockquote`     |
| 8    | `HorizontalRule` |
| 9    | `Html`           |
| 10   | `Yaml`           |
| 11   | `Toml`           |
| 12   | `Math`           |
| 13   | `Definition`     |
| 14   | `Footnote`       |

## PropertyValue Encoding

Each property value starts with a one-byte type tag.

| Tag    | Variant  | Payload                                |
| ------ | -------- | ---------------------------------------- |
| `0x00` | `Null`   | none                                    |
| `0x01` | `String` | `u32 LE byte_len` + UTF-8 bytes          |
| `0x02` | `Int`    | `i64 LE`                                |
| `0x03` | `Float`  | `f64 LE`                                |
| `0x04` | `Bool`   | `u8` (`0` or `1`)                       |
| `0x05` | `Array`  | `u16 LE count` + encoded child values    |

Arrays are recursive: each element is another complete `PropertyValue`.

## Index Pages

A document's secondary index (zone maps aside) is serialized separately from its blocks and stored in its own page chain, pointed to by the catalog entry's `index_start_page`.

- The first chunk is stored in a page with `page_type = 5` (`Index`).
- Continuation chunks use `page_type = 4` (`Overflow`), same as block chains.
- The encoded bytes are opaque to the page layer; only `DocumentIndex` interprets them.

## Table Data Pages

Custom-table rows (from `CREATE TABLE` / `INSERT INTO`) live in their own page chain, referenced by the catalog's `first_row_page` and `last_row_page`.

- The first page is `page_type = 6` (`TableData`); continuation pages are `page_type = 4` (`Overflow`).
- Unlike block and index chains, table chains grow incrementally, one `INSERT` at a time, so a chunk does not always fill a full page body. Each table-data/overflow page therefore reserves its first 2 bytes for a `u16 LE` chunk length, followed by that many bytes of row data.
- Row data is a flat stream of `u32 LE`-length-prefixed UTF-8 cells. Decoding needs the table's row and column counts (from the catalog) since rows are not self-delimiting.

## ZoneMap Encoding

Zone maps are encoded independently and embedded as opaque bytes inside catalog entries.

| Order | Field                    | Type                                     |
| ----- | ------------------------- | ------------------------------------------ |
| 1     | `max_heading_depth`      | `u8`                                     |
| 2     | `num_heading_slugs`      | `u16 LE`                                 |
| 3     | `heading_slug` items     | `u16 LE len` + UTF-8 bytes               |
| 4     | `num_heading_contents`   | `u16 LE`                                 |
| 5     | `heading_content` items  | `u16 LE len` + UTF-8 bytes               |
| 6     | `num_code_langs`         | `u16 LE`                                 |
| 7     | `code_lang` items        | `u16 LE len` + UTF-8 bytes               |
| 8     | `num_frontmatter_keys`   | `u16 LE`                                 |
| 9     | `frontmatter_key` items  | `u16 LE len` + UTF-8 bytes               |
| 10    | `title_present`          | `u8`                                     |
| 11    | `title`                  | `u16 LE len` + UTF-8 bytes when present  |
| 12    | `num_tags`               | `u16 LE`                                 |
| 13    | `tag` items              | `u16 LE len` + UTF-8 bytes               |

Sets are serialized as sorted UTF-8 strings for deterministic output.

## Checksum Algorithm

The checksum is a wrapping sum over every byte in the 8192-byte page, except the checksum field itself (`page[4..8]`).

Pseudo-code:

```text
checksum = 0u32
for i in 0..8192:
    if i in [4, 5, 6, 7]:
        continue
    checksum = checksum.wrapping_add(page[i] as u32)
```

Verification recomputes the checksum and compares it to the stored `checksum` field.

## Multi-Page Chains

Large catalog payloads, document block streams, index blobs, and table-row batches are all stored as chains.

```text
+---------+      +---------+      +---------+
| page_id |----->| page_id |----->| page_id |
| next=42 |      | next=77 |      | next=0  |
+---------+      +---------+      +---------+
```

The body bytes of each page are concatenated in chain order to reconstruct the original serialized byte stream.

## Atomic Write Procedure

`DocumentStore::save()` writes atomically using a sibling temporary file:

1. Create `path.tmp` (a fresh page file with only the file header and an empty catalog page).
2. Write every document's blocks as a block-data chain.
3. Write every document's secondary index as an index chain.
4. Write every custom table's rows as a table-data chain.
5. Serialize and write the final catalog chain, referencing all pages written above.
6. Rename `path.tmp` to `path`.

Because the final rename is atomic on the same filesystem, readers either observe the old file or the new complete file, never a partially written database image.
