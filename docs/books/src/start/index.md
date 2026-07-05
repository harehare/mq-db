# Getting Started

This section walks through installing `mq-db`, indexing Markdown into a store file, and querying it from the CLI, REPL, TUI, HTTP server, or the Rust library directly.

The typical workflow is:

1. **Index** one or more Markdown files into a `.mq-db` store file ([Install](install.md), [CLI](cli.md#index)).
2. **Query** the store with SQL or mq, either one-shot from the CLI, interactively in the [REPL](repl.md) / [TUI](tui.md), or over HTTP via [`mq-db serve`](http-server.md).
3. Optionally, embed `mq-db` directly with the [Library API](library.md) instead of shelling out to the CLI.

```bash
mq-db index docs/ --recursive --output store.mq-db
mq-db sql "SELECT block_type, count(*) FROM blocks GROUP BY block_type" --db store.mq-db
```
