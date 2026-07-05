# Block Model

Every Markdown element becomes a `Block`:

```rust
struct Block {
    id: u32,
    document_id: u32,
    block_type: BlockType,  // Heading, Paragraph, Code, List, …
    content: String,
    span: Option<Span>,     // line/column for editor sync
    pre: u32,               // interval index pre-order
    post: u32,              // interval index post-order
    properties: Properties, // row-polymorphic extra attributes
}
```

`properties` is row-polymorphic: different `block_type`s carry different keys.

| Block type | Properties |
| --- | --- |
| `Heading` | `{ "depth": 2, "slug": "architecture" }` |
| `Code` | `{ "lang": "rust", "meta": "no_run" }` |
| `List` | `{ "ordered": false, "level": 1, "checked": null }` |
| `Yaml` / `Toml` | parsed front-matter keys (`"title"`, `"tags"`, …) |

## Block types

`BlockType` covers every CST node `mq-markdown` produces:

`Heading`, `Paragraph`, `Code`, `List`, `TableCell`, `TableRow`, `TableAlign`, `Blockquote`, `HorizontalRule`, `Html`, `Yaml`, `Toml`, `Math`, `Definition`, `Footnote`.

In SQL, `block_type` is exposed as the lowercase, snake_case string form (e.g. `'table_cell'`, `'horizontal_rule'`).

See [Storage Format](storage-format.md) for the exact on-disk wire encoding of a `Block` and its `properties`.
