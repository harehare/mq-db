# TUI

`mq-db tui` opens a full-screen terminal UI for browsing indexed documents and running SQL/mq queries side by side, built with [ratatui](https://ratatui.rs).

```bash
mq-db tui --db store.mq-db
```

```text
 mq-db  SQL  Tab:switch  i:input  j/k:nav  d/u:scroll  q:quit
┌─ Documents ──────────┬─ SQL ────────────────────────────────────────────────┐
│ DESIGN.md            │ SELECT block_type, count(*) FROM blocks GROUP BY b_  │
│   142 blocks         ├─ Results ────────────────────────────────────────────┤
│ API.md               │ ┌─────────────┬──────────┐                           │
│   87 blocks  API     │ │ block_type  │ count(*) │                           │
│ README.md            │ ├─────────────┼──────────┤                           │
│   34 blocks          │ │ paragraph   │ 48       │                           │
└──────────────────────┴──────────────────────────────────────────────────────┘
 5 docs  632 blocks  3 rows
```

The left pane lists indexed documents; selecting one shows its full block breakdown (type, `pre`/`post`, content preview) in the results pane. The top-right pane accepts a query in the current mode (`mq` or `SQL`); running it replaces the results pane with the query output.

## Keys

| Key | Action |
| --- | --- |
| `i` | Focus query input |
| `Esc` | Blur input |
| `Enter` | Run query |
| `Tab` | Toggle mq / SQL mode |
| `j` / `k` (or `↓` / `↑`) | Navigate document list |
| `d` / `u` (or PageDown / PageUp) | Scroll results down / up |
| `g` / `G` | Jump results to top / bottom |
| `q` / `Ctrl+C` | Quit |

## Color scheme

Block types are color-coded for quick scanning (heading, paragraph, code, list, blockquote, table, frontmatter, html, math, …), using the same warm paper/ink/accent palette as the [project site](https://harehare.github.io/mq-db/).
