# HTTP Server

`mq-db serve` starts an HTTP server (built on [axum](https://github.com/tokio-rs/axum)) exposing SQL and mq query endpoints over the indexed store.

```bash
mq-db serve --db store.mq-db              # listens on 127.0.0.1:7878
mq-db serve --db store.mq-db --port 8080  # custom port
mq-db serve --db store.mq-db --host 0.0.0.0 --port 8080
```

## Endpoints

| Method | Path | Body | Description |
| --- | --- | --- | --- |
| `GET` | `/health` | — | `{"status":"ok","documents":<n>}` |
| `POST` | `/sql` | `{"query":"SELECT …"}` | Execute a SQL query, returns JSON rows |
| `POST` | `/mq` | `{"code":".h1"}` | Evaluate an mq expression, returns `{"results":[…]}` |

## Examples

```bash
# Health check
curl http://127.0.0.1:7878/health

# SQL via HTTP
curl -s -X POST http://127.0.0.1:7878/sql \
  -H 'Content-Type: application/json' \
  -d '{"query":"SELECT block_type, count(*) FROM blocks GROUP BY block_type"}'

# mq via HTTP
curl -s -X POST http://127.0.0.1:7878/mq \
  -H 'Content-Type: application/json' \
  -d '{"code":".h1"}'
```
