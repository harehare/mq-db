# CLI

Every subcommand operates on a `.mq-db` store file (`--db` / `-d`, default `store.mq-db`). Output-producing commands accept `--format` / `-F`: `table` (default), `json`, `csv`, `tsv`, `markdown`, `html`.

```bash
mq-db --help
mq-db <command> --help
```

## `index`

Index Markdown files or directories into a store file.

```bash
mq-db index docs/ --recursive --output store.mq-db
mq-db index README.md DESIGN.md
mq-db index docs/ --no-spans   # omit source spans (~21 bytes/block saved)
```

| Flag | Description |
| --- | --- |
| `paths` | Markdown files or directories to index (required) |
| `-o, --output <PATH>` | Output store file (default `store.mq-db`) |
| `-r, --recursive` | Recursively walk directories |
| `--no-spans` | Do not store source line/column spans |

```text
  ✓ docs/DESIGN.md
  ✓ docs/API.md

Indexed 2 files → store.mq-db
```

## `list`

List all indexed documents.

```bash
mq-db list --db store.mq-db
mq-db list --db store.mq-db --format json
```

```text
┌──────┬────────────────────────────────────────────────────┬────────┬──────────┐
│   ID │ Path / Title                                       │ Blocks │ Tags     │
├──────┼────────────────────────────────────────────────────┼────────┼──────────┤
│    0 │ docs/DESIGN.md                                     │    142 │          │
│    1 │ docs/API.md                                        │     87 │ api, v2  │
└──────┴────────────────────────────────────────────────────┴────────┴──────────┘
2 documents
```

## `sql`

Run a SQL query over the store. See the [Reference](../reference/index.md) for the virtual schema and function library.

```bash
mq-db sql "SELECT block_type, count(*) FROM blocks GROUP BY block_type" --db store.mq-db
mq-db sql --file query.sql --db store.mq-db
mq-db sql "SELECT ..." --db store.mq-db --format json
```

| Flag | Description |
| --- | --- |
| `query` | SQL query string (omit when using `--file`) |
| `-f, --file <PATH>` | Read SQL from a file |

## `mq`

Run an [mq](https://github.com/harehare/mq) query over the store.

```bash
mq-db mq ".h1" --db store.mq-db
mq-db mq 'select(.code_lang == "rust")' --db store.mq-db
mq-db mq ".h1" --db store.mq-db --format markdown
```

## `repl`

Interactive REPL supporting both query modes; switch with `.mode`.

```bash
mq-db repl --db store.mq-db --mode sql
```

See [REPL](repl.md) for the full command list.

## `lint`

Run structural lint checks (currently: a heading at the given depth immediately followed by a list).

```bash
mq-db lint --db store.mq-db --depth 2
```

```text
✗  1 violation  (H2 immediately followed by list)

  file                                      heading
  ────────────────────────────────────────  ──────────────────────────────
  docs/DESIGN.md                            "Quick Start"
```

## `stats`

Show store-wide statistics: document/block counts, block-type distribution, code-language distribution.

```bash
mq-db stats --db store.mq-db
```

```text
  Documents  5
  Blocks     632

  Block types
  ────────────────────────────────────────────────────────
   ¶  paragraph    ████████████████████░░░░   241  (38%)
   #  heading      ████████░░░░░░░░░░░░░░░░    89  (14%)
  {}  code         ███████░░░░░░░░░░░░░░░░░    73  (12%)
   •  list         ██████░░░░░░░░░░░░░░░░░░    58   (9%)
```

## `show`

Show the full block structure of one document by ID (see `list` for IDs).

```bash
mq-db show 0 --db store.mq-db
```

```text
  docs/DESIGN.md
  title   Design Document
  blocks  142

  pre   post  type               content
  ────  ────  ────────────────   ──────────────────────────────────────────
     0   141  heading H1         Design Document
     2    55  heading H2         Architecture
     4    21  paragraph          The system is built on…
```

## `tui`

Launch the interactive TUI. See [TUI](tui.md).

```bash
mq-db tui --db store.mq-db
```

## `serve`

Start an HTTP server exposing SQL/mq query endpoints. See [HTTP Server](http-server.md).

```bash
mq-db serve --db store.mq-db --host 0.0.0.0 --port 8080
```
