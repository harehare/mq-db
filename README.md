<div align="center">
  <img src="assets/logo.svg" width="96" height="96" alt="mq-db logo" />

<a href="https://db.mqlang.org">Visit the site 🌐</a>
—
<a href="https://db.mqlang.org/book/">Read the book 📖</a>

  <h1>mq-db</h1>

[![ci](https://img.shields.io/github/actions/workflow/status/harehare/mq-db/ci.yml?style=flat-square&logo=github-actions&label=ci)](https://github.com/harehare/mq-db/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/mq-db?style=flat-square&logo=rust)](https://crates.io/crates/mq-db)
[![license](https://img.shields.io/badge/license-MIT-blue.svg?style=flat-square)](LICENSE)

</div>

`mq-db` is an embedded database that treats Markdown documents as structured, hierarchical data.
It parses Markdown into a flat block list with an interval index (pre/post order), so section hierarchy queries are a single integer comparison. Documents can be queried with **SQL** or **[mq](https://github.com/harehare/mq)** and persisted to a compact page-file format.

![demo](./assets/demo.gif)

> [!WARNING]
> `mq-db` is currently under active development and the API may change.

## Features

- Query Markdown with [SQL](https://db.mqlang.org/book/reference/sql-schema) or mq, and hierarchy queries with `under()`
- [Full-text search](https://db.mqlang.org/book/reference/full-text-search) (`match()` / `bm25()`) and [vector search](https://db.mqlang.org/book/reference/vector-search)
- [CTEs](https://db.mqlang.org/book/reference/cte), [`EXPLAIN`](https://db.mqlang.org/book/reference/explain), [custom tables, views and `ATTACH`](https://db.mqlang.org/book/reference/sql-ddl)
- [`read_csv()` / `read_json()`](https://db.mqlang.org/book/reference/external-files) to query external files in `FROM`
- [`INSERT` / `UPDATE` / `DELETE` with write-back](https://db.mqlang.org/book/reference/write-back) to the source Markdown
- Incremental re-indexing, zone maps and secondary indexes for fast scans
- [CLI](https://db.mqlang.org/book/start/cli), [REPL](https://db.mqlang.org/book/start/repl), [TUI](https://db.mqlang.org/book/start/tui), [HTTP server](https://db.mqlang.org/book/start/http-server) and [Rust library](https://db.mqlang.org/book/start/library)

## Installation

```bash
curl -fsSL https://raw.githubusercontent.com/harehare/mq-db/main/bin/install.sh | bash
```

or with Cargo:

```sh
cargo install mq-db
```

## Usage

```bash
mq-db index docs/ --recursive --output store.mq-db   # incremental; --prune drops deleted files
mq-db list --db store.mq-db                          # list indexed documents
mq-db find "error handling" --db store.mq-db         # full-text search
mq-db sql "SELECT block_type, count(*) FROM blocks GROUP BY block_type" --db store.mq-db
mq-db mq 'select(.code_lang == "rust")' --db store.mq-db
mq-db repl --db store.mq-db --mode sql               # interactive REPL
mq-db tui --db store.mq-db                           # terminal UI
mq-db serve --db store.mq-db                         # HTTP server on 127.0.0.1:7878
```

Find everything inside a section with `under()`:

```sql
SELECT b.block_type, b.content
FROM blocks b
WHERE under(b.pre, b.post,
  (SELECT pre  FROM blocks WHERE block_type = 'heading' AND content = 'Architecture'),
  (SELECT post FROM blocks WHERE block_type = 'heading' AND content = 'Architecture'))
ORDER BY b.pre;
```

Use it as a library:

```rust
use mq_db::{DocumentStore, SqlEngine};

let mut store = DocumentStore::new();
store.add_file("docs/DESIGN.md")?;

let engine = SqlEngine::new(&store)?;
let out = engine.execute("SELECT content FROM blocks WHERE block_type = 'heading' ORDER BY pre")?;
print!("{}", out.to_table());
```

## Documentation

- [Install](https://db.mqlang.org/book/start/install) and [CLI](https://db.mqlang.org/book/start/cli) (`index`, `find`, `lint`, `stats`, `vacuum`, …)
- [Library API](https://db.mqlang.org/book/start/library), [HTTP server](https://db.mqlang.org/book/start/http-server)
- [SQL schema](https://db.mqlang.org/book/reference/sql-schema), [functions](https://db.mqlang.org/book/reference/sql-functions) and [examples](https://db.mqlang.org/book/reference/sql-examples)
- [Block model](https://db.mqlang.org/book/reference/block-model), [index layers](https://db.mqlang.org/book/reference/index-layers) and [storage format](https://db.mqlang.org/book/reference/storage-format)

## Support

- 🐛 [Report bugs](https://github.com/harehare/mq-db/issues/new)
- 💡 [Request features](https://github.com/harehare/mq-db/issues/new)
- ⭐ [Star the project](https://github.com/harehare/mq-db) if you find it useful!

## Contributing

Contributions are welcome! Feel free to open an issue or submit a pull request.

## License

[MIT](LICENSE)
