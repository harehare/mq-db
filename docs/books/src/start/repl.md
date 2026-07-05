# REPL

The interactive REPL supports both query modes in a single session.

```bash
mq-db repl --db store.mq-db --mode sql
```

```text
mq-db  (.help for commands  .quit to exit)
mode: sql  (.mode mq | .mode sql)

sql> SELECT content FROM blocks WHERE block_type = 'heading' LIMIT 3;
┌──────────────────┐
│ content          │
├──────────────────┤
│ Overview         │
│ Architecture     │
│ Query Engine     │
└──────────────────┘
(3 rows)

sql> .mode mq
→ mq mode
mq> .h2
## Architecture
## Query Engine
```

## Dot commands

| Command | Description |
| --- | --- |
| `.help` | List available commands |
| `.mode sql` | Switch to SQL query mode |
| `.mode mq` | Switch to mq query mode |
| `.quit` | Exit the REPL |

The initial mode can be set with `--mode sql` or `--mode mq` (default `sql`).
